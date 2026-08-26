//! Per-domain tests for the SQLite backend (`store::tests`).

use super::*;

#[test]
fn test_facts_set_and_get_roundtrip() {
    let store = test_store();
    let id = store
        .set_fact("project:icm", "gcp.project", "rtk-ai-labs-01", "cli")
        .unwrap();
    assert!(!id.is_empty());

    let f = store
        .get_fact("project:icm", "gcp.project")
        .unwrap()
        .expect("active fact must exist");
    assert_eq!(f.value, "rtk-ai-labs-01");
    assert_eq!(f.source, "cli");
    assert!(f.is_active());
}

#[test]
fn test_facts_set_same_value_is_noop() {
    let store = test_store();
    let id1 = store.set_fact("e", "k", "v", "src").unwrap();
    let id2 = store.set_fact("e", "k", "v", "src2").unwrap();
    assert_eq!(id1, id2, "same value re-asserted must NOT create a new row");
    let history = store.history("e", "k").unwrap();
    assert_eq!(history.len(), 1);
    // Source is NOT updated by the no-op (intentional: avoids
    // creating noise just because the same fact was re-asserted
    // from a different surface).
    assert_eq!(history[0].source, "src");
}

#[test]
fn test_facts_supersede_keeps_history() {
    let store = test_store();
    let id1 = store
        .set_fact("project:icm", "version", "0.10.51", "release-please")
        .unwrap();
    let id2 = store
        .set_fact("project:icm", "version", "0.10.52", "release-please")
        .unwrap();
    assert_ne!(id1, id2);

    let active = store
        .get_fact("project:icm", "version")
        .unwrap()
        .expect("active fact must exist after supersession");
    assert_eq!(active.value, "0.10.52");
    assert!(active.is_active());

    let history = store.history("project:icm", "version").unwrap();
    assert_eq!(history.len(), 2);
    // History returned newest-first.
    assert_eq!(history[0].value, "0.10.52");
    assert!(history[0].is_active());
    assert_eq!(history[1].value, "0.10.51");
    assert!(!history[1].is_active(), "older row must be superseded");
}

#[test]
fn test_facts_list_by_entity_alpha_sorted() {
    let store = test_store();
    store
        .set_fact("host:db", "owner", "ops-team", "cli")
        .unwrap();
    store
        .set_fact("host:db", "deploy.region", "europe-west1", "cli")
        .unwrap();
    store.set_fact("host:db", "cpu.cores", "16", "cli").unwrap();
    store
        .set_fact("host:web", "owner", "ui-team", "cli")
        .unwrap();

    let all = store.list_facts("host:db", None).unwrap();
    let keys: Vec<&str> = all.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, vec!["cpu.cores", "deploy.region", "owner"]);
    // Other entity NOT included.
    assert!(all.iter().all(|f| f.entity == "host:db"));
}

#[test]
fn test_facts_list_prefix_filter() {
    let store = test_store();
    store
        .set_fact("svc:api", "deploy.region", "europe-west1", "cli")
        .unwrap();
    store
        .set_fact("svc:api", "deploy.replicas", "3", "cli")
        .unwrap();
    store
        .set_fact("svc:api", "owner", "platform-team", "cli")
        .unwrap();

    let deploys = store.list_facts("svc:api", Some("deploy.")).unwrap();
    assert_eq!(deploys.len(), 2);
    assert!(deploys.iter().all(|f| f.key.starts_with("deploy.")));
}

#[test]
fn test_facts_forget_drops_history_too() {
    let store = test_store();
    store.set_fact("e", "k", "v1", "cli").unwrap();
    store.set_fact("e", "k", "v2", "cli").unwrap();
    let n = store.forget_fact("e", "k").unwrap();
    assert_eq!(n, 2, "must delete both active and superseded rows");
    assert!(store.get_fact("e", "k").unwrap().is_none());
    assert!(store.history("e", "k").unwrap().is_empty());
}

#[test]
fn test_facts_stats_breakdown() {
    let store = test_store();
    store.set_fact("e1", "a", "1", "cli").unwrap();
    store.set_fact("e1", "b", "2", "cli").unwrap();
    store.set_fact("e2", "a", "3", "cli").unwrap();
    // Supersede e1.a — history grows, active stays the same.
    store.set_fact("e1", "a", "1-bis", "cli").unwrap();

    let stats = store.facts_stats().unwrap();
    assert_eq!(stats.active_count, 3, "3 active slots");
    assert_eq!(stats.total_count, 4, "4 rows including superseded");
    assert_eq!(stats.distinct_entities, 2);
    let top: Vec<&str> = stats.top_entities.iter().map(|(e, _)| e.as_str()).collect();
    assert!(top.contains(&"e1") && top.contains(&"e2"));
}

#[test]
fn test_facts_rejects_empty_entity_or_key() {
    let store = test_store();
    assert!(store.set_fact("", "k", "v", "cli").is_err());
    assert!(store.set_fact("e", "", "v", "cli").is_err());
}

/// Issue #273 perf invariant: primary-key lookup must stay
/// sub-millisecond even at 10k facts. Loose budget so CI runners
/// don't flake.
#[test]
fn perf_facts_get_at_10k_under_5ms() {
    let store = test_store();
    for i in 0..10_000 {
        let entity = format!("entity:{}", i % 100);
        let key = format!("key.{i}");
        store
            .set_fact(&entity, &key, &format!("val-{i}"), "bench")
            .unwrap();
    }
    let start = std::time::Instant::now();
    for _ in 0..1_000 {
        let _ = store.get_fact("entity:42", "key.42").unwrap();
    }
    let elapsed = start.elapsed();
    let per_lookup_us = elapsed.as_micros() / 1_000;
    // 5ms / lookup in debug mode — generous; release is well
    // under 1ms.
    assert!(
        per_lookup_us < 5_000,
        "facts.get averaged {per_lookup_us}us / lookup over 1k iters (budget 5000us)",
    );
}

// ──────────────────────────────────────────────────────────────
// list_all_facts tests
// ──────────────────────────────────────────────────────────────

/// When a second `set_fact` supersedes the first for the same (entity, key),
/// `list_all_facts` should return exactly one active fact — the latest one.
#[test]
fn list_all_facts_returns_only_active() {
    let store = test_store();
    // Insert first value.
    store.set_fact("host:db", "ip", "10.0.0.1", "test").unwrap();
    // Supersede it with a new value.
    store.set_fact("host:db", "ip", "10.0.0.2", "test").unwrap();
    let facts = store.list_all_facts().unwrap();
    assert_eq!(
        facts.len(),
        1,
        "list_all_facts must return exactly 1 active fact after supersession"
    );
    assert_eq!(
        facts[0].value, "10.0.0.2",
        "the active fact must be the latest value"
    );
    assert!(
        facts[0].superseded_at.is_none(),
        "the returned fact must have superseded_at = None (still active)"
    );
}
