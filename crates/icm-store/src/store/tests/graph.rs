//! Per-domain tests for the SQLite backend (`store::tests`).

use super::*;

#[test]
fn test_expand_with_neighbors_brings_hop_1() {
    let store = test_store();
    // Create 3 memories. m1 is a direct query hit; m2 and m3 are
    // related to m1 via related_ids; m3 is unrelated.
    let mut m1 = make_memory("decisions", "primary hit");
    let mut m2 = make_memory("decisions", "related neighbor");
    let m3 = make_memory("unrelated", "far away");

    // Set up the edges before storing.
    m1.related_ids.push(m2.id.clone());
    m2.related_ids.push(m1.id.clone());

    let id1 = store.store(m1.clone()).unwrap();
    let _id2 = store.store(m2.clone()).unwrap();
    let _id3 = store.store(m3.clone()).unwrap();

    let m1_full = store.get(&id1).unwrap().unwrap();
    let initial = vec![(m1_full, 0.9_f32)];

    let expanded = store.expand_with_neighbors(&initial, 5, 0.5, 10).unwrap();

    assert_eq!(expanded.len(), 2, "primary + 1 neighbor");
    assert!(expanded.iter().any(|(m, _)| m.id == m1.id));
    assert!(
        expanded.iter().any(|(m, _)| m.id == m2.id),
        "neighbor should be pulled in"
    );
    assert!(
        expanded.iter().all(|(m, _)| m.id != m3.id),
        "unrelated memory must not be pulled in: {expanded:?}"
    );
}

#[test]
fn test_expand_with_neighbors_dedupes_initial() {
    let store = test_store();
    let mut m1 = make_memory("t", "hit 1");
    let mut m2 = make_memory("t", "hit 2");
    m1.related_ids.push(m2.id.clone());
    m2.related_ids.push(m1.id.clone());

    let id1 = store.store(m1.clone()).unwrap();
    let id2 = store.store(m2.clone()).unwrap();

    // Both already in the initial set — no neighbor to add.
    let m1_full = store.get(&id1).unwrap().unwrap();
    let m2_full = store.get(&id2).unwrap().unwrap();
    let initial = vec![(m1_full, 0.9_f32), (m2_full, 0.85_f32)];

    let expanded = store.expand_with_neighbors(&initial, 5, 0.5, 10).unwrap();
    assert_eq!(expanded.len(), 2, "no duplicates when both already present");
}

#[test]
fn test_expand_with_neighbors_respects_max_neighbors() {
    let store = test_store();
    // m1 has 5 neighbors. Cap max_neighbors at 2.
    let mut m1 = make_memory("t", "hub");
    let n1 = make_memory("t", "neighbor 1");
    let n2 = make_memory("t", "neighbor 2");
    let n3 = make_memory("t", "neighbor 3");
    let n4 = make_memory("t", "neighbor 4");
    let n5 = make_memory("t", "neighbor 5");
    m1.related_ids.extend([
        n1.id.clone(),
        n2.id.clone(),
        n3.id.clone(),
        n4.id.clone(),
        n5.id.clone(),
    ]);

    let id1 = store.store(m1.clone()).unwrap();
    for n in [&n1, &n2, &n3, &n4, &n5] {
        store.store(n.clone()).unwrap();
    }

    let m1_full = store.get(&id1).unwrap().unwrap();
    let initial = vec![(m1_full, 0.9_f32)];

    let expanded = store.expand_with_neighbors(&initial, 2, 0.5, 10).unwrap();
    // 1 primary + 2 neighbors = 3.
    assert_eq!(expanded.len(), 3);
}

#[test]
fn test_expand_with_neighbors_applies_discount() {
    let store = test_store();
    let mut m1 = make_memory("t", "primary");
    let m2 = make_memory("t", "neighbor");
    m1.related_ids.push(m2.id.clone());

    let id1 = store.store(m1.clone()).unwrap();
    store.store(m2.clone()).unwrap();

    let m1_full = store.get(&id1).unwrap().unwrap();
    let initial = vec![(m1_full, 0.9_f32)];

    let expanded = store.expand_with_neighbors(&initial, 5, 0.5, 10).unwrap();

    // Find neighbor score: should be 0.9 * 0.5 = 0.45
    let neighbor_score = expanded
        .iter()
        .find(|(m, _)| m.id == m2.id)
        .map(|(_, s)| *s)
        .unwrap();
    assert!(
        (neighbor_score - 0.45).abs() < 1e-5,
        "neighbor discount wrong: {neighbor_score}"
    );
}

#[test]
fn test_expand_with_neighbors_respects_max_total() {
    let store = test_store();
    // 3 primaries + 3 neighbors, but max_total=4 caps output.
    let mut m1 = make_memory("t", "p1");
    let mut m2 = make_memory("t", "p2");
    let mut m3 = make_memory("t", "p3");
    let n1 = make_memory("t", "n1");
    let n2 = make_memory("t", "n2");
    let n3 = make_memory("t", "n3");
    m1.related_ids.push(n1.id.clone());
    m2.related_ids.push(n2.id.clone());
    m3.related_ids.push(n3.id.clone());

    let id1 = store.store(m1.clone()).unwrap();
    let id2 = store.store(m2.clone()).unwrap();
    let id3 = store.store(m3.clone()).unwrap();
    store.store(n1).unwrap();
    store.store(n2).unwrap();
    store.store(n3).unwrap();

    let initial = vec![
        (store.get(&id1).unwrap().unwrap(), 0.9),
        (store.get(&id2).unwrap().unwrap(), 0.85),
        (store.get(&id3).unwrap().unwrap(), 0.8),
    ];

    let expanded = store.expand_with_neighbors(&initial, 5, 0.5, 4).unwrap();
    assert_eq!(expanded.len(), 4, "must respect max_total cap");
    // Top scorer remains first.
    assert!((expanded[0].1 - 0.9).abs() < 1e-5);
}

#[test]
fn test_expand_with_neighbors_empty_initial_passthrough() {
    let store = test_store();
    let expanded = store.expand_with_neighbors(&[], 5, 0.5, 10).unwrap();
    assert!(expanded.is_empty());
}

#[test]
fn test_expand_with_neighbors_zero_neighbors_disables() {
    let store = test_store();
    let mut m1 = make_memory("t", "primary");
    let m2 = make_memory("t", "would-be neighbor");
    m1.related_ids.push(m2.id.clone());

    let id1 = store.store(m1.clone()).unwrap();
    store.store(m2).unwrap();

    let initial = vec![(store.get(&id1).unwrap().unwrap(), 0.9)];
    let expanded = store.expand_with_neighbors(&initial, 0, 0.5, 10).unwrap();
    assert_eq!(expanded.len(), 1, "max_neighbors=0 disables expansion");
}

#[test]
fn test_expand_with_neighbors_skips_missing_targets() {
    let store = test_store();
    // m1 points to a ghost id that no longer exists (e.g., deleted).
    let mut m1 = make_memory("t", "has ghost link");
    m1.related_ids.push("01GHOSTID".into());
    let id1 = store.store(m1.clone()).unwrap();

    let initial = vec![(store.get(&id1).unwrap().unwrap(), 0.9)];
    let expanded = store.expand_with_neighbors(&initial, 5, 0.5, 10).unwrap();
    assert_eq!(expanded.len(), 1, "ghost link must be silently skipped");
}

// get_many (batched fetch)

#[test]
fn test_get_many_returns_requested_ids() {
    let store = test_store();
    let m1 = make_memory("t", "first");
    let m2 = make_memory("t", "second");
    let m3 = make_memory("t", "third");
    let id1 = store.store(m1.clone()).unwrap();
    let id2 = store.store(m2.clone()).unwrap();
    store.store(m3).unwrap();

    let got = store.get_many(&[id1.as_str(), id2.as_str()]).unwrap();
    assert_eq!(got.len(), 2);
    assert!(got.contains_key(&id1));
    assert!(got.contains_key(&id2));
}

#[test]
fn test_get_many_empty_input_returns_empty() {
    let store = test_store();
    let got = store.get_many(&[]).unwrap();
    assert!(got.is_empty());
}

#[test]
fn test_get_many_missing_ids_silently_dropped() {
    let store = test_store();
    let m1 = make_memory("t", "real");
    let id1 = store.store(m1).unwrap();

    let got = store.get_many(&[id1.as_str(), "01NONEXISTENT"]).unwrap();
    assert_eq!(got.len(), 1);
    assert!(got.contains_key(&id1));
}

#[test]
fn test_get_many_dedupes_input() {
    let store = test_store();
    let m1 = make_memory("t", "only");
    let id1 = store.store(m1).unwrap();

    // Same id three times — must not blow up the IN clause.
    let got = store
        .get_many(&[id1.as_str(), id1.as_str(), id1.as_str()])
        .unwrap();
    assert_eq!(got.len(), 1);
}

// LRU cache invalidation

#[test]
fn test_cache_serves_after_first_get() {
    let store = test_store();
    let m = make_memory("t", "original");
    let id = store.store(m).unwrap();

    // Warm the cache.
    let first = store.get(&id).unwrap().unwrap();
    assert_eq!(first.summary, "original");

    // Mutate the row out-of-band so a stale cache hit would show.
    store
        .conn
        .execute(
            "UPDATE memories SET summary = 'mutated' WHERE id = ?1",
            params![id],
        )
        .unwrap();

    // Cache is unaware of the raw SQL write, so we should still
    // see "original" — that's the proof the cache is serving reads.
    let cached = store.get(&id).unwrap().unwrap();
    assert_eq!(cached.summary, "original", "cache must serve hot reads");
}

#[test]
fn test_update_invalidates_cache() {
    let store = test_store();
    let m = make_memory("t", "v1");
    let id = store.store(m).unwrap();

    // Warm cache.
    let _ = store.get(&id).unwrap();

    // Proper update through the trait flushes the cache entry.
    let mut updated = store.get(&id).unwrap().unwrap();
    updated.summary = "v2".into();
    store.update(&updated).unwrap();

    let after = store.get(&id).unwrap().unwrap();
    assert_eq!(after.summary, "v2");
}

#[test]
fn test_delete_invalidates_cache() {
    let store = test_store();
    let m = make_memory("t", "doomed");
    let id = store.store(m).unwrap();

    // Warm the cache, then delete.
    let _ = store.get(&id).unwrap();
    store.delete(&id).unwrap();

    let after = store.get(&id).unwrap();
    assert!(after.is_none(), "deleted memory must not survive in cache");
}

#[test]
fn test_apply_decay_clears_cache() {
    let store = test_store();
    let m1 = make_memory("t", "a");
    let m2 = make_memory("t", "b");
    let id1 = store.store(m1).unwrap();
    let id2 = store.store(m2).unwrap();

    // Warm cache for both.
    let before1 = store.get(&id1).unwrap().unwrap().weight;
    let _ = store.get(&id2).unwrap();

    store.apply_decay(0.5).unwrap();

    // After decay, cache must have been wiped, so the next read
    // returns the decayed weight from disk.
    let after1 = store.get(&id1).unwrap().unwrap().weight;
    assert!(
        after1 < before1,
        "post-decay weight should reflect DB, not stale cache (before={before1}, after={after1})"
    );
}

#[test]
fn test_get_many_uses_cache_for_warm_ids() {
    let store = test_store();
    let m = make_memory("t", "warm");
    let id = store.store(m).unwrap();

    // Warm the cache via single get.
    let _ = store.get(&id).unwrap();

    // Out-of-band mutate — cached value should still be served by
    // get_many for this id.
    store
        .conn
        .execute(
            "UPDATE memories SET summary = 'mutated' WHERE id = ?1",
            params![id],
        )
        .unwrap();

    let got = store.get_many(&[id.as_str()]).unwrap();
    assert_eq!(got.get(&id).unwrap().summary, "warm");
}

// content-hash dedup

#[test]
fn test_dedup_same_topic_summary_collapses() {
    let store = test_store();
    let m1 = make_memory("dedup", "Use Turso for cloud sync");
    let m2 = make_memory("dedup", "Use Turso for cloud sync"); // identical content
    let id1 = store.store(m1).unwrap();
    let id2 = store.store(m2).unwrap();
    // Both calls return the SAME id (the first row's). Second store
    // is a no-op at the DB level, but the contract still returns an
    // id pointing at a real row.
    assert_eq!(id1, id2, "dedup must return the existing row's id");
    assert_eq!(store.count().unwrap(), 1, "only one row in memories");
}

#[test]
fn test_dedup_normalizes_whitespace_and_case() {
    let store = test_store();
    let m1 = make_memory("DEDUP", "Use   Turso   for cloud sync");
    let m2 = make_memory("dedup", "use turso for cloud sync");
    let id1 = store.store(m1).unwrap();
    let id2 = store.store(m2).unwrap();
    assert_eq!(id1, id2);
    assert_eq!(store.count().unwrap(), 1);
}

/// Audit regression: SQLite's built-in `LOWER()` is ASCII-only and does
/// not fold 'É' → 'é', while `summary_hash` uses Rust's Unicode-correct
/// `to_lowercase()`. Two topics that differ only in the case of an
/// accented letter must still dedup — this used to fail because the
/// (now-removed) `LOWER(topic)` index column and SELECT comparison
/// disagreed with the hash's own case-folding.
#[test]
fn test_dedup_normalizes_accented_case() {
    let store = test_store();
    let m1 = make_memory("Décisions", "on utilise Turso pour la synchro");
    let m2 = make_memory("DÉCISIONS", "on utilise Turso pour la synchro");
    let id1 = store.store(m1).unwrap();
    let id2 = store.store(m2).unwrap();
    assert_eq!(
        id1, id2,
        "accented topics differing only in case must dedup to the same row"
    );
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn test_dedup_different_topic_keeps_both() {
    let store = test_store();
    let m1 = make_memory("topic-a", "shared body");
    let m2 = make_memory("topic-b", "shared body");
    let id1 = store.store(m1).unwrap();
    let id2 = store.store(m2).unwrap();
    assert_ne!(id1, id2, "different topic = different row");
    assert_eq!(store.count().unwrap(), 2);
}

#[test]
fn test_store_is_atomic() {
    let store = test_store();
    let mut mem = make_memory("atomic", "test atomicity");
    mem.embedding = Some(vec![0.1; 384]);
    let id = mem.id.clone();

    store.store(mem).unwrap();

    // Verify main table has the row
    let retrieved = store.get(&id).unwrap().unwrap();
    assert_eq!(retrieved.summary, "test atomicity");

    // Verify vec_memories also has the row
    let vec_count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM vec_memories WHERE memory_id = ?1",
            params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(vec_count, 1);
}

#[test]
fn test_busy_timeout_pragma() {
    // Audit #185 M: 6/20 parallel hook handlers timed out at the
    // previous 5s busy_timeout. Bumping to 30s covers realistic
    // burst-write contention (large transcript extraction triggers
    // many writes on PreCompact/SessionEnd) without hiding genuine
    // lock issues — anyone holding a write lock for >30s has a
    // real bug worth surfacing.
    let store = test_store();
    let timeout: i64 = store
        .conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(timeout, 30000);
}

#[test]
fn test_fts_sanitize_utf8_safe() {
    // Build a string with multibyte chars near the 10k boundary.
    // Each emoji is 4 bytes. Fill up to just past 10_000 bytes.
    let base = "a".repeat(9_998);
    // Add a 4-byte emoji that straddles the 10_000 boundary
    let input = format!("{base}\u{1F600}\u{1F600}"); // 9998 + 4 + 4 = 10006 bytes
    assert!(input.len() > 10_000);

    // This should not panic (the old code could split a UTF-8 char)
    let result = sanitize_fts_query(&input);
    // The result should be valid UTF-8 (it's a String, so it is by construction)
    assert!(!result.is_empty());
    // The truncated input should not contain partial emoji
    // (9998 + 4 = 10002 > 10000, so the emoji at 9998 is excluded; end = 9998)
    // Result should just be the 'a' tokens
}

#[test]
fn test_forget_topic() {
    let store = test_store();

    // Create 3 memories in topic "ephemeral"
    for i in 0..3 {
        let m = make_memory("ephemeral", &format!("item {i}"));
        store.store(m).unwrap();
    }

    // Verify they exist
    let before = store.get_by_topic("ephemeral").unwrap();
    assert_eq!(before.len(), 3);

    // Delete all memories in the topic
    for m in &before {
        store.delete(&m.id).unwrap();
    }

    // Verify 0 remain
    let after = store.get_by_topic("ephemeral").unwrap();
    assert!(after.is_empty());
}

// === TranscriptStore tests ===
