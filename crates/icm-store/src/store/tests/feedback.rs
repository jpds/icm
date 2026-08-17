//! Per-domain tests for the SQLite backend (`store::tests`).

use super::*;

// === FeedbackStore tests ===

#[test]
fn test_feedback_store_and_list() {
    let store = test_store();
    let fb = make_feedback("triage", "issue about crashes", "low", "high");
    let id = fb.id.clone();
    store.store_feedback(fb).unwrap();

    let results = store.list_feedback(None, 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, id);
    assert_eq!(results[0].topic, "triage");
    assert_eq!(results[0].predicted, "low");
    assert_eq!(results[0].corrected, "high");
}

#[test]
fn test_feedback_list_by_topic() {
    let store = test_store();
    store
        .store_feedback(make_feedback("triage", "ctx1", "a", "b"))
        .unwrap();
    store
        .store_feedback(make_feedback("pr-review", "ctx2", "c", "d"))
        .unwrap();

    let triage = store.list_feedback(Some("triage"), 10).unwrap();
    assert_eq!(triage.len(), 1);
    assert_eq!(triage[0].topic, "triage");

    let all = store.list_feedback(None, 10).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn test_feedback_search() {
    let store = test_store();
    store
        .store_feedback(make_feedback(
            "triage",
            "user reports memory leak",
            "low priority",
            "high priority",
        ))
        .unwrap();
    store
        .store_feedback(make_feedback(
            "triage",
            "build failure on CI",
            "feature",
            "bug",
        ))
        .unwrap();

    let results = store
        .search_feedback("memory leak", None, None, 10)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].context.contains("memory leak"));
}

#[test]
fn test_feedback_search_with_topic_filter() {
    let store = test_store();
    store
        .store_feedback(make_feedback("triage", "memory issue", "low", "high"))
        .unwrap();
    store
        .store_feedback(make_feedback("pr-review", "memory usage", "ok", "bad"))
        .unwrap();

    let results = store
        .search_feedback("memory", None, Some("triage"), 10)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].topic, "triage");
}

/// Manual-testing finding: `feedback search` had no semantic fallback
/// at all — pure FTS5 with implicit AND, so a query missing even one
/// exact token (no stemming: "formatting" != "format") returned
/// nothing, even with an obviously relevant entry stored. Proves the
/// fix: the exact real-world query that failed now succeeds once an
/// embedding is attached and a query embedding is supplied.
#[test]
fn feedback_search_falls_back_to_semantic_similarity_on_partial_fts_miss() {
    let store = SqliteStore::in_memory_with_dims(64).unwrap();

    let mut fb = make_feedback(
        "code-style",
        "user asked to format a date",
        "used strftime with %Y-%m-%d",
        "should use format_local helper for timezone consistency",
    );
    fb.embedding = Some(vec![0.5_f32; 64]);
    store.store_feedback(fb).unwrap();

    // FTS-only (no query embedding) must reproduce the original bug:
    // "formatting" has no exact-token match anywhere in the entry.
    let fts_only = store
        .search_feedback("date formatting", None, None, 10)
        .unwrap();
    assert!(
        fts_only.is_empty(),
        "sanity check: FTS-only must still miss this partial-token query"
    );

    // With a query embedding, semantic similarity must find it even
    // though the FTS side still misses.
    let query_embedding = vec![0.5_f32; 64];
    let hybrid = store
        .search_feedback("date formatting", Some(&query_embedding), None, 10)
        .unwrap();
    assert_eq!(
        hybrid.len(),
        1,
        "semantic fallback must surface the entry FTS alone misses"
    );
}

#[test]
fn test_feedback_increment_applied() {
    let store = test_store();
    let fb = make_feedback("triage", "ctx", "a", "b");
    let id = fb.id.clone();
    store.store_feedback(fb).unwrap();

    store.increment_applied(&id).unwrap();
    store.increment_applied(&id).unwrap();

    let results = store.list_feedback(None, 10).unwrap();
    assert_eq!(results[0].applied_count, 2);
}

#[test]
fn test_feedback_increment_applied_not_found() {
    let store = test_store();
    let result = store.increment_applied("nonexistent");
    assert!(result.is_err());
}

#[test]
fn test_feedback_delete() {
    let store = test_store();
    let fb = make_feedback("triage", "ctx", "a", "b");
    let id = fb.id.clone();
    store.store_feedback(fb).unwrap();

    store.delete_feedback(&id).unwrap();
    let results = store.list_feedback(None, 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_feedback_delete_not_found() {
    let store = test_store();
    let result = store.delete_feedback("nonexistent");
    assert!(result.is_err());
}

#[test]
fn test_feedback_stats() {
    let store = test_store();
    store
        .store_feedback(make_feedback("triage", "ctx1", "a", "b"))
        .unwrap();
    store
        .store_feedback(make_feedback("triage", "ctx2", "c", "d"))
        .unwrap();
    store
        .store_feedback(make_feedback("pr-review", "ctx3", "e", "f"))
        .unwrap();

    let fb = make_feedback("triage", "ctx4", "g", "h");
    let id = fb.id.clone();
    store.store_feedback(fb).unwrap();
    store.increment_applied(&id).unwrap();

    let stats = store.feedback_stats().unwrap();
    assert_eq!(stats.total, 4);
    assert_eq!(stats.by_topic.len(), 2);
    assert_eq!(stats.by_topic[0].0, "triage");
    assert_eq!(stats.by_topic[0].1, 3);
    assert_eq!(stats.most_applied.len(), 1);
    assert_eq!(stats.most_applied[0].1, 1);
}

// === sanitize_fts_query tests ===
