//! Per-domain tests for the SQLite backend (`store::tests`).

use super::*;

#[test]
fn test_store_with_embedding() {
    let store = test_store();
    let mut mem = make_memory("test", "vector enabled");
    mem.embedding = Some(vec![0.1; 384]);
    let id = store.store(mem).unwrap();

    let retrieved = store.get(&id).unwrap().unwrap();
    assert!(retrieved.embedding.is_some());
    assert_eq!(retrieved.embedding.as_ref().unwrap().len(), 384);
}

#[test]
fn test_store_without_embedding() {
    let store = test_store();
    let mem = make_memory("test", "no vector");
    let id = store.store(mem).unwrap();

    let retrieved = store.get(&id).unwrap().unwrap();
    assert!(retrieved.embedding.is_none());
}

#[test]
fn test_search_by_embedding() {
    let store = test_store();

    // Store 3 memories with different embeddings
    let mut m1 = make_memory("rust", "Rust systems programming");
    m1.embedding = Some(vec![
        1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    ]);
    store.store(m1).unwrap();

    let mut m2 = make_memory("python", "Python scripting");
    // Very different embedding
    let mut emb2 = vec![0.0; 384];
    emb2[1] = 1.0;
    m2.embedding = Some(emb2);
    store.store(m2).unwrap();

    // Store one without embedding
    store.store(make_memory("go", "Go programming")).unwrap();

    // Search with a query vector close to m1
    let mut query = vec![0.0; 384];
    query[0] = 0.9;
    let results = store.search_by_embedding(&query, 5).unwrap();

    assert!(!results.is_empty());
    // First result should be closest to query
    assert_eq!(results[0].0.topic, "rust");
}

#[test]
fn test_delete_cleans_vec_table() {
    let store = test_store();
    let mut mem = make_memory("test", "to delete with vec");
    mem.embedding = Some(vec![0.5; 384]);
    let id = store.store(mem).unwrap();

    store.delete(&id).unwrap();

    // Verify vec_memories is also cleaned
    let query = vec![0.5; 384];
    let results = store.search_by_embedding(&query, 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_search_hybrid() {
    let store = test_store();

    // Store memory with both text and embedding
    let mut mem = make_memory("rust", "Rust is great for systems programming");
    mem.embedding = Some(vec![0.8; 384]);
    store.store(mem).unwrap();

    let mut mem2 = make_memory("python", "Python is great for scripting");
    let mut emb2 = vec![0.0; 384];
    emb2[1] = 1.0;
    mem2.embedding = Some(emb2);
    store.store(mem2).unwrap();

    // Hybrid search with both text match and close embedding
    let query_emb = vec![0.7; 384]; // close to m1's embedding
    let results = store
        .search_hybrid("rust programming", &query_emb, 5)
        .unwrap();

    assert!(!results.is_empty());
    // Rust should rank first (matches both FTS and vector)
    assert_eq!(results[0].0.topic, "rust");
    // Score should be > 0
    assert!(results[0].1 > 0.0);
}

/// Audit regression: `1.0 / (1.0 + rank.abs())` inverted FTS relevance —
/// a stronger bm25 match (more negative rank) scored LOWER than a weak
/// one. Neither memory has an embedding, which isolates the FTS
/// component of the hybrid score (vector side is 0.0 for both).
#[test]
fn test_search_hybrid_ranks_strong_fts_match_above_weak_one() {
    let store = test_store();

    // Strong match: the query term repeated, short document — bm25
    // favors high term frequency in a short field.
    store
        .store(make_memory(
            "t",
            "database database database database database",
        ))
        .unwrap();
    // Weak match: the query term appears once, diluted by many other
    // unrelated terms — bm25 penalizes this relative to the strong doc.
    store
        .store(make_memory(
            "t",
            "we briefly touched on a database as one topic among many \
                 entirely unrelated software engineering concerns discussed today",
        ))
        .unwrap();

    let no_embedding = vec![0.0; 384];
    let results = store.search_hybrid("database", &no_embedding, 5).unwrap();
    assert_eq!(results.len(), 2);
    let strong = results
        .iter()
        .find(|(m, _)| m.summary.starts_with("database database"))
        .expect("strong match must be present");
    let weak = results
        .iter()
        .find(|(m, _)| m.summary.starts_with("we briefly"))
        .expect("weak match must be present");
    assert!(
        strong.1 > weak.1,
        "strong FTS match ({}) must outscore weak match ({})",
        strong.1,
        weak.1
    );
}

/// Audit regression: `find_similar_memory` used to compare
/// `DEDUP_SIMILARITY_THRESHOLD` (0.85) against the hybrid
/// `0.3*fts + 0.7*cosine` score. A memory found ONLY via the vector
/// side (no shared keywords, so fts=0) could score at most
/// `0.3*0 + 0.7*1.0 = 0.70` even for a byte-identical embedding —
/// always below 0.85, so semantic-only duplicates were never caught.
/// Switching to pure `search_by_embedding` (cosine) fixes this: an
/// identical embedding now scores ~1.0, comfortably above threshold,
/// regardless of keyword overlap.
#[test]
fn test_find_similar_memory_detects_purely_semantic_duplicate() {
    let store = test_store();
    let embedding = vec![0.42; 384];

    let mut original = make_memory("t", "the quick brown fox jumps over the lazy dog");
    original.embedding = Some(embedding.clone());
    store.store(original).unwrap();

    // Shares literally no keywords with the stored summary — the FTS
    // component of the old hybrid comparison would be exactly 0.0.
    let found = icm_core::find_similar_memory(
        &store,
        "a fast animal leaping above a sleepy canine",
        &embedding,
        "t",
        icm_core::DEDUP_SIMILARITY_THRESHOLD,
    )
    .unwrap();
    assert!(
        found.is_some(),
        "an identical embedding must be detected as a duplicate even with zero keyword overlap"
    );
    assert!(found.unwrap().1 > 0.99);
}
