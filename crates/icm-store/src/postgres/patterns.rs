//! PostgreSQL backend -- split out of the former monolithic postgres.rs.
//!
//! Neighbor expansion and pattern-mining helpers.

use super::*;

impl PostgresStore {
    // Memory reads used by recall expansion

    /// Fetch many memories by id in one round-trip, deduplicated by id.
    pub fn get_many(&self, ids: &[&str]) -> IcmResult<HashMap<String, Memory>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let id_vec: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        let mut c = self.conn()?;
        let rows = c
            .query(
                &format!("SELECT {SELECT_COLS} FROM memories WHERE id = ANY($1)"),
                &[&id_vec],
            )
            .map_err(pg_err)?;
        let mut map = HashMap::with_capacity(rows.len());
        for row in &rows {
            let m = row_to_memory(row);
            map.insert(m.id.clone(), m);
        }
        Ok(map)
    }

    /// Expand a scored result set with one hop of related memories.
    /// Backend-agnostic logic mirrored from the SQLite store.
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

        let mut combined: Vec<(Memory, f32)> = initial.to_vec();
        combined.extend(neighbors);
        combined.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        combined.truncate(max_total);
        Ok(combined)
    }

    /// Memories whose topic starts with `topic` (prefix match).
    pub fn get_by_topic_prefix(&self, topic: &str) -> IcmResult<Vec<Memory>> {
        // Audit finding: `topic` (the literal prefix to match) was
        // interpolated unescaped — a topic containing `%`/`_` turned into
        // unintended wildcards within what's meant to be a literal prefix.
        // The trailing `%` is the deliberate "starts with" wildcard and
        // stays outside the escaped portion.
        let pattern = format!("{}%", escape_like_wildcards(topic));
        let mut c = self.conn()?;
        let rows = c
            .query(
                &format!(
                    "SELECT {SELECT_COLS} FROM memories WHERE topic LIKE $1 ESCAPE '\\' \
                     ORDER BY weight DESC LIMIT 500"
                ),
                &[&pattern],
            )
            .map_err(pg_err)?;
        Ok(rows.iter().map(row_to_memory).collect())
    }

    /// Distinct topics (optionally prefix-filtered) with their counts.
    pub fn list_topics_with_prefix(&self, prefix: Option<&str>) -> IcmResult<Vec<(String, usize)>> {
        let mut c = self.conn()?;
        let rows = match prefix {
            Some(p) => {
                let pattern = format!("{p}%");
                c.query(
                    "SELECT topic, COUNT(*)::bigint FROM memories WHERE topic LIKE $1 \
                     GROUP BY topic ORDER BY topic",
                    &[&pattern],
                )
            }
            None => c.query(
                "SELECT topic, COUNT(*)::bigint FROM memories GROUP BY topic ORDER BY topic",
                &[],
            ),
        }
        .map_err(pg_err)?;
        Ok(rows
            .iter()
            .map(|row| {
                let n: i64 = row.get(1);
                (row.get(0), n.max(0) as usize)
            })
            .collect())
    }

    // Consolidation / patterns

    /// Auto-consolidation is not yet implemented on the PostgreSQL
    /// backend; the call is a no-op (returns "did not consolidate") so the
    /// normal store path is unaffected. Unlike the other parity gaps (which
    /// return `Unsupported`), this one is silent by design — but the user
    /// deserves to know their `auto_consolidate_enabled = true` does nothing
    /// here, so warn once per process (audit finding).
    pub fn auto_consolidate(&self, _topic: &str, _threshold: usize) -> IcmResult<bool> {
        warn_auto_consolidate_unsupported();
        Ok(false)
    }

    /// See [`Self::auto_consolidate`].
    pub fn auto_consolidate_with_embedder(
        &self,
        _topic: &str,
        _threshold: usize,
        _embedder: Option<&dyn Embedder>,
    ) -> IcmResult<bool> {
        warn_auto_consolidate_unsupported();
        Ok(false)
    }

    /// Pattern mining is not yet available on the PostgreSQL backend.
    pub fn detect_patterns(
        &self,
        _topic: &str,
        _min_cluster_size: usize,
    ) -> IcmResult<Vec<PatternCluster>> {
        Err(IcmError::Unsupported(
            "detect_patterns (use the default SQLite backend)".into(),
        ))
    }

    /// Pattern mining is not yet available on the PostgreSQL backend.
    pub fn extract_pattern_as_concept(
        &self,
        _cluster: &PatternCluster,
        _memoir_id: &str,
    ) -> IcmResult<String> {
        Err(IcmError::Unsupported(
            "extract_pattern_as_concept (use the default SQLite backend)".into(),
        ))
    }
}
