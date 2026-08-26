//! SQLite backend — split out of the former monolithic `store.rs`.
//!
//! `SqliteStore` and the shared row/parse helpers live in `super`
//! (`store/mod.rs`); each submodule here holds one trait impl (or a
//! coherent group of inherent methods) on that type.

use super::*;

impl SqliteStore {
    /// Would inserting an edge `source → target` close a cycle in the
    /// concept graph? BFS from `target` along outgoing edges; if we
    /// reach `source`, the new edge would form a cycle.
    ///
    /// Returns `Ok(false)` for the empty-graph case and bounds the
    /// search to a depth-limit equal to twice the link count to avoid
    /// pathological loops on already-corrupt graphs.
    pub(super) fn would_create_cycle(&self, source: &str, target: &str) -> IcmResult<bool> {
        if source == target {
            return Ok(true);
        }
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(target.to_string());
        visited.insert(target.to_string());
        // Soft cap on traversal — guards against malformed pre-existing
        // cycles (which shouldn't exist, but the user could have edited
        // the DB by hand).
        let cap = 10_000;
        let mut steps = 0;
        while let Some(current) = queue.pop_front() {
            steps += 1;
            if steps > cap {
                tracing::warn!(
                    "would_create_cycle: BFS hit {cap}-step cap while checking {source} → {target}"
                );
                break;
            }
            for next_link in self.get_links_from(&current)? {
                if next_link.target_id == source {
                    return Ok(true);
                }
                if visited.insert(next_link.target_id.clone()) {
                    queue.push_back(next_link.target_id);
                }
            }
        }
        Ok(false)
    }

    /// Graph expansion: given a list of `(Memory, score)` results from a
    /// primary search, follow each memory's `related_ids` one hop and fetch
    /// the neighbors that are not already in the result set.
    ///
    /// Each neighbor is scored as `parent_score * hop_discount` (default
    /// 0.5) so it ranks below its direct-match parent but above unrelated
    /// low-score results. Returns the combined, deduped, score-descending
    /// list capped at `max_total` (pass `usize::MAX` for no cap).
    ///
    /// This is the core of the graph-aware recall feature: it lets the
    /// recall path surface memories that are semantically or causally
    /// linked to the query's direct matches, even if they don't match the
    /// query text themselves.
    pub fn expand_with_neighbors(
        &self,
        initial: &[(Memory, f32)],
        max_neighbors: usize,
        hop_discount: f32,
        max_total: usize,
    ) -> IcmResult<Vec<(Memory, f32)>> {
        if max_neighbors == 0 || initial.is_empty() {
            let mut out = initial.to_vec();
            out.truncate(max_total);
            return Ok(out);
        }

        let initial_ids: HashSet<String> = initial.iter().map(|(m, _)| m.id.clone()).collect();

        // Phase 1 — collect candidate neighbour ids in score-priority order
        // up to max_neighbors. We track parent scores here so we can apply
        // the hop discount once memories come back from the batched fetch.
        let mut candidates: Vec<(String, f32)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        'outer: for (mem, score) in initial {
            for neighbor_id in &mem.related_ids {
                if candidates.len() >= max_neighbors {
                    break 'outer;
                }
                if initial_ids.contains(neighbor_id) || !seen.insert(neighbor_id.clone()) {
                    continue;
                }
                candidates.push((neighbor_id.clone(), *score));
            }
        }

        // Phase 2 — single batched SELECT instead of N round-trips.
        let mut neighbors: Vec<(Memory, f32)> = Vec::new();
        if !candidates.is_empty() {
            let ids: Vec<&str> = candidates.iter().map(|(id, _)| id.as_str()).collect();
            let fetched = self.get_many(&ids)?;
            for (id, parent_score) in candidates {
                if let Some(m) = fetched.get(&id) {
                    neighbors.push((m.clone(), parent_score * hop_discount));
                }
            }
        }

        // Merge initial + neighbors, then sort descending by score.
        let mut combined: Vec<(Memory, f32)> = initial.to_vec();
        combined.extend(neighbors);
        combined.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        combined.truncate(max_total);
        Ok(combined)
    }

    /// Fetch many memories by id in one round-trip, deduplicated by id.
    ///
    /// Cache-aware: cached entries are served from memory, misses are
    /// batched into a single `IN (?,?,…)` query. Missing ids are
    /// silently dropped — callers expecting a strict mapping should
    /// diff their input vs the returned map.
    pub fn get_many(&self, ids: &[&str]) -> IcmResult<HashMap<String, Memory>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        // Dedup so the IN clause gets at most one slot per id.
        let mut unique: Vec<&str> = Vec::with_capacity(ids.len());
        let mut seen: HashSet<&str> = HashSet::new();
        for id in ids {
            if seen.insert(*id) {
                unique.push(*id);
            }
        }

        // Phase 1 — pull cache hits aside, leaving only true misses for SQL.
        let mut out: HashMap<String, Memory> = HashMap::with_capacity(unique.len());
        let mut misses: Vec<&str> = Vec::with_capacity(unique.len());
        if let Ok(mut cache) = self.cache.lock() {
            for id in &unique {
                if let Some(m) = cache.get(*id) {
                    out.insert((*id).to_string(), m.clone());
                } else {
                    misses.push(*id);
                }
            }
        } else {
            misses.extend_from_slice(&unique);
        }

        if misses.is_empty() {
            return Ok(out);
        }

        // Phase 2 — single batched SELECT for the misses.
        let placeholders: Vec<String> = (1..=misses.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT {SELECT_COLS} FROM memories WHERE id IN ({})",
            placeholders.join(", ")
        );

        let mut stmt = self.conn.prepare(&sql).map_err(db_err)?;
        let params_vec: Vec<&dyn rusqlite::types::ToSql> = misses
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();

        let rows = stmt
            .query_map(params_vec.as_slice(), row_to_memory)
            .map_err(db_err)?;

        let mut fetched: Vec<Memory> = Vec::new();
        for row in rows {
            fetched.push(row.map_err(db_err)?);
        }

        if let Ok(mut cache) = self.cache.lock() {
            for m in &fetched {
                cache.put(m.id.clone(), m.clone());
            }
        }
        for m in fetched {
            out.insert(m.id.clone(), m);
        }
        Ok(out)
    }

    /// Get memories by topic prefix (e.g., "wshm" matches "wshm:owner/repo").
    ///
    /// If `topic` ends with `*`, uses LIKE matching. Otherwise exact match.
    pub fn get_by_topic_prefix(&self, topic: &str) -> IcmResult<Vec<Memory>> {
        if let Some(prefix) = topic.strip_suffix('*') {
            let pattern = format!("{prefix}%");
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT {SELECT_COLS} FROM memories WHERE topic LIKE ?1 ORDER BY weight DESC LIMIT 500"
                ))
                .map_err(db_err)?;

            let rows = stmt
                .query_map(params![pattern], row_to_memory)
                .map_err(db_err)?;

            collect_rows(rows)
        } else {
            self.get_by_topic(topic)
        }
    }

    /// List topics, optionally filtered by a prefix.
    pub fn list_topics_with_prefix(&self, prefix: Option<&str>) -> IcmResult<Vec<(String, usize)>> {
        match prefix {
            Some(p) => {
                let pattern = format!("{p}%");
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT topic, COUNT(*) FROM memories WHERE topic LIKE ?1 GROUP BY topic ORDER BY topic",
                    )
                    .map_err(db_err)?;

                let rows = stmt
                    .query_map(params![pattern], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
                    })
                    .map_err(db_err)?;

                let mut results = Vec::new();
                for row in rows {
                    results.push(row.map_err(db_err)?);
                }
                Ok(results)
            }
            None => self.list_topics(),
        }
    }

    /// Detect recurring patterns in a topic by computing Jaccard similarity on keywords.
    ///
    /// Groups memories with keyword similarity > 0.5 into clusters,
    /// and returns clusters of size >= `min_cluster_size`.
    pub fn detect_patterns(
        &self,
        topic: &str,
        min_cluster_size: usize,
    ) -> IcmResult<Vec<PatternCluster>> {
        let memories = self.get_by_topic(topic)?;
        if memories.len() < min_cluster_size {
            return Ok(Vec::new());
        }

        // Build keyword sets for each memory
        let keyword_sets: Vec<HashSet<String>> = memories
            .iter()
            .map(|m| m.keywords.iter().map(|k| k.to_lowercase()).collect())
            .collect();

        // Union-Find-style clustering via adjacency
        let n = memories.len();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], i: usize) -> usize {
            let mut i = i;
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }

        fn union(parent: &mut [usize], a: usize, b: usize) {
            let ra = find(parent, a);
            let rb = find(parent, b);
            if ra != rb {
                parent[ra] = rb;
            }
        }

        // Compute Jaccard similarity for each pair, union if > 0.5
        for i in 0..n {
            for j in (i + 1)..n {
                if keyword_sets[i].is_empty() && keyword_sets[j].is_empty() {
                    continue;
                }
                let intersection = keyword_sets[i].intersection(&keyword_sets[j]).count();
                let union_size = keyword_sets[i].union(&keyword_sets[j]).count();
                if union_size > 0 {
                    let jaccard = intersection as f32 / union_size as f32;
                    if jaccard > 0.5 {
                        union(&mut parent, i, j);
                    }
                }
            }
        }

        // Group by cluster root
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            clusters.entry(root).or_default().push(i);
        }

        // Build PatternCluster for each group meeting the minimum size
        let mut result: Vec<PatternCluster> = Vec::new();
        for indices in clusters.values() {
            if indices.len() < min_cluster_size {
                continue;
            }

            // Representative = the highest-weight memory in the cluster
            let best_idx = match indices.iter().max_by(|&&a, &&b| {
                memories[a]
                    .weight
                    .partial_cmp(&memories[b].weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }) {
                Some(&idx) => idx,
                None => continue, // empty cluster, skip
            };

            // Collect all unique keywords from the cluster
            let mut all_kw: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for &idx in indices {
                for kw in &memories[idx].keywords {
                    let lower = kw.to_lowercase();
                    if seen.insert(lower) {
                        all_kw.push(kw.clone());
                    }
                }
            }

            result.push(PatternCluster {
                representative_summary: memories[best_idx].summary.clone(),
                memory_ids: indices.iter().map(|&i| memories[i].id.clone()).collect(),
                keywords: all_kw,
                count: indices.len(),
            });
        }

        // Sort by cluster size descending
        result.sort_by_key(|b| std::cmp::Reverse(b.count));

        Ok(result)
    }

    /// Extract a pattern cluster as a concept in a memoir.
    ///
    /// Creates a Concept with:
    /// - name derived from common keywords
    /// - definition = combined summary of the cluster
    /// - source_memory_ids = memory IDs in the cluster
    /// - confidence = 0.5 + (count * 0.05) capped at 0.9
    /// - labels = common keywords as labels
    pub fn extract_pattern_as_concept(
        &self,
        cluster: &PatternCluster,
        memoir_id: &str,
    ) -> IcmResult<String> {
        // Derive concept name from top keywords
        let concept_name = if cluster.keywords.is_empty() {
            format!("pattern-{}", &cluster.memory_ids[0][..8])
        } else {
            cluster
                .keywords
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("-")
        };

        // Build definition from cluster representative + count
        let definition = format!(
            "{} (pattern detected across {} memories)",
            cluster.representative_summary, cluster.count
        );

        let mut concept = Concept::new(memoir_id.into(), concept_name, definition);
        concept.source_memory_ids = cluster.memory_ids.clone();
        concept.confidence = (0.5 + cluster.count as f32 * 0.05).min(0.9);
        concept.labels = cluster
            .keywords
            .iter()
            .take(5)
            .map(|kw| Label::new("pattern", kw.as_str()))
            .collect();

        self.add_concept(concept)
    }
}
