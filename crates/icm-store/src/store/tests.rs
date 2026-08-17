//! SQLite backend — split out of the former monolithic `store.rs`.
//!
//! `SqliteStore` and the shared row/parse helpers live in `super`
//! (`store/mod.rs`); each submodule here holds one trait impl (or a
//! coherent group of inherent methods) on that type.

use super::*;
use icm_core::Importance;

fn test_store() -> SqliteStore {
    SqliteStore::in_memory().unwrap()
}

fn make_memory(topic: &str, summary: &str) -> Memory {
    Memory::new(topic.into(), summary.into(), Importance::Medium)
}

fn make_memoir(name: &str) -> Memoir {
    Memoir::new(name.into(), format!("Description for {name}"))
}

fn make_concept(memoir_id: &str, name: &str, definition: &str) -> Concept {
    Concept::new(memoir_id.into(), name.into(), definition.into())
}

// === Integrity check / repair (issue #313) ===

#[test]
fn integrity_check_ok_on_healthy_db() {
    let store = test_store();
    store.store(make_memory("t", "healthy row")).unwrap();
    assert_eq!(store.integrity_check().unwrap(), vec!["ok".to_string()]);
}

#[test]
fn integrity_check_structural_works_on_a_read_only_connection() {
    // #313 follow-up: `icm doctor` / `repair --dry-run` must inspect a DB
    // without a writable open. The structural check runs `PRAGMA
    // integrity_check` only (no FTS INSERT), so it works read-only.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.db");
    let _ = seed_writable_db(&path);

    let ro = SqliteStore::open_readonly(&path).unwrap();
    assert!(ro.is_readonly());
    // Read-only structural check succeeds and reports healthy.
    assert_eq!(
        ro.integrity_check_structural().unwrap(),
        vec!["ok".to_string()]
    );
    // The full check issues an FTS `INSERT` a read-only connection can't
    // run, so it degrades to reporting that as a problem — which is exactly
    // why the read-only inspection paths use the structural variant.
    assert_ne!(ro.integrity_check().unwrap(), vec!["ok".to_string()]);
}

#[test]
fn rebuild_search_indexes_lists_fts_tables_and_keeps_integrity() {
    let store = test_store();
    store.store(make_memory("t", "a row to index")).unwrap();
    let rebuilt = store.rebuild_search_indexes().unwrap();
    // memories_fts always exists; the concepts/feedback/messages FTS
    // tables are created by schema init too.
    assert!(
        rebuilt.contains(&"memories_fts".to_string()),
        "got: {rebuilt:?}"
    );
    assert_eq!(store.integrity_check().unwrap(), vec!["ok".to_string()]);
}

#[test]
fn rebuild_search_indexes_regenerates_fts_from_content() {
    // The core repair mechanism (#313): rebuilding must reconstruct the
    // FTS index from the intact content table. Deterministically wipe the
    // FTS index (the "index damaged, base table intact" class) and prove
    // rebuild restores searchability.
    let store = test_store();
    store
        .store(make_memory("t", "singulartoken repairable"))
        .unwrap();

    let fts_hits = |s: &SqliteStore| -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'singulartoken'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(fts_hits(&store), 1, "token should be indexed after store");

    // Wipe the FTS index while leaving the content row in `memories`.
    store
        .conn
        .execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('delete-all');")
        .unwrap();
    assert_eq!(fts_hits(&store), 0, "index wiped → no FTS hit");

    // integrity_check (rank=1 content check) must flag the desync so that
    // `icm repair` actually triggers on it rather than reporting healthy.
    assert_ne!(
        store.integrity_check().unwrap(),
        vec!["ok".to_string()],
        "index/content desync must be detected"
    );

    store.rebuild_search_indexes().unwrap();
    assert_eq!(fts_hits(&store), 1, "rebuild must regenerate the FTS index");
    assert_eq!(store.integrity_check().unwrap(), vec!["ok".to_string()]);
}

#[test]
fn open_maintenance_errors_on_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.db");
    match SqliteStore::open_maintenance(&path) {
        Ok(_) => panic!("open_maintenance on missing file must error"),
        Err(IcmError::NotFound(_)) => {}
        Err(other) => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn open_maintenance_opens_existing_db_writable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seeded.db");
    let _ = seed_writable_db(&path);
    let store = SqliteStore::open_maintenance(&path).unwrap();
    assert!(!store.is_readonly());
    assert_eq!(store.integrity_check().unwrap(), vec!["ok".to_string()]);
}

// === Read-only store (issue #263) ===

fn seed_writable_db(path: &Path) -> Memory {
    let store = SqliteStore::new(path).unwrap();
    let mut m = make_memory("project:icm", "read-only fixture summary");
    m.embedding = Some(vec![0.1_f32; icm_core::DEFAULT_EMBEDDING_DIMS]);
    store.store(m.clone()).unwrap();
    m
}

#[test]
fn open_readonly_errors_on_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.db");
    match SqliteStore::open_readonly(&path) {
        Ok(_) => panic!("open_readonly on missing file must error"),
        Err(IcmError::NotFound(msg)) => {
            assert!(msg.contains("absent.db"), "msg: {msg}")
        }
        Err(other) => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn open_readonly_can_read_existing_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seeded.db");
    let seeded = seed_writable_db(&path);

    let ro = SqliteStore::open_readonly(&path).unwrap();
    assert!(ro.is_readonly());

    // Read by id: must return the seeded memory verbatim.
    let got = ro.get(&seeded.id).unwrap().expect("memory must be present");
    assert_eq!(got.topic, "project:icm");
    assert_eq!(got.summary, "read-only fixture summary");
}

#[test]
fn read_only_connection_sees_writes_committed_after_open() {
    // #319: a long-lived `--read-only serve` connection must observe
    // writes that hooks/CLI commit to the same DB *after* the server
    // opened. The old `immutable=1` open served a permanently stale
    // snapshot (and eventually spurious "database disk image is malformed"
    // on a healthy DB).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.db");
    let _ = seed_writable_db(&path); // 1 memory, WAL mode

    let ro = SqliteStore::open_readonly(&path).unwrap();
    assert_eq!(ro.count().unwrap(), 1);

    // A separate writer commits a new memory to the same file.
    {
        let rw = SqliteStore::new(&path).unwrap();
        let mut m = make_memory("project:icm", "written after the reader opened");
        m.embedding = Some(vec![0.2_f32; icm_core::DEFAULT_EMBEDDING_DIMS]);
        rw.store(m).unwrap();
    }

    // The already-open read-only connection must now see it.
    assert_eq!(
        ro.count().unwrap(),
        2,
        "read-only connection must see writes committed after it opened"
    );
}

#[cfg(unix)]
#[test]
fn open_readonly_falls_back_to_immutable_on_unwritable_dir() {
    // #263 must keep working after #319: on a `chmod -w` parent directory
    // the live `mode=ro` open can't create the `-shm` sidecar for a WAL
    // DB, so open_readonly must fall back to `immutable=1` and still read.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sandboxed.db");
    let seeded = seed_writable_db(&path);
    // Keep the DB in WAL mode (that's what forces the -shm requirement)
    // but fold all rows into the main file and drop the sidecars, so a
    // fresh read-only open must recreate -shm — which fails in a
    // read-only dir. (A DELETE-mode DB would open fine via plain mode=ro
    // and never exercise the fallback.)
    {
        let rw = SqliteStore::new(&path).unwrap();
        rw.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    for ext in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_os_string();
        side.push(ext);
        let _ = std::fs::remove_file(std::path::PathBuf::from(side));
    }

    let original = std::fs::metadata(dir.path()).unwrap().permissions();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    // Guard: confirm the live `mode=ro` path is genuinely unusable here,
    // otherwise this test would pass without ever exercising the fallback.
    let live_reads = match open_readonly_uri(&path, false) {
        Ok(conn) => conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .is_ok(),
        Err(_) => false,
    };
    // The public open_readonly must still succeed via the immutable fallback.
    let opened = SqliteStore::open_readonly(&path);

    // Restore permissions before asserting so tempdir cleanup succeeds.
    std::fs::set_permissions(dir.path(), original).unwrap();

    assert!(
        !live_reads,
        "sandbox setup must make the live mode=ro path unusable, else the fallback isn't tested"
    );
    let ro = opened.expect("read-only open must fall back to immutable on a chmod -w dir");
    let got = ro
        .get(&seeded.id)
        .unwrap()
        .expect("memory must be readable");
    assert_eq!(got.topic, "project:icm");
}

#[test]
fn read_only_recall_path_skips_access_bookkeeping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seeded.db");
    let seeded = seed_writable_db(&path);

    let ro = SqliteStore::open_readonly(&path).unwrap();
    // The exact methods recall depends on:
    ro.maybe_auto_decay().unwrap();
    ro.update_access(&seeded.id).unwrap();
    ro.batch_update_access(&[&seeded.id]).unwrap();

    // Re-open writable to confirm nothing actually changed.
    let rw = SqliteStore::new(&path).unwrap();
    let after = rw.get(&seeded.id).unwrap().unwrap();
    assert_eq!(
        after.access_count, seeded.access_count,
        "access_count must NOT have been bumped by a read-only call",
    );
    assert_eq!(
        after.last_accessed, seeded.last_accessed,
        "last_accessed must NOT have been touched by a read-only call",
    );
}

#[test]
fn read_only_apply_decay_returns_readonly_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seeded.db");
    seed_writable_db(&path);
    let ro = SqliteStore::open_readonly(&path).unwrap();
    let err = ro.apply_decay(0.9).unwrap_err();
    match err {
        IcmError::ReadOnly(op) => assert_eq!(op, "apply_decay"),
        other => panic!("expected ReadOnly, got {other:?}"),
    }
}

#[test]
fn read_only_mutation_attempts_are_rejected_by_sqlite() {
    // Defense-in-depth: even mutation methods that aren't explicitly
    // gated must fail because the SQLite connection itself is
    // opened RO. If this test ever passes, SQLite's RO flag has
    // been bypassed somewhere.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seeded.db");
    seed_writable_db(&path);
    let ro = SqliteStore::open_readonly(&path).unwrap();
    let m = make_memory("project:icm", "should not land");
    let err = ro.store(m).unwrap_err();
    // SQLite's own "attempt to write a readonly database" wraps into
    // IcmError::Database — the actual variant doesn't matter, only
    // that the write was blocked.
    match err {
        IcmError::Database(_) | IcmError::ReadOnly(_) => {}
        other => panic!("expected Database or ReadOnly, got {other:?}"),
    }
}

// === Embedding dim peek (issue #267) ===

/// `read_stored_embedding_dims` must NOT trigger schema init: it is
/// called from the CLI *before* `with_dims` precisely to avoid the
/// destructive recreate path when running in `--no-embeddings`
/// against a DB that was populated with a non-default dim.
#[test]
fn read_stored_dims_returns_none_for_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist.db");
    assert_eq!(
        SqliteStore::read_stored_embedding_dims(&path).unwrap(),
        None
    );
}

#[test]
fn read_stored_dims_returns_none_for_legacy_db_without_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    // Plain SQLite file with no icm_metadata table — pretends to be a
    // pre-metadata legacy DB.
    let conn = Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE foo (id TEXT)", []).unwrap();
    drop(conn);
    assert_eq!(
        SqliteStore::read_stored_embedding_dims(&path).unwrap(),
        None
    );
}

#[test]
fn read_stored_dims_returns_stored_value_for_populated_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("populated.db");
    // Build a DB at 1024 dims (representative of multilingual-e5-large).
    let store = SqliteStore::with_dims(&path, 1024).unwrap();
    drop(store);

    assert_eq!(
        SqliteStore::read_stored_embedding_dims(&path).unwrap(),
        Some(1024),
        "should return the stored dim, not the default 384",
    );
}

/// Issue #267 regression: opening the store at the *stored* dim
/// (the path the CLI now takes when no embedder is loaded) must
/// leave `vec_memories` and the `embedding` blobs intact. Before
/// the fix the CLI passed `DEFAULT_EMBEDDING_DIMS` instead and the
/// `stored != requested` branch of `init_db_with_dims` would
/// silently DROP `vec_memories` + NULL out every embedding.
#[test]
fn opening_at_stored_dims_preserves_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preserve.db");

    // First open: build the DB at 1024 dims and seed a memory with a
    // matching embedding.
    {
        let store = SqliteStore::with_dims(&path, 1024).unwrap();
        let mut mem = make_memory("topic-1024", "user prefers e5-large");
        mem.embedding = Some(vec![0.5_f32; 1024]);
        store.store(mem.clone()).unwrap();
    }

    // CLI-side resolution: peek the stored dims, then reopen at
    // exactly that value (the fix path).
    let stored = SqliteStore::read_stored_embedding_dims(&path)
        .unwrap()
        .expect("stored dim must be readable on populated DB");
    assert_eq!(stored, 1024);

    let store = SqliteStore::with_dims(&path, stored).unwrap();
    // Vec table still exists with the original dim.
    let dim_str: String = store
        .conn
        .query_row(
            "SELECT value FROM icm_metadata WHERE key = 'embedding_dims'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dim_str, "1024");

    // The seeded memory's embedding still has the original 1024
    // floats — the destructive migration did NOT run.
    let kept_bytes: Option<i64> = store
        .conn
        .query_row(
            "SELECT length(embedding) FROM memories WHERE topic = 'topic-1024'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        kept_bytes,
        Some(1024 * 4),
        "embedding blob must NOT have been NULLed by the open path",
    );
}

// === MemoryStore tests ===

#[test]
fn test_store_and_get() {
    let store = test_store();
    let mem = make_memory("test", "hello world");
    let id = mem.id.clone();

    store.store(mem).unwrap();
    let retrieved = store.get(&id).unwrap().unwrap();
    assert_eq!(retrieved.summary, "hello world");
    assert_eq!(retrieved.topic, "test");
}

#[test]
fn test_get_not_found() {
    let store = test_store();
    let result = store.get("nonexistent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_update() {
    let store = test_store();
    let mut mem = make_memory("test", "original");
    let id = mem.id.clone();
    store.store(mem.clone()).unwrap();

    mem.summary = "updated".into();
    store.update(&mem).unwrap();

    let retrieved = store.get(&id).unwrap().unwrap();
    assert_eq!(retrieved.summary, "updated");
}

#[test]
fn test_delete() {
    let store = test_store();
    let mem = make_memory("test", "to delete");
    let id = mem.id.clone();
    store.store(mem).unwrap();

    store.delete(&id).unwrap();
    assert!(store.get(&id).unwrap().is_none());
}

#[test]
fn test_delete_not_found() {
    let store = test_store();
    let result = store.delete("nonexistent");
    assert!(matches!(result, Err(IcmError::NotFound(_))));
}

/// Manual-testing finding: deleting a memory left it as a dangling
/// entry in every other memory's `related_ids` (auto-link
/// back-references) forever. `expand_with_neighbors` tolerates the
/// miss silently, but each stale id still spends a slot out of the
/// caller's `max_neighbors` budget instead of surfacing a real, live
/// neighbor, and any external consumer of the JSON export sees a
/// reference to nothing.
#[test]
fn test_delete_cleans_up_dangling_related_ids() {
    let store = test_store();

    let mut a = make_memory("t", "memory a");
    let mut b = make_memory("t", "memory b");
    let mut c = make_memory("t", "memory c");
    let a_id = store.store(a.clone()).unwrap();
    let b_id = store.store(b.clone()).unwrap();
    let c_id = store.store(c.clone()).unwrap();

    a.id = a_id.clone();
    a.related_ids = vec![b_id.clone(), c_id.clone()];
    store.update(&a).unwrap();
    b.id = b_id.clone();
    b.related_ids = vec![a_id.clone(), c_id.clone()];
    store.update(&b).unwrap();
    c.id = c_id.clone();
    c.related_ids = vec![a_id.clone(), b_id.clone()];
    store.update(&c).unwrap();

    store.delete(&a_id).unwrap();

    let b_after = store.get(&b_id).unwrap().unwrap();
    assert_eq!(
        b_after.related_ids,
        vec![c_id.clone()],
        "b's related_ids must no longer reference the deleted a"
    );
    let c_after = store.get(&c_id).unwrap().unwrap();
    assert_eq!(
        c_after.related_ids,
        vec![b_id.clone()],
        "c's related_ids must no longer reference the deleted a"
    );
}

#[test]
fn test_search_fts() {
    let store = test_store();
    store
        .store(make_memory(
            "rust",
            "Rust is a systems programming language",
        ))
        .unwrap();
    store
        .store(make_memory("python", "Python is great for scripting"))
        .unwrap();

    let results = store.search_fts("rust programming", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].topic, "rust");
}

#[test]
fn test_search_by_keywords() {
    let store = test_store();
    let mut mem = make_memory("test", "database optimization tips");
    mem.keywords = vec!["database".into(), "optimization".into()];
    store.store(mem).unwrap();

    let results = store.search_by_keywords(&["database"], 10).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_list_topics() {
    let store = test_store();
    store.store(make_memory("alpha", "first")).unwrap();
    store.store(make_memory("alpha", "second")).unwrap();
    store.store(make_memory("beta", "third")).unwrap();

    let topics = store.list_topics().unwrap();
    assert_eq!(topics.len(), 2);
    assert!(topics.contains(&("alpha".into(), 2)));
    assert!(topics.contains(&("beta".into(), 1)));
}

#[test]
fn test_restore_upgrades_importance_on_dedup() {
    // Audit #185 H2: re-storing the same content with a higher
    // importance must upgrade the existing row, not silently
    // drop the new value.
    let store = test_store();
    let mut first = make_memory("topic", "long enough summary content for storage");
    first.importance = Importance::Medium;
    let id1 = store.store(first).unwrap();

    let mut second = make_memory("topic", "long enough summary content for storage");
    second.importance = Importance::Critical;
    let id2 = store.store(second).unwrap();
    assert_eq!(id1, id2, "dedup must return the same id");

    let merged = store.get(&id1).unwrap().unwrap();
    assert_eq!(
        merged.importance,
        Importance::Critical,
        "re-store with higher priority must upgrade importance"
    );
}

#[test]
fn test_restore_does_not_downgrade_importance_on_dedup() {
    // Sanity: a re-store with a lower priority is a no-op so an
    // accidental write can't downgrade an already-flagged
    // critical memory.
    let store = test_store();
    let mut first = make_memory("topic", "long enough summary content for storage");
    first.importance = Importance::Critical;
    let id1 = store.store(first).unwrap();

    let mut second = make_memory("topic", "long enough summary content for storage");
    second.importance = Importance::Low;
    store.store(second).unwrap();

    let preserved = store.get(&id1).unwrap().unwrap();
    assert_eq!(
        preserved.importance,
        Importance::Critical,
        "re-store with lower priority must not downgrade importance"
    );
}

#[test]
fn test_restore_unions_keywords_on_dedup() {
    let store = test_store();
    let mut first = make_memory("topic", "long enough summary content for storage");
    first.keywords = vec!["alpha".into(), "beta".into()];
    let id1 = store.store(first).unwrap();

    let mut second = make_memory("topic", "long enough summary content for storage");
    second.keywords = vec!["beta".into(), "gamma".into()];
    store.store(second).unwrap();

    let merged = store.get(&id1).unwrap().unwrap();
    assert_eq!(
        merged.keywords,
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string(),],
        "keywords must be unioned and deduped, preserving existing order"
    );
}

#[test]
fn test_restore_sets_raw_excerpt_when_previously_none() {
    let store = test_store();
    let first = make_memory("topic", "long enough summary content for storage");
    let id1 = store.store(first).unwrap();
    assert!(store.get(&id1).unwrap().unwrap().raw_excerpt.is_none());

    let mut second = make_memory("topic", "long enough summary content for storage");
    second.raw_excerpt = Some("verbatim copy from source".into());
    store.store(second).unwrap();

    let merged = store.get(&id1).unwrap().unwrap();
    assert_eq!(
        merged.raw_excerpt.as_deref(),
        Some("verbatim copy from source"),
    );
}

#[test]
fn test_restore_keeps_existing_raw_excerpt_when_new_is_none() {
    let store = test_store();
    let mut first = make_memory("topic", "long enough summary content for storage");
    first.raw_excerpt = Some("important verbatim".into());
    let id1 = store.store(first).unwrap();

    let second = make_memory("topic", "long enough summary content for storage");
    store.store(second).unwrap();

    let preserved = store.get(&id1).unwrap().unwrap();
    assert_eq!(
        preserved.raw_excerpt.as_deref(),
        Some("important verbatim"),
        "re-store with None must not erase existing raw_excerpt"
    );
}

#[test]
fn test_restore_unchanged_metadata_is_noop() {
    let store = test_store();
    let mut first = make_memory("topic", "long enough summary content for storage");
    first.importance = Importance::High;
    first.keywords = vec!["alpha".into()];
    let id1 = store.store(first.clone()).unwrap();
    let original = store.get(&id1).unwrap().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(10));
    store.store(first).unwrap();
    let after = store.get(&id1).unwrap().unwrap();

    assert_eq!(
        original.updated_at, after.updated_at,
        "no-op re-store must not bump updated_at"
    );
}

#[test]
fn test_apply_decay() {
    let store = test_store();
    store.store(make_memory("test", "decayable")).unwrap();

    let mut critical = make_memory("test", "critical memory");
    critical.importance = Importance::Critical;
    store.store(critical).unwrap();

    let affected = store.apply_decay(0.9).unwrap();
    assert_eq!(affected, 1); // Only the non-critical one
}

/// Audit regression: `apply_decay` must never drive weight negative.
/// Low importance (2x multiplier), zero access count, factor=0.4 (still
/// inside the CLI's own `[0.0, 1.0)` validation) makes the raw
/// multiplier `1.0 - (1.0-0.4)*2.0 = -0.2` — negative before the
/// `MAX(0.0, ...)` clamp.
#[test]
fn test_apply_decay_never_goes_negative() {
    let store = test_store();
    let mut low = make_memory("t", "low importance, never accessed");
    low.importance = Importance::Low;
    store.store(low).unwrap();

    store.apply_decay(0.4).unwrap();

    let mem = store.get_by_topic("t").unwrap().into_iter().next().unwrap();
    assert!(
        mem.weight >= 0.0,
        "weight must never go negative, got {}",
        mem.weight
    );
}

/// Audit regression: `maybe_auto_decay` used to apply a flat 0.95 step
/// whenever >= 1 day had passed, regardless of how many days actually
/// elapsed. Simulate a 5-day gap (by backdating `last_decay_at`
/// directly) and assert the applied decay compounds to ~0.95^5, not a
/// single 0.95 step.
#[test]
fn test_maybe_auto_decay_scales_with_elapsed_days() {
    let store = test_store();
    let mut mem = make_memory("t", "elapsed-days probe");
    mem.importance = Importance::Medium;
    store.store(mem).unwrap();

    let five_days_ago = (Utc::now() - chrono::Duration::days(5)).to_rfc3339();
    store
        .conn
        .execute(
            "INSERT INTO icm_metadata (key, value) VALUES ('last_decay_at', ?1)",
            params![five_days_ago],
        )
        .unwrap();

    store.maybe_auto_decay().unwrap();

    let after = store.get_by_topic("t").unwrap().into_iter().next().unwrap();
    // Medium importance, access_count=0 -> multiplier = factor directly.
    let expected_single_step = 0.95_f32;
    let expected_five_days = 0.95_f32.powi(5);
    assert!(
        (after.weight - expected_five_days).abs() < 0.01,
        "expected weight ~{expected_five_days} (0.95^5) for a 5-day gap, got {}",
        after.weight
    );
    assert!(
        after.weight < expected_single_step - 0.05,
        "a 5-day gap must decay more than a single flat 0.95 step \
             (got {}, single-step would be {expected_single_step})",
        after.weight
    );
}

#[test]
fn test_apply_decay_caps_access_count_amplification() {
    // Audit #185 H7: the pre-fix decay formula had an uncapped
    // `1 + access_count * 0.1` term, which let memories with 100+
    // accesses become effectively decay-immune. Repro the gaming
    // and assert the capped formula prevents it.
    //
    // After 5 decay rounds at factor=0.8:
    // - real (access=0): naively 0.95 ^ 5 ≈ 0.77 → with importance
    //   medium and the capped slowdown, weight should drop to
    //   roughly 0.5×.
    // - junk (access=100): pre-fix it stayed near 0.95 (no decay
    //   amplification immune); post-fix it's capped at 5 accesses
    //   so it decays at the same rate as a memory with 5 accesses.
    // The exact ratio depends on the cap; the crucial property is
    // that after enough decay rounds, junk's weight no longer
    // exceeds real's weight. We assert that property directly
    // rather than pinning specific weight numbers.
    let store = test_store();

    let mut real = make_memory("topic", "real high-importance fact");
    real.importance = Importance::Medium;
    let real_id = store.store(real).unwrap();

    let mut junk = make_memory("topic", "junk fact accessed by gaming loop");
    junk.importance = Importance::Medium;
    let junk_id = store.store(junk).unwrap();

    // Inflate junk's access_count to 100 (the M01 reproduction).
    store
        .conn
        .execute(
            "UPDATE memories SET access_count = 100 WHERE id = ?1",
            params![junk_id],
        )
        .unwrap();

    // 5 aggressive decay rounds.
    for _ in 0..5 {
        store.apply_decay(0.8).unwrap();
    }

    let real_after = store.get(&real_id).unwrap().unwrap();
    let junk_after = store.get(&junk_id).unwrap().unwrap();

    // Pre-fix: junk weight ≈ 0.95, real weight ≈ 0.31 → junk dominates.
    // Post-fix: with the cap, junk decays meaningfully even at
    // access=100. We require junk weight < real weight + a small
    // headroom — the cap must not let a low-relevance, frequently-
    // accessed memory overtake real same-importance memories.
    assert!(
        junk_after.weight < real_after.weight * 1.6,
        "junk weight {} must not dominate real weight {} after 5 decay rounds (cap is broken)",
        junk_after.weight,
        real_after.weight,
    );

    // Sanity: junk did still decay measurably (cap didn't make it
    // permanent).
    assert!(
        junk_after.weight < 0.97,
        "junk weight {} barely decayed at all (cap is too aggressive a slowdown)",
        junk_after.weight,
    );
}

#[test]
fn test_prune() {
    let store = test_store();
    let mut low = make_memory("test", "low weight");
    low.weight = 0.05;
    store.store(low).unwrap();

    store.store(make_memory("test", "normal weight")).unwrap();

    let pruned = store.prune(0.1).unwrap();
    assert_eq!(pruned, 1);
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn test_stats() {
    let store = test_store();
    store.store(make_memory("a", "first")).unwrap();
    store.store(make_memory("b", "second")).unwrap();

    let stats = store.stats().unwrap();
    assert_eq!(stats.total_memories, 2);
    assert_eq!(stats.total_topics, 2);
    assert!(stats.avg_weight > 0.0);
    assert!(stats.oldest_memory.is_some());
    assert!(stats.newest_memory.is_some());
}

#[test]
fn test_update_access() {
    let store = test_store();
    let mem = make_memory("test", "access test");
    let id = mem.id.clone();
    store.store(mem).unwrap();

    store.update_access(&id).unwrap();
    let retrieved = store.get(&id).unwrap().unwrap();
    assert_eq!(retrieved.access_count, 1);
}

#[test]
fn test_consolidate_topic() {
    let store = test_store();
    store.store(make_memory("topic-a", "entry 1")).unwrap();
    store.store(make_memory("topic-a", "entry 2")).unwrap();
    store.store(make_memory("topic-b", "other")).unwrap();

    let consolidated = make_memory("topic-a", "consolidated summary");
    store.consolidate_topic("topic-a", consolidated).unwrap();

    let memories = store.get_by_topic("topic-a").unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].summary, "consolidated summary");

    // topic-b should be untouched
    assert_eq!(store.get_by_topic("topic-b").unwrap().len(), 1);
}

/// Audit regression: consolidation must honor the "critical = never
/// forget" contract that `apply_decay` and `prune` already respect.
#[test]
fn consolidate_topic_preserves_critical_memories() {
    let store = test_store();
    store.store(make_memory("t", "expendable 1")).unwrap();
    store.store(make_memory("t", "expendable 2")).unwrap();
    store
        .store(Memory::new(
            "t".into(),
            "never forget this".into(),
            Importance::Critical,
        ))
        .unwrap();

    store
        .consolidate_topic("t", make_memory("t", "rollup"))
        .unwrap();

    let after = store.get_by_topic("t").unwrap();
    let summaries: Vec<&str> = after.iter().map(|m| m.summary.as_str()).collect();
    assert_eq!(after.len(), 2, "critical + consolidated must both survive");
    assert!(summaries.contains(&"never forget this"));
    assert!(summaries.contains(&"rollup"));
    assert!(!summaries.contains(&"expendable 1"));
}

/// Manual-testing finding: consolidate_topic bulk-deletes the topic's
/// non-critical memories and does its own DELETE, entirely separate
/// from the single-id `delete()` — so it had the same dangling
/// related_ids bug in a second place. An external memory (in a
/// different topic) that referenced one of the consolidated-away ids
/// must have that reference cleaned up too.
#[test]
fn consolidate_topic_cleans_up_dangling_related_ids_in_other_memories() {
    let store = test_store();
    let a_id = store.store(make_memory("t", "memory a")).unwrap();
    let b_id = store.store(make_memory("t", "memory b")).unwrap();

    let mut external = make_memory("other-topic", "external memory");
    external.related_ids = vec![a_id.clone(), b_id.clone()];
    let external_id = store.store(external).unwrap();

    store
        .consolidate_topic("t", make_memory("t", "rollup"))
        .unwrap();

    let external_after = store.get(&external_id).unwrap().unwrap();
    assert!(
        external_after.related_ids.is_empty(),
        "external memory must no longer reference the consolidated-away ids: {:?}",
        external_after.related_ids
    );
}

/// Audit regression: critical memories are exempt from consolidation, so
/// they must not count toward the auto-consolidate threshold — otherwise
/// a topic full of criticals would churn a fresh rollup on every store.
#[test]
fn auto_consolidate_ignores_critical_for_threshold() {
    let store = test_store();
    for i in 0..3 {
        store
            .store(Memory::new(
                "t".into(),
                format!("critical {i}"),
                Importance::Critical,
            ))
            .unwrap();
    }
    store.store(make_memory("t", "one expendable")).unwrap();

    // 4 total but only 1 consolidatable — below threshold 3.
    assert!(!store.auto_consolidate("t", 3).unwrap());
    assert_eq!(store.get_by_topic("t").unwrap().len(), 4);

    store.store(make_memory("t", "expendable 2")).unwrap();
    store.store(make_memory("t", "expendable 3")).unwrap();

    // Now 3 consolidatable — rollup fires, criticals survive.
    assert!(store.auto_consolidate("t", 3).unwrap());
    let after = store.get_by_topic("t").unwrap();
    let criticals = after
        .iter()
        .filter(|m| matches!(m.importance, Importance::Critical))
        .count();
    assert_eq!(criticals, 3, "all criticals must survive the rollup");
    assert_eq!(after.len(), 4, "3 criticals + 1 consolidated");
}

/// Audit regression: `update()` previously bypassed all validation, so
/// oversized or NUL-carrying payloads could enter via store-small-then-
/// update-big.
#[test]
fn update_rejects_oversized_and_nul_payloads() {
    let store = test_store();
    let id = store.store(make_memory("t", "small")).unwrap();
    let mut m = store.get(&id).unwrap().unwrap();

    m.summary = "x".repeat(MAX_SUMMARY_BYTES + 1);
    assert!(matches!(store.update(&m), Err(IcmError::InvalidInput(_))));

    m.summary = "has a \0 NUL".into();
    assert!(matches!(store.update(&m), Err(IcmError::InvalidInput(_))));

    // The stored row is untouched by the rejected updates.
    assert_eq!(store.get(&id).unwrap().unwrap().summary, "small");
}

/// Audit regression: the MCP consolidate path passes a caller-provided
/// summary that previously bypassed every size check.
#[test]
fn consolidate_topic_validates_consolidated_summary() {
    let store = test_store();
    store.store(make_memory("t", "entry")).unwrap();

    let oversized = make_memory("t", &"x".repeat(MAX_SUMMARY_BYTES + 1));
    assert!(matches!(
        store.consolidate_topic("t", oversized),
        Err(IcmError::InvalidInput(_))
    ));
    // Originals untouched on rejection.
    assert_eq!(store.get_by_topic("t").unwrap().len(), 1);
}

/// Audit regression: transcript messages had no size bound at all; they
/// are best-effort logs, so oversized content is truncated, not lost.
#[test]
fn record_message_truncates_oversized_content() {
    let store = test_store();
    let sid = store.create_session("test-agent", None, None).unwrap();
    let big = "é".repeat(200 * 1024); // 400 KB of two-byte chars
    store
        .record_message(&sid, Role::User, &big, None, None, None)
        .unwrap();

    let msgs = store.list_session_messages(&sid, 10, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].content.len() <= 256 * 1024);
    assert!(!msgs[0].content.is_empty());
    // Truncation must respect char boundaries (no broken UTF-8).
    assert!(msgs[0].content.chars().all(|c| c == 'é'));
}

/// Reproduces issue #44: after consolidation, recall should only return the
/// consolidated memory — not stale fragments from the originals.
#[test]
fn test_consolidate_no_stale_fts_results() {
    let store = test_store();

    // Step 1: store 3 related memories on the same topic
    store
        .store(make_memory(
            "errors-resolved",
            "fix: null pointer in parser",
        ))
        .unwrap();
    store
        .store(make_memory(
            "errors-resolved",
            "fix: timeout in HTTP client",
        ))
        .unwrap();
    store
        .store(make_memory(
            "errors-resolved",
            "fix: race condition in cache",
        ))
        .unwrap();

    // Verify FTS finds them before consolidation
    let before = store.search_fts("fix", 10).unwrap();
    assert_eq!(before.len(), 3);

    // Step 2: consolidate
    let consolidated = make_memory(
        "errors-resolved",
        "All errors resolved: parser, HTTP, cache",
    );
    store
        .consolidate_topic("errors-resolved", consolidated)
        .unwrap();

    // Step 3: recall — should only return the consolidated memory
    let after = store.search_fts("fix", 10).unwrap();
    assert!(
        after.len() <= 1,
        "expected at most 1 result after consolidation, got {}",
        after.len()
    );

    // The consolidated memory should be findable
    let consolidated_results = store.search_fts("errors resolved parser", 10).unwrap();
    assert_eq!(consolidated_results.len(), 1);
    assert!(consolidated_results[0]
        .summary
        .contains("All errors resolved"));

    // Verify topic has exactly 1 memory
    let topic_mems = store.get_by_topic("errors-resolved").unwrap();
    assert_eq!(topic_mems.len(), 1);
}

// === MemoirStore tests ===

#[test]
fn test_memoir_crud() {
    let store = test_store();
    let m = make_memoir("my-project");
    let id = store.create_memoir(m).unwrap();

    let retrieved = store.get_memoir(&id).unwrap().unwrap();
    assert_eq!(retrieved.name, "my-project");

    let by_name = store.get_memoir_by_name("my-project").unwrap().unwrap();
    assert_eq!(by_name.id, id);

    store.delete_memoir(&id).unwrap();
    assert!(store.get_memoir(&id).unwrap().is_none());
}

#[test]
fn test_memoir_unique_name() {
    let store = test_store();
    store.create_memoir(make_memoir("dup")).unwrap();
    let result = store.create_memoir(make_memoir("dup"));
    assert!(result.is_err());
}

#[test]
fn test_list_memoirs() {
    let store = test_store();
    store.create_memoir(make_memoir("beta")).unwrap();
    store.create_memoir(make_memoir("alpha")).unwrap();

    let list = store.list_memoirs().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].name, "alpha"); // sorted by name
    assert_eq!(list[1].name, "beta");
}

#[test]
fn test_concept_crud() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();

    let mut c = make_concept(&m_id, "event-sourcing", "Events stored in SQLite");
    c.labels = vec![Label::new("domain", "arch"), Label::new("type", "decision")];
    let c_id = store.add_concept(c).unwrap();

    let retrieved = store.get_concept(&c_id).unwrap().unwrap();
    assert_eq!(retrieved.name, "event-sourcing");
    assert_eq!(retrieved.labels.len(), 2);

    let by_name = store
        .get_concept_by_name(&m_id, "event-sourcing")
        .unwrap()
        .unwrap();
    assert_eq!(by_name.id, c_id);

    store.delete_concept(&c_id).unwrap();
    assert!(store.get_concept(&c_id).unwrap().is_none());
}

#[test]
fn test_concept_unique_within_memoir() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();

    store
        .add_concept(make_concept(&m_id, "dup", "first"))
        .unwrap();
    let result = store.add_concept(make_concept(&m_id, "dup", "second"));
    assert!(result.is_err());
}

#[test]
fn test_concept_same_name_different_memoirs() {
    let store = test_store();
    let m1 = store.create_memoir(make_memoir("proj1")).unwrap();
    let m2 = store.create_memoir(make_memoir("proj2")).unwrap();

    store
        .add_concept(make_concept(&m1, "sqlite", "def1"))
        .unwrap();
    store
        .add_concept(make_concept(&m2, "sqlite", "def2"))
        .unwrap();

    let c1 = store.get_concept_by_name(&m1, "sqlite").unwrap().unwrap();
    let c2 = store.get_concept_by_name(&m2, "sqlite").unwrap().unwrap();
    assert_ne!(c1.id, c2.id);
}

#[test]
fn test_refine_concept() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let c_id = store
        .add_concept(make_concept(&m_id, "es", "Events v1"))
        .unwrap();

    let orig = store.get_concept(&c_id).unwrap().unwrap();
    assert_eq!(orig.revision, 1);
    let orig_confidence = orig.confidence;

    store
        .refine_concept(&c_id, "Events v2 with snapshots", &["mem-1".into()])
        .unwrap();

    let refined = store.get_concept(&c_id).unwrap().unwrap();
    assert_eq!(refined.revision, 2);
    assert_eq!(refined.definition, "Events v2 with snapshots");
    assert!(refined.confidence > orig_confidence);
    assert!(refined.source_memory_ids.contains(&"mem-1".into()));
}

#[test]
fn test_concept_links() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let c1_id = store
        .add_concept(make_concept(&m_id, "event-sourcing", "ES pattern"))
        .unwrap();
    let c2_id = store
        .add_concept(make_concept(&m_id, "sqlite", "SQLite storage"))
        .unwrap();

    let link = ConceptLink::new(c1_id.clone(), c2_id.clone(), Relation::DependsOn);
    let link_id = store.add_link(link).unwrap();

    let from = store.get_links_from(&c1_id).unwrap();
    assert_eq!(from.len(), 1);
    assert_eq!(from[0].target_id, c2_id);
    assert_eq!(from[0].relation, Relation::DependsOn);

    let to = store.get_links_to(&c2_id).unwrap();
    assert_eq!(to.len(), 1);
    assert_eq!(to[0].source_id, c1_id);

    store.delete_link(&link_id).unwrap();
    assert!(store.get_links_from(&c1_id).unwrap().is_empty());
}

#[test]
fn test_self_link_rejected() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let c_id = store
        .add_concept(make_concept(&m_id, "concept", "def"))
        .unwrap();

    let link = ConceptLink::new(c_id.clone(), c_id, Relation::RelatedTo);
    let result = store.add_link(link);
    assert!(result.is_err());
}

#[test]
fn test_transitive_cycle_rejected() {
    // Audit M11/CYC1: A → B → C → A used to be silently accepted,
    // corrupting BFS in `get_neighborhood`. Now the third edge
    // (closing the cycle) is rejected with `InvalidInput`.
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let a = store.add_concept(make_concept(&m_id, "A", "a")).unwrap();
    let b = store.add_concept(make_concept(&m_id, "B", "b")).unwrap();
    let c = store.add_concept(make_concept(&m_id, "C", "c")).unwrap();

    // A → B: ok
    store
        .add_link(ConceptLink::new(a.clone(), b.clone(), Relation::DependsOn))
        .unwrap();
    // B → C: ok
    store
        .add_link(ConceptLink::new(b.clone(), c.clone(), Relation::Refines))
        .unwrap();
    // C → A: would close the cycle — reject
    let cycle_attempt = store.add_link(ConceptLink::new(c, a, Relation::RelatedTo));
    assert!(
        cycle_attempt.is_err(),
        "C → A should be rejected as a cycle"
    );
    let err_msg = cycle_attempt.unwrap_err().to_string();
    assert!(
        err_msg.contains("cycle"),
        "error message should mention cycle: {err_msg}"
    );
}

#[test]
fn test_dag_links_still_allowed() {
    // Sanity: rejecting cycles must not break legitimate DAG links.
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let a = store.add_concept(make_concept(&m_id, "A", "a")).unwrap();
    let b = store.add_concept(make_concept(&m_id, "B", "b")).unwrap();
    let c = store.add_concept(make_concept(&m_id, "C", "c")).unwrap();

    // A → B, A → C, B → C — three edges in a DAG, all should pass.
    store
        .add_link(ConceptLink::new(a.clone(), b.clone(), Relation::DependsOn))
        .unwrap();
    store
        .add_link(ConceptLink::new(a, c.clone(), Relation::DependsOn))
        .unwrap();
    store
        .add_link(ConceptLink::new(b, c, Relation::Refines))
        .unwrap();
}

#[test]
fn test_get_neighbors() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let c1 = store
        .add_concept(make_concept(&m_id, "a", "node a"))
        .unwrap();
    let c2 = store
        .add_concept(make_concept(&m_id, "b", "node b"))
        .unwrap();
    let c3 = store
        .add_concept(make_concept(&m_id, "c", "node c"))
        .unwrap();

    store
        .add_link(ConceptLink::new(
            c1.clone(),
            c2.clone(),
            Relation::DependsOn,
        ))
        .unwrap();
    store
        .add_link(ConceptLink::new(c3.clone(), c1.clone(), Relation::PartOf))
        .unwrap();

    let neighbors = store.get_neighbors(&c1, None).unwrap();
    assert_eq!(neighbors.len(), 2);

    let dep_neighbors = store.get_neighbors(&c1, Some(Relation::DependsOn)).unwrap();
    assert_eq!(dep_neighbors.len(), 1);
    assert_eq!(dep_neighbors[0].name, "b");
}

#[test]
fn test_get_neighborhood_bfs() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let c1 = store
        .add_concept(make_concept(&m_id, "a", "node a"))
        .unwrap();
    let c2 = store
        .add_concept(make_concept(&m_id, "b", "node b"))
        .unwrap();
    let c3 = store
        .add_concept(make_concept(&m_id, "c", "node c"))
        .unwrap();
    let c4 = store
        .add_concept(make_concept(&m_id, "d", "node d"))
        .unwrap();

    // a -> b -> c -> d
    store
        .add_link(ConceptLink::new(
            c1.clone(),
            c2.clone(),
            Relation::DependsOn,
        ))
        .unwrap();
    store
        .add_link(ConceptLink::new(
            c2.clone(),
            c3.clone(),
            Relation::DependsOn,
        ))
        .unwrap();
    store
        .add_link(ConceptLink::new(c3, c4, Relation::DependsOn))
        .unwrap();

    // depth=1 should get a + b
    let (concepts, links) = store.get_neighborhood(&c1, 1).unwrap();
    assert_eq!(concepts.len(), 2);
    assert!(!links.is_empty());

    // depth=2 should get a + b + c
    let (concepts, _) = store.get_neighborhood(&c1, 2).unwrap();
    assert_eq!(concepts.len(), 3);

    // depth=3 should get all 4
    let (concepts, _) = store.get_neighborhood(&c1, 3).unwrap();
    assert_eq!(concepts.len(), 4);
}

#[test]
fn test_cascade_delete_memoir() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();
    let c1 = store.add_concept(make_concept(&m_id, "a", "def")).unwrap();
    let c2 = store.add_concept(make_concept(&m_id, "b", "def")).unwrap();
    store
        .add_link(ConceptLink::new(c1, c2, Relation::RelatedTo))
        .unwrap();

    store.delete_memoir(&m_id).unwrap();

    // Concepts and links should be gone
    let concepts = store.list_concepts(&m_id).unwrap();
    assert!(concepts.is_empty());
}

#[test]
fn test_memoir_stats() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();

    let mut c = make_concept(&m_id, "es", "event sourcing");
    c.labels = vec![Label::new("domain", "arch")];
    let c1 = store.add_concept(c).unwrap();

    let mut c = make_concept(&m_id, "sqlite", "sqlite storage");
    c.labels = vec![Label::new("domain", "arch"), Label::new("type", "tech")];
    let c2 = store.add_concept(c).unwrap();

    store
        .add_link(ConceptLink::new(c1, c2, Relation::DependsOn))
        .unwrap();

    let stats = store.memoir_stats(&m_id).unwrap();
    assert_eq!(stats.total_concepts, 2);
    assert_eq!(stats.total_links, 1);
    assert!(stats.avg_confidence > 0.0);
    assert!(!stats.label_counts.is_empty());
}

#[test]
fn test_search_concepts_fts() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();

    store
        .add_concept(make_concept(
            &m_id,
            "event-sourcing",
            "Store domain events in append-only log",
        ))
        .unwrap();
    store
        .add_concept(make_concept(
            &m_id,
            "cqrs",
            "Command Query Responsibility Segregation",
        ))
        .unwrap();

    let results = store.search_concepts_fts(&m_id, "events", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "event-sourcing");
}

#[test]
fn test_search_concepts_by_label() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();

    let mut c1 = make_concept(&m_id, "es", "event sourcing");
    c1.labels = vec![Label::new("domain", "arch")];
    store.add_concept(c1).unwrap();

    let mut c2 = make_concept(&m_id, "sqlite", "storage");
    c2.labels = vec![Label::new("domain", "tech")];
    store.add_concept(c2).unwrap();

    let results = store
        .search_concepts_by_label(&m_id, &Label::new("domain", "arch"), 10)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "es");
}

/// Audit regression: the label pattern built by `search_concepts_by_label`
/// interpolated `namespace`/`value` into a LIKE pattern unescaped, so a
/// literal `_` in a search value acted as a SQL "any single char"
/// wildcard instead of matching only that exact character.
#[test]
fn test_search_concepts_by_label_escapes_wildcards() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();

    let mut c1 = make_concept(&m_id, "c1", "def1");
    c1.labels = vec![Label::new("domain", "test")];
    store.add_concept(c1).unwrap();

    let mut c2 = make_concept(&m_id, "c2", "def2");
    c2.labels = vec![Label::new("domain", "text")];
    store.add_concept(c2).unwrap();

    // "te_t" is not the literal value of either concept, but with an
    // unescaped `_` it matches both "test" and "text" as a wildcard.
    let results = store
        .search_concepts_by_label(&m_id, &Label::new("domain", "te_t"), 10)
        .unwrap();
    assert!(
        results.is_empty(),
        "unescaped '_' wildcard matched unrelated label values: {results:?}"
    );
}

// === Vector search tests ===

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

#[test]
fn test_sanitize_fts_query() {
    // Normal words get quoted
    assert_eq!(sanitize_fts_query("hello world"), "\"hello\" \"world\"");

    // Special chars become spaces, splitting into separate tokens
    assert_eq!(sanitize_fts_query("sqlite-vec"), "\"sqlite\" \"vec\"");
    assert_eq!(sanitize_fts_query("foo*bar"), "\"foo\" \"bar\"");
    assert_eq!(sanitize_fts_query("col:value"), "\"col\" \"value\"");

    // Empty/whitespace returns empty
    assert_eq!(sanitize_fts_query(""), "");
    assert_eq!(sanitize_fts_query("  "), "");
    assert_eq!(sanitize_fts_query("---"), "");

    // Mixed content
    assert_eq!(
        sanitize_fts_query("no-such column:vec"),
        "\"no\" \"such\" \"column\" \"vec\""
    );
}

#[test]
fn test_search_fts_special_chars() {
    let store = test_store();
    store
        .store(make_memory(
            "tools",
            "sqlite-vec is a vector search extension",
        ))
        .unwrap();

    // This query used to crash with "no such column: vec"
    let results = store.search_fts("sqlite-vec", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].topic, "tools");

    // Pure special chars should return empty, not error
    let results = store.search_fts("---", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_search_concepts_fts_special_chars() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("proj")).unwrap();

    store
        .add_concept(make_concept(
            &m_id,
            "sqlite-vec",
            "Vector search extension for SQLite",
        ))
        .unwrap();

    // Should not crash with special chars in query
    let results = store.search_concepts_fts(&m_id, "sqlite-vec", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "sqlite-vec");

    // Pure special chars should return empty
    let results = store.search_concepts_fts(&m_id, "***", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_sql_injection_in_topic() {
    let store = test_store();
    let mem = make_memory("'; DROP TABLE memories; --", "should be safe");
    store.store(mem.clone()).unwrap();

    let retrieved = store.get(&mem.id).unwrap().unwrap();
    assert_eq!(retrieved.topic, "'; DROP TABLE memories; --");
    assert_eq!(store.count().unwrap(), 1);
    let topics = store.list_topics().unwrap();
    assert_eq!(topics.len(), 1);
}

#[test]
fn test_sql_injection_in_summary() {
    let store = test_store();
    let mem = make_memory("test", "value'); DELETE FROM memories WHERE ('1'='1");
    store.store(mem).unwrap();
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn test_sql_injection_in_fts_query() {
    let store = test_store();
    store
        .store(make_memory("test", "normal content here"))
        .unwrap();

    // FTS5 injection attempts
    let results = store.search_fts("') OR 1=1 --", 10).unwrap();
    assert!(results.is_empty() || results.len() <= 1);

    let results = store.search_fts("NEAR(a b)", 10).unwrap();
    let _ = results;
}

#[test]
fn test_sql_injection_in_keywords() {
    let store = test_store();
    let mut mem = make_memory("test", "keyword injection");
    mem.keywords = vec!["normal".into(), "'; DROP TABLE memories; --".into()];
    store.store(mem).unwrap();
    assert_eq!(store.count().unwrap(), 1);

    let results = store
        .search_by_keywords(&["'; DROP TABLE memories; --"], 10)
        .unwrap();
    let _ = results;
}

#[test]
fn test_null_bytes_in_summary_rejected() {
    // Audit finding: libsql binds text via NUL-terminated C strings,
    // so anything past the first `\0` was silently dropped — a
    // memory written as `"before\0after"` came back as `"before"`.
    // We now reject the write so callers know their data isn't
    // round-tripping intact.
    let store = test_store();
    let mem = make_memory("test", "before\0after");
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("NUL")),
        "expected InvalidInput(NUL...) got {err:?}"
    );
}

#[test]
fn test_null_bytes_in_topic_rejected() {
    let store = test_store();
    let mem = make_memory("topic\0fake", "real summary content");
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("NUL")),
        "expected InvalidInput(NUL...) got {err:?}"
    );
}

#[test]
fn test_unicode_topic_with_trailing_null_rejected() {
    // The previous permissive behaviour stored the topic
    // `\u{1F600}\u{1F4A9}\u{0000}` and round-tripped only the
    // pre-NUL prefix. Now we reject so callers don't think they
    // stored what they passed.
    let store = test_store();
    let unicode_topic = "\u{1F600}\u{1F4A9}\u{0000}";
    let mem = make_memory(unicode_topic, "emoji topic content here");
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("NUL")),
        "expected NUL rejection on emoji+NUL topic, got {err:?}"
    );
}

#[test]
fn test_unicode_emoji_topic_without_null_accepted() {
    // Sanity: legitimate emoji topics should still work.
    let store = test_store();
    let mem = make_memory("\u{1F525}-decisions", "real content here please");
    let id = store.store(mem.clone()).unwrap();
    let retrieved = store.get(&id).unwrap().unwrap();
    assert!(retrieved.topic.starts_with('\u{1F525}'));
}

#[test]
fn test_summary_within_cap_accepted() {
    let store = test_store();
    let summary = "a".repeat(60_000);
    let mem = make_memory("test", &summary);
    store.store(mem.clone()).unwrap();
    let retrieved = store.get(&mem.id).unwrap().unwrap();
    assert_eq!(retrieved.summary.len(), 60_000);
}

#[test]
fn test_summary_exceeding_cap_rejected() {
    // Audit finding: a 1 MB single text block landed verbatim as a
    // single memory's summary, blowing up DB size and embedding
    // compute. Cap at 64 KB.
    let store = test_store();
    let long_summary = "a".repeat(100_000);
    let mem = make_memory("test", &long_summary);
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("summary exceeds")),
        "expected summary-size rejection got {err:?}"
    );
}

#[test]
fn test_empty_topic_rejected() {
    let store = test_store();
    let mem = make_memory("", "real summary content here");
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("topic cannot be empty")),
        "expected empty-topic rejection got {err:?}"
    );
}

#[test]
fn test_whitespace_only_topic_rejected() {
    let store = test_store();
    let mem = make_memory("   \t  ", "real summary content here");
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("topic cannot be empty")),
        "expected empty-after-trim rejection got {err:?}"
    );
}

#[test]
fn test_empty_summary_rejected() {
    let store = test_store();
    let mem = make_memory("topic", "");
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("summary cannot be empty")),
        "expected empty-summary rejection got {err:?}"
    );
}

#[test]
fn test_topic_with_newline_rejected() {
    let store = test_store();
    let mem = make_memory("topic\nfake-topic", "real summary content here");
    let err = store.store(mem).unwrap_err();
    assert!(
        matches!(err, IcmError::InvalidInput(ref m) if m.contains("newline")),
        "expected newline rejection got {err:?}"
    );
}

#[test]
fn test_topic_trailing_whitespace_trimmed_on_store() {
    // Two topics that visually look identical (`"trail "` vs
    // `"trail"`) should land in the same bucket. We trim on the
    // way in.
    let store = test_store();
    let id1 = store
        .store(make_memory("  trail  ", "summary one content"))
        .unwrap();
    let mem1 = store.get(&id1).unwrap().unwrap();
    assert_eq!(mem1.topic, "trail", "topic should be trimmed");
}

#[test]
fn test_bulk_insert_100() {
    let store = test_store();
    for i in 0..100 {
        store
            .store(make_memory("bulk", &format!("memory number {i}")))
            .unwrap();
    }
    assert_eq!(store.count().unwrap(), 100);
    let by_topic = store.get_by_topic("bulk").unwrap();
    assert_eq!(by_topic.len(), 100);
}

#[test]
fn test_fts_search_many_entries() {
    let store = test_store();
    for i in 0..50 {
        store
            .store(make_memory(
                "lang",
                &format!("programming language number {i}"),
            ))
            .unwrap();
    }
    store
        .store(make_memory(
            "unique",
            "Rust is a memory-safe systems language",
        ))
        .unwrap();

    let results = store.search_fts("memory-safe systems", 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].topic, "unique");
}

#[test]
fn test_decay_bulk() {
    let store = test_store();
    for i in 0..50 {
        let mut mem = make_memory("decay", &format!("entry {i}"));
        if i % 5 == 0 {
            mem.importance = Importance::Critical;
        }
        store.store(mem).unwrap();
    }
    // 10 critical, 40 non-critical
    let affected = store.apply_decay(0.9).unwrap();
    assert_eq!(affected, 40);
}

#[test]
fn test_prune_leaves_important() {
    let store = test_store();
    for i in 0..20 {
        let mut mem = make_memory("prune", &format!("entry {i}"));
        mem.weight = if i < 10 { 0.01 } else { 0.5 };
        store.store(mem).unwrap();
    }
    let pruned = store.prune(0.1).unwrap();
    assert_eq!(pruned, 10);
    assert_eq!(store.count().unwrap(), 10);
}

#[test]
fn test_many_topics_listing() {
    let store = test_store();
    for i in 0..30 {
        store
            .store(make_memory(&format!("topic-{i}"), &format!("content {i}")))
            .unwrap();
    }
    let topics = store.list_topics().unwrap();
    assert_eq!(topics.len(), 30);
}

#[test]
fn test_consolidate_large_topic() {
    let store = test_store();
    for i in 0..25 {
        store
            .store(make_memory("big-topic", &format!("detail {i}")))
            .unwrap();
    }
    let consolidated = make_memory("big-topic", "consolidated summary of 25 entries");
    store.consolidate_topic("big-topic", consolidated).unwrap();
    let remaining = store.get_by_topic("big-topic").unwrap();
    assert_eq!(remaining.len(), 1);
    assert!(remaining[0].summary.contains("consolidated"));
}

#[test]
fn test_get_by_topic_returns_sorted_by_weight() {
    let store = test_store();
    let mut low = make_memory("ux", "low weight");
    low.weight = 0.3;
    store.store(low).unwrap();

    let mut high = make_memory("ux", "high weight");
    high.weight = 0.9;
    store.store(high).unwrap();

    let results = store.get_by_topic("ux").unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].weight >= results[1].weight);
}

#[test]
fn test_update_access_increments_correctly() {
    let store = test_store();
    let mem = make_memory("ux", "access counter");
    let id = mem.id.clone();
    store.store(mem).unwrap();

    for _ in 0..5 {
        store.update_access(&id).unwrap();
    }
    let retrieved = store.get(&id).unwrap().unwrap();
    assert_eq!(retrieved.access_count, 5);
}

#[test]
fn test_stats_on_empty_store() {
    let store = test_store();
    let stats = store.stats().unwrap();
    assert_eq!(stats.total_memories, 0);
    assert_eq!(stats.total_topics, 0);
    assert_eq!(stats.avg_weight, 0.0);
    assert!(stats.oldest_memory.is_none());
    assert!(stats.newest_memory.is_none());
}

#[test]
fn test_double_delete_returns_not_found() {
    let store = test_store();
    let mem = make_memory("ux", "delete twice");
    let id = mem.id.clone();
    store.store(mem).unwrap();

    store.delete(&id).unwrap();
    let result = store.delete(&id);
    assert!(matches!(result, Err(IcmError::NotFound(_))));
}

#[test]
fn test_update_syncs_embedding() {
    let store = test_store();
    let mut mem = make_memory("test", "before update");
    let id = mem.id.clone();
    store.store(mem.clone()).unwrap();

    // Initially no embedding
    assert!(store.get(&id).unwrap().unwrap().embedding.is_none());

    // Update with embedding
    mem.embedding = Some(vec![0.3; 384]);
    store.update(&mem).unwrap();

    let retrieved = store.get(&id).unwrap().unwrap();
    assert!(retrieved.embedding.is_some());

    // Should be findable via vector search
    let results = store.search_by_embedding(&vec![0.3; 384], 5).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0.id, id);
}

#[test]
fn perf_store_1000() {
    let store = test_store();
    let start = std::time::Instant::now();
    for i in 0..1000 {
        store
            .store(make_memory("perf", &format!("memory number {i}")))
            .unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2000,
        "1000 stores took {}ms (max 2000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_store_with_embeddings_1000() {
    let store = test_store();
    let start = std::time::Instant::now();
    for i in 0..1000 {
        let mut mem = make_memory("perf", &format!("embedded memory {i}"));
        mem.embedding = Some(vec![0.1; 384]);
        store.store(mem).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 3000,
        "1000 stores+embedding took {}ms (max 3000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_fts_search_100() {
    let store = test_store();
    for i in 0..500 {
        store
            .store(make_memory(
                "lang",
                &format!("programming language {i} with features"),
            ))
            .unwrap();
    }
    let start = std::time::Instant::now();
    for _ in 0..100 {
        store
            .search_fts("programming language features", 10)
            .unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 1000,
        "100 FTS searches took {}ms (max 1000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_vector_search_100() {
    let store = test_store();
    for i in 0..500 {
        let mut mem = make_memory("vec", &format!("vector memory {i}"));
        let mut emb = vec![0.0; 384];
        emb[i % 384] = 1.0;
        mem.embedding = Some(emb);
        store.store(mem).unwrap();
    }
    let query = vec![0.5; 384];
    let start = std::time::Instant::now();
    for _ in 0..100 {
        store.search_by_embedding(&query, 10).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 5000,
        "100 vector searches took {}ms (max 5000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_hybrid_search_100() {
    let store = test_store();
    for i in 0..500 {
        let mut mem = make_memory("hybrid", &format!("hybrid searchable memory {i}"));
        mem.embedding = Some(vec![0.1; 384]);
        store.store(mem).unwrap();
    }
    let query_emb = vec![0.1; 384];
    let start = std::time::Instant::now();
    for _ in 0..100 {
        store
            .search_hybrid("hybrid searchable", &query_emb, 10)
            .unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 10000,
        "100 hybrid searches took {}ms (max 10000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_decay_1000() {
    let store = test_store();
    for i in 0..1000 {
        store
            .store(make_memory("decay", &format!("decayable {i}")))
            .unwrap();
    }
    let start = std::time::Instant::now();
    store.apply_decay(0.95).unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 500,
        "decay on 1000 memories took {}ms (max 500ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_get_by_id_1000() {
    let store = test_store();
    let mut ids = Vec::new();
    for i in 0..1000 {
        let mem = make_memory("get", &format!("lookup {i}"));
        let id = mem.id.clone();
        store.store(mem).unwrap();
        ids.push(id);
    }
    let start = std::time::Instant::now();
    for id in &ids {
        store.get(id).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 1000,
        "1000 gets took {}ms (max 1000ms)",
        elapsed.as_millis()
    );
}

/// Measure cache-hot vs cache-cold `get()` cost.
///
/// Run with `cargo test -p icm-store -- --ignored --nocapture
/// bench_cache_hit_vs_miss`. Informational only — no assertion.
#[test]
#[ignore]
fn bench_cache_hit_vs_miss() {
    let store = test_store();
    let mut ids: Vec<String> = Vec::new();
    for i in 0..50 {
        let mem = make_memory("bench", &format!("memory {i}"));
        ids.push(mem.id.clone());
        store.store(mem).unwrap();
    }

    // Cold: clear cache, read each id once. Mix of cache-fill + DB hit.
    store.cache_clear();
    let cold = std::time::Instant::now();
    for id in &ids {
        store.get(id).unwrap();
    }
    let cold_elapsed = cold.elapsed();

    // Warm: cache already populated by the cold pass; 1000 iterations
    // of the same id set are all cache hits.
    let warm = std::time::Instant::now();
    for _ in 0..1000 {
        for id in &ids {
            store.get(id).unwrap();
        }
    }
    let warm_elapsed = warm.elapsed();
    let warm_per_get_ns = warm_elapsed.as_nanos() / (1000 * ids.len() as u128);
    let cold_per_get_ns = cold_elapsed.as_nanos() / ids.len() as u128;

    eprintln!("=== bench_cache_hit_vs_miss ===");
    eprintln!("  cold (50 fills,   first read each): {cold_per_get_ns} ns/get");
    eprintln!("  warm (50000 hits, all cache reads): {warm_per_get_ns} ns/get");
    if warm_per_get_ns > 0 {
        eprintln!(
            "  speedup on hot reads: {:.1}x",
            cold_per_get_ns as f64 / warm_per_get_ns as f64
        );
    }
}

/// Measure batched `get_many` vs per-id `get` round-trips.
///
/// Run with `cargo test -p icm-store -- --ignored --nocapture
/// bench_get_many_vs_n_plus_one`. Informational only.
#[test]
#[ignore]
fn bench_get_many_vs_n_plus_one() {
    let store = test_store();
    let mut ids: Vec<String> = Vec::new();
    for i in 0..50 {
        let mem = make_memory("bench", &format!("entry {i}"));
        ids.push(mem.id.clone());
        store.store(mem).unwrap();
    }
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();

    // Cold batched fetch.
    store.cache_clear();
    let t = std::time::Instant::now();
    let got = store.get_many(&id_refs).unwrap();
    let batch_elapsed = t.elapsed();
    assert_eq!(got.len(), 50);

    // Cold N+1 fetch.
    store.cache_clear();
    let t = std::time::Instant::now();
    for id in &id_refs {
        store.get(id).unwrap();
    }
    let n_plus_one_elapsed = t.elapsed();

    eprintln!("=== bench_get_many_vs_n_plus_one (50 ids) ===");
    eprintln!("  batched get_many: {} µs", batch_elapsed.as_micros());
    eprintln!("  N+1 individual:   {} µs", n_plus_one_elapsed.as_micros());
    if batch_elapsed.as_micros() > 0 {
        eprintln!(
            "  speedup: {:.1}x",
            n_plus_one_elapsed.as_micros() as f64 / batch_elapsed.as_micros() as f64
        );
    }
}

// === Additional performance tests ===

#[test]
fn perf_search_fts_latency_with_1000_entries() {
    let store = test_store();
    for i in 0..1000 {
        store
                .store(make_memory(
                    &format!("topic-{}", i % 50),
                    &format!("detailed description about system component {i} with features and architecture"),
                ))
                .unwrap();
    }
    let start = std::time::Instant::now();
    for _ in 0..50 {
        let results = store
            .search_fts("system component architecture", 10)
            .unwrap();
        assert!(!results.is_empty());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2000,
        "50 FTS searches over 1000 entries took {}ms (max 2000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_sequential_store_operations_rapid() {
    let store = test_store();
    let start = std::time::Instant::now();
    // Simulate concurrent-like rapid sequential operations mixing stores, gets, searches
    for i in 0..500 {
        let mem = make_memory("rapid", &format!("rapid entry {i}"));
        let id = mem.id.clone();
        store.store(mem).unwrap();
        // Interleave reads
        if i % 5 == 0 {
            store.get(&id).unwrap();
        }
        // Interleave searches
        if i % 20 == 0 {
            store.search_fts("rapid entry", 5).unwrap();
        }
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 3000,
        "500 mixed store/get/search ops took {}ms (max 3000ms)",
        elapsed.as_millis()
    );
    assert_eq!(store.count().unwrap(), 500);
}

#[test]
fn perf_memoir_creation_and_concept_linking() {
    let store = test_store();
    let start = std::time::Instant::now();

    // Create 10 memoirs, each with 10 concepts and links between them
    for m in 0..10 {
        let m_id = store
            .create_memoir(make_memoir(&format!("perf-memoir-{m}")))
            .unwrap();
        let mut concept_ids = Vec::new();
        for c in 0..10 {
            let c_id = store
                .add_concept(make_concept(
                    &m_id,
                    &format!("concept-{m}-{c}"),
                    &format!("Definition for concept {c} in memoir {m}"),
                ))
                .unwrap();
            concept_ids.push(c_id);
        }
        // Link each concept to the next one (chain)
        for w in concept_ids.windows(2) {
            store
                .add_link(ConceptLink::new(
                    w[0].clone(),
                    w[1].clone(),
                    Relation::DependsOn,
                ))
                .unwrap();
        }
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 3000,
        "10 memoirs x 10 concepts + links took {}ms (max 3000ms)",
        elapsed.as_millis()
    );

    // Verify structure
    let memoirs = store.list_memoirs().unwrap();
    assert_eq!(memoirs.len(), 10);
}

#[test]
fn perf_neighborhood_bfs_large_graph() {
    let store = test_store();
    let m_id = store.create_memoir(make_memoir("large-graph")).unwrap();

    // Create a large graph: 50 concepts in a chain
    let mut concept_ids = Vec::new();
    for i in 0..50 {
        let c_id = store
            .add_concept(make_concept(
                &m_id,
                &format!("node-{i}"),
                &format!("Graph node number {i}"),
            ))
            .unwrap();
        concept_ids.push(c_id);
    }
    // Chain: 0->1->2->...->49
    for w in concept_ids.windows(2) {
        store
            .add_link(ConceptLink::new(
                w[0].clone(),
                w[1].clone(),
                Relation::DependsOn,
            ))
            .unwrap();
    }
    // Add some cross-links for complexity
    for i in (0..50).step_by(5) {
        if i + 10 < 50 {
            store
                .add_link(ConceptLink::new(
                    concept_ids[i].clone(),
                    concept_ids[i + 10].clone(),
                    Relation::RelatedTo,
                ))
                .unwrap();
        }
    }

    let start = std::time::Instant::now();
    // BFS traversal at various depths
    for depth in 1..=5 {
        let (concepts, links) = store.get_neighborhood(&concept_ids[0], depth).unwrap();
        assert!(!concepts.is_empty());
        assert!(!links.is_empty());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2000,
        "BFS traversals (depth 1-5) on 50-node graph took {}ms (max 2000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_embedding_storage_batch() {
    let store = test_store();
    let start = std::time::Instant::now();
    for i in 0..500 {
        let mut mem = make_memory("embed-perf", &format!("embedding batch entry {i}"));
        let mut emb = vec![0.0f32; 384];
        // Vary embeddings so they're not all identical
        emb[i % 384] = 1.0;
        emb[(i * 7) % 384] = 0.5;
        mem.embedding = Some(emb);
        store.store(mem).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 3000,
        "500 stores with embeddings took {}ms (max 3000ms)",
        elapsed.as_millis()
    );

    // Now search
    let query = vec![0.5f32; 384];
    let search_start = std::time::Instant::now();
    for _ in 0..50 {
        let results = store.search_by_embedding(&query, 10).unwrap();
        assert!(!results.is_empty());
    }
    let search_elapsed = search_start.elapsed();
    assert!(
        search_elapsed.as_millis() < 3000,
        "50 vector searches over 500 entries took {}ms (max 3000ms)",
        search_elapsed.as_millis()
    );
}

#[test]
fn perf_keyword_search_with_many_entries() {
    let store = test_store();
    for i in 0..1000 {
        let mut mem = make_memory(
            &format!("kw-topic-{}", i % 20),
            &format!("keyword searchable entry number {i}"),
        );
        mem.keywords = vec![
            format!("keyword-{}", i % 10),
            format!("category-{}", i % 5),
            "common".into(),
        ];
        store.store(mem).unwrap();
    }

    let start = std::time::Instant::now();
    for i in 0..50 {
        let results = store
            .search_by_keywords(&[&format!("keyword-{}", i % 10)], 10)
            .unwrap();
        assert!(!results.is_empty());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2000,
        "50 keyword searches over 1000 entries took {}ms (max 2000ms)",
        elapsed.as_millis()
    );
}

#[test]
fn perf_consolidate_large_topic_timing() {
    let store = test_store();
    for i in 0..100 {
        store
            .store(make_memory(
                "consolidate-perf",
                &format!("detail entry {i} with various information"),
            ))
            .unwrap();
    }
    let start = std::time::Instant::now();
    let consolidated = make_memory("consolidate-perf", "All 100 entries consolidated");
    store
        .consolidate_topic("consolidate-perf", consolidated)
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 1000,
        "Consolidating 100 entries took {}ms (max 1000ms)",
        elapsed.as_millis()
    );
    assert_eq!(store.get_by_topic("consolidate-perf").unwrap().len(), 1);
}

#[test]
fn perf_list_topics_many() {
    let store = test_store();
    // Create 200 distinct topics
    for i in 0..200 {
        store
            .store(make_memory(
                &format!("distinct-topic-{i}"),
                &format!("content for topic {i}"),
            ))
            .unwrap();
    }
    let start = std::time::Instant::now();
    for _ in 0..50 {
        let topics = store.list_topics().unwrap();
        assert_eq!(topics.len(), 200);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 1000,
        "50 list_topics calls over 200 topics took {}ms (max 1000ms)",
        elapsed.as_millis()
    );
}

// === FeedbackStore tests ===

fn make_feedback(topic: &str, context: &str, predicted: &str, corrected: &str) -> Feedback {
    Feedback::new(
        topic.into(),
        context.into(),
        predicted.into(),
        corrected.into(),
        None,
        "test".into(),
    )
}

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

#[test]
fn test_sanitize_fts_empty() {
    assert_eq!(sanitize_fts_query(""), "");
    assert_eq!(sanitize_fts_query("   "), "");
}

#[test]
fn test_sanitize_fts_special_chars() {
    // All FTS5 operators should be stripped
    assert_eq!(sanitize_fts_query("hello-world"), "\"hello\" \"world\"");
    assert_eq!(sanitize_fts_query("foo*bar"), "\"foo\" \"bar\"");
    assert_eq!(sanitize_fts_query("a:b"), "\"a\" \"b\"");
    assert_eq!(sanitize_fts_query("(test)"), "\"test\"");
    assert_eq!(sanitize_fts_query("x^y+z~w"), "\"x\" \"y\" \"z\" \"w\"");
}

#[test]
fn test_sanitize_fts_quotes_stripped() {
    // Embedded quotes must be removed before wrapping in quotes
    assert_eq!(sanitize_fts_query("say \"hello\""), "\"say\" \"hello\"");
}

#[test]
fn test_sanitize_fts_unicode() {
    assert_eq!(sanitize_fts_query("café résumé"), "\"café\" \"résumé\"");
    assert_eq!(sanitize_fts_query("日本語テスト"), "\"日本語テスト\"");
}

#[test]
fn test_sanitize_fts_long_input_truncated() {
    let long = "a ".repeat(6000); // 12000 chars
    let result = sanitize_fts_query(&long);
    // Input is truncated to 10_000 chars, then tokens are capped at 100
    let token_count = result.split_whitespace().count();
    assert!(token_count <= 100);
}

#[test]
fn test_sanitize_fts_many_tokens_capped() {
    // 200 tokens should be capped to 100
    let many_tokens: String = (0..200)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let result = sanitize_fts_query(&many_tokens);
    let token_count = result.split_whitespace().count();
    assert_eq!(token_count, 100);
}

// === search limit cap tests ===

#[test]
fn test_search_fts_limit_capped() {
    let store = test_store();
    // Store a memory so search has something to find
    store.store(make_memory("test", "hello world")).unwrap();

    // Even with a huge limit, it should not error (capped internally)
    let results = store.search_fts("hello", 999_999).unwrap();
    assert!(results.len() <= 100);
}

#[test]
fn test_search_by_keywords_limit_capped() {
    let store = test_store();
    let mut mem = make_memory("test", "keyword search test");
    mem.keywords = vec!["findme".into()];
    store.store(mem).unwrap();

    let results = store.search_by_keywords(&["findme"], 999_999).unwrap();
    assert!(results.len() <= 100);
}

// === Additional MemoryStore coverage ===

#[test]
fn test_search_fts_empty_query() {
    let store = test_store();
    store.store(make_memory("topic", "hello world")).unwrap();
    let results = store.search_fts("", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_search_by_keywords_empty() {
    let store = test_store();
    let results = store.search_by_keywords(&[], 10).unwrap();
    assert!(results.is_empty());
}

/// Audit regression: an unescaped `%` keyword degenerates into a
/// match-everything LIKE pattern instead of matching literal `%`.
#[test]
fn test_search_by_keywords_escapes_percent_wildcard() {
    let store = test_store();
    // Contains the literal substring "100%" — the only row that should
    // match a properly-escaped '100%' keyword.
    store
        .store(make_memory("t", "revenue grew by 100% year over year"))
        .unwrap();
    // Decoy: contains "100" but NOT the literal "100%". An unescaped
    // '%' in the keyword makes the LIKE pattern `%100%%`, which SQLite
    // collapses to `%100%` ("contains 100 anywhere") — this row would
    // wrongly match under that bug, since it's the difference between
    // "contains the substring 100%" and "contains 100".
    store
        .store(make_memory("t", "the report has exactly 100 lines total"))
        .unwrap();

    let results = store.search_by_keywords(&["100%"], 10).unwrap();
    assert_eq!(
        results.len(),
        1,
        "a literal '100%' keyword must match only rows containing that exact \
             substring, not any row containing '100', got {} hits",
        results.len()
    );
    assert!(results[0].summary.contains("100%"));
}

/// Audit regression: an unescaped `_` keyword matches any single
/// character in that position, so "snake_case" would also match
/// "snakeXcase" for any X.
#[test]
fn test_search_by_keywords_escapes_underscore_wildcard() {
    let store = test_store();
    store
        .store(make_memory("t", "uses snake_case naming"))
        .unwrap();
    store
        .store(make_memory(
            "t",
            "uses snakeXcase naming (not the real word)",
        ))
        .unwrap();

    let results = store.search_by_keywords(&["snake_case"], 10).unwrap();
    assert_eq!(
        results.len(),
        1,
        "'_' in a keyword must be literal, not a single-char wildcard, got {} hits",
        results.len()
    );
    assert!(results[0].summary.contains("snake_case"));
}

#[test]
fn test_update_nonexistent_memory() {
    let store = test_store();
    let mut mem = make_memory("t", "s");
    mem.id = "nonexistent-id".to_string();
    let result = store.update(&mem);
    assert!(result.is_err());
}

#[test]
fn test_delete_nonexistent_memory() {
    let store = test_store();
    let result = store.delete("nonexistent-id");
    assert!(result.is_err());
}

#[test]
fn test_batch_update_access() {
    let store = test_store();
    let id1 = store.store(make_memory("t", "one")).unwrap();
    let id2 = store.store(make_memory("t", "two")).unwrap();
    store.batch_update_access(&[&id1, &id2]).unwrap();
    let m1 = store.get(&id1).unwrap().unwrap();
    let m2 = store.get(&id2).unwrap().unwrap();
    assert_eq!(m1.access_count, 1);
    assert_eq!(m2.access_count, 1);
}

#[test]
fn test_auto_consolidate_below_threshold() {
    let store = test_store();
    store.store(make_memory("t", "one")).unwrap();
    store.store(make_memory("t", "two")).unwrap();
    // Threshold is 10, so no consolidation
    let result = store.auto_consolidate("t", 10).unwrap();
    assert!(!result);
    assert_eq!(store.count_by_topic("t").unwrap(), 2);
}

#[test]
fn test_auto_consolidate_above_threshold() {
    let store = test_store();
    for i in 0..12 {
        store
            .store(make_memory("bulk", &format!("entry {i}")))
            .unwrap();
    }
    let result = store.auto_consolidate("bulk", 10).unwrap();
    assert!(result);
    assert_eq!(store.count_by_topic("bulk").unwrap(), 1);
}

#[test]
fn test_auto_consolidate_with_embedder_attaches_embedding() {
    // Audit M2/AC2: the embedder-aware variant must produce a
    // consolidated memory that is recall-ready (embedding != None).
    struct StubEmbedder;
    impl icm_core::Embedder for StubEmbedder {
        fn embed(&self, _text: &str) -> IcmResult<Vec<f32>> {
            Ok(vec![0.42; icm_core::DEFAULT_EMBEDDING_DIMS])
        }
        fn embed_batch(&self, texts: &[&str]) -> IcmResult<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|_| vec![0.42; icm_core::DEFAULT_EMBEDDING_DIMS])
                .collect())
        }
        fn dimensions(&self) -> usize {
            icm_core::DEFAULT_EMBEDDING_DIMS
        }
    }
    let store = test_store();
    for i in 0..11 {
        store
            .store(make_memory("rolled", &format!("fact {i}")))
            .unwrap();
    }
    let stub = StubEmbedder;
    let did = store
        .auto_consolidate_with_embedder("rolled", 10, Some(&stub))
        .unwrap();
    assert!(did);
    let consolidated = store.get_by_topic("rolled").unwrap();
    assert_eq!(consolidated.len(), 1);
    let embedding = consolidated[0]
        .embedding
        .as_ref()
        .expect("consolidated memory must have an embedding");
    assert_eq!(embedding.len(), icm_core::DEFAULT_EMBEDDING_DIMS);
    assert!((embedding[0] - 0.42).abs() < 1e-6);
}

#[test]
fn test_apply_decay_with_aggressive_factor() {
    let store = test_store();
    store.store(make_memory("t", "decayable")).unwrap();
    let affected = store.apply_decay(0.5).unwrap();
    assert!(affected > 0);
    let mems = store.get_by_topic("t").unwrap();
    assert!(mems[0].weight < 1.0);
}

#[test]
fn test_prune_low_weight() {
    let store = test_store();
    store.store(make_memory("t", "will be pruned")).unwrap();
    // Apply aggressive decay
    store.apply_decay(0.01).unwrap();
    let pruned = store.prune(0.5).unwrap();
    assert!(pruned > 0);
    assert_eq!(store.count().unwrap(), 0);
}

#[test]
fn test_list_topics_multiple() {
    let store = test_store();
    store.store(make_memory("alpha", "a")).unwrap();
    store.store(make_memory("beta", "b")).unwrap();
    store.store(make_memory("alpha", "c")).unwrap();
    let topics = store.list_topics().unwrap();
    assert_eq!(topics.len(), 2);
}

#[test]
fn test_stats_multi_topic() {
    let store = test_store();
    store.store(make_memory("t1", "one")).unwrap();
    store.store(make_memory("t2", "two")).unwrap();
    let stats = store.stats().unwrap();
    assert_eq!(stats.total_memories, 2);
    assert_eq!(stats.total_topics, 2);
}

#[test]
fn test_get_by_topic_prefix() {
    let store = test_store();
    store
        .store(make_memory("project:web", "web stuff"))
        .unwrap();
    store
        .store(make_memory("project:api", "api stuff"))
        .unwrap();
    store.store(make_memory("other", "unrelated")).unwrap();
    let results = store.get_by_topic_prefix("project:*").unwrap();
    assert_eq!(results.len(), 2);
}

// ── expand_with_neighbors ────────────────────────────────────────────

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

// ── get_many (batched fetch) ─────────────────────────────────────────

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

// ── LRU cache invalidation ────────────────────────────────────────────

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

// ── content-hash dedup ───────────────────────────────────────────────

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

#[test]
fn test_transcript_create_session_and_record() {
    let store = test_store();
    let sid = store
        .create_session("claude-code", Some("proj"), None)
        .unwrap();
    assert!(!sid.is_empty());

    let mid = store
        .record_message(&sid, Role::User, "hello world", None, None, None)
        .unwrap();
    assert!(!mid.is_empty());

    let msgs = store.list_session_messages(&sid, 10, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "hello world");
    assert_eq!(msgs[0].role, Role::User);
}

/// Issue #272 perf invariant: FTS5 search across a few thousand
/// archived messages must stay sub-second. The bench keeps the
/// budget loose so CI noise doesn't flake; the goal is a regression
/// bell, not microbenchmarking.
#[test]
fn perf_session_archive_search_2k_messages() {
    let store = test_store();
    let sid = store
        .ensure_session("perf-sess", "claude-code", Some("icm"), None)
        .unwrap();
    for i in 0..2_000 {
        // Sprinkle the keyword into ~1% of messages so search
        // returns something but doesn't degenerate to a full scan.
        let body = if i % 100 == 0 {
            format!("turbofish needle hit {i}")
        } else {
            format!("filler payload number {i}")
        };
        store
            .record_message(&sid, Role::User, &body, None, None, None)
            .unwrap();
    }
    let start = std::time::Instant::now();
    let hits = store
        .search_transcripts("turbofish", None, None, 50)
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        hits.len() >= 10,
        "expected at least 10 hits, got {}",
        hits.len()
    );
    assert!(
        elapsed.as_millis() < 1500,
        "search across 2k archived messages took {}ms (budget 1500ms)",
        elapsed.as_millis(),
    );
}

// === FactsStore tests (issue #273) ===

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

/// Issue #272: `ensure_session` must be idempotent so repeated
/// hook fires keyed by the same external `session_id` land under
/// one row, not N.
#[test]
fn test_ensure_session_is_idempotent() {
    let store = test_store();
    let external_id = "claude-sess-abc-123";
    let id1 = store
        .ensure_session(external_id, "claude-code", Some("icm"), None)
        .unwrap();
    assert_eq!(id1, external_id);

    // Re-call with the same id — must NOT create a new row.
    let id2 = store
        .ensure_session(external_id, "claude-code", Some("icm"), None)
        .unwrap();
    assert_eq!(id2, external_id);

    let sessions = store.list_sessions(Some("icm"), 10).unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "ensure_session must be idempotent, got: {sessions:?}",
    );
    assert_eq!(sessions[0].id, external_id);

    // Recording into the same id works.
    store
        .record_message(external_id, Role::User, "first turn", None, None, None)
        .unwrap();
    store
        .record_message(
            external_id,
            Role::Tool,
            "tool out",
            Some("bash"),
            None,
            None,
        )
        .unwrap();
    let msgs = store.list_session_messages(external_id, 10, 0).unwrap();
    assert_eq!(msgs.len(), 2);
}

#[test]
fn test_transcript_record_into_missing_session_fails() {
    let store = test_store();
    let err = store
        .record_message("nonexistent", Role::User, "hi", None, None, None)
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("session"));
}

#[test]
fn test_transcript_search_fts5_boolean_and_phrase() {
    let store = test_store();
    let sid = store
        .create_session("cli", Some("db-debate"), None)
        .unwrap();
    store
        .record_message(
            &sid,
            Role::Assistant,
            "Postgres 16 supports JSONB and BRIN indexes natively.",
            None,
            None,
            None,
        )
        .unwrap();
    store
        .record_message(
            &sid,
            Role::Assistant,
            "MySQL lacks BRIN; its JSON type is stored differently.",
            None,
            None,
            None,
        )
        .unwrap();
    store
        .record_message(&sid, Role::User, "Et SQLite ?", None, None, None)
        .unwrap();

    // Boolean OR
    let hits = store
        .search_transcripts("postgres OR mysql", None, None, 10)
        .unwrap();
    assert_eq!(hits.len(), 2);

    // Exact phrase
    let phrase_hits = store
        .search_transcripts("\"BRIN indexes\"", None, None, 10)
        .unwrap();
    assert_eq!(phrase_hits.len(), 1);
    assert!(phrase_hits[0].message.content.contains("Postgres"));
}

/// Audit regression: `search_transcripts` bound the raw query straight
/// to `messages_fts MATCH ?1` with no handling for malformed FTS5
/// syntax. A trailing boolean operator or an unbalanced paren threw a
/// raw sqlite error instead of degrading gracefully to "no results" —
/// while still preserving valid FTS5 syntax (see the OR/phrase test
/// above), which a blanket `sanitize_fts_query` call would have broken.
#[test]
fn test_transcript_search_malformed_fts5_query_degrades_gracefully() {
    let store = test_store();
    let sid = store.create_session("cli", None, None).unwrap();
    store
        .record_message(&sid, Role::User, "hello world", None, None, None)
        .unwrap();

    for bad_query in ["hello AND", "(hello", "hello OR OR"] {
        let result = store.search_transcripts(bad_query, None, None, 10);
        assert!(
            result.is_ok(),
            "malformed FTS5 query {bad_query:?} must not error: {result:?}"
        );
        assert!(result.unwrap().is_empty());
    }
}

#[test]
fn test_transcript_search_scoped_by_session_and_project() {
    let store = test_store();
    let s1 = store.create_session("cli", Some("alpha"), None).unwrap();
    let s2 = store.create_session("cli", Some("beta"), None).unwrap();
    store
        .record_message(&s1, Role::User, "alpha wants postgres", None, None, None)
        .unwrap();
    store
        .record_message(&s2, Role::User, "beta wants postgres", None, None, None)
        .unwrap();

    // Global search returns both
    let all = store
        .search_transcripts("postgres", None, None, 10)
        .unwrap();
    assert_eq!(all.len(), 2);

    // Session filter
    let only_s1 = store
        .search_transcripts("postgres", Some(&s1), None, 10)
        .unwrap();
    assert_eq!(only_s1.len(), 1);
    assert_eq!(only_s1[0].message.session_id, s1);

    // Project filter
    let only_beta = store
        .search_transcripts("postgres", None, Some("beta"), 10)
        .unwrap();
    assert_eq!(only_beta.len(), 1);
    assert_eq!(only_beta[0].session.project.as_deref(), Some("beta"));
}

#[test]
fn test_transcript_stats_breakdown() {
    let store = test_store();
    let s = store.create_session("claude-code", None, None).unwrap();
    store
        .record_message(&s, Role::User, "q", None, None, None)
        .unwrap();
    store
        .record_message(&s, Role::Assistant, "a", None, None, None)
        .unwrap();
    store
        .record_message(&s, Role::Tool, "{}", Some("Bash"), Some(10), None)
        .unwrap();

    let stats = store.transcript_stats().unwrap();
    assert_eq!(stats.total_sessions, 1);
    assert_eq!(stats.total_messages, 3);
    assert!(stats.total_bytes > 0);
    assert_eq!(stats.by_role.len(), 3);
    assert!(stats.by_agent.iter().any(|(a, _)| a == "claude-code"));
    assert_eq!(stats.top_sessions.len(), 1);
    assert_eq!(stats.top_sessions[0].1, 3);
}

#[test]
fn test_transcript_forget_cascade_deletes_messages() {
    let store = test_store();
    let s = store.create_session("cli", None, None).unwrap();
    for i in 0..5 {
        store
            .record_message(&s, Role::User, &format!("msg {i}"), None, None, None)
            .unwrap();
    }

    store.forget_session(&s).unwrap();

    assert!(store.get_session(&s).unwrap().is_none());
    let msgs = store.list_session_messages(&s, 100, 0).unwrap();
    assert!(msgs.is_empty());
}

#[test]
fn test_transcript_list_sessions_sorted_by_updated() {
    let store = test_store();
    let a = store.create_session("cli", Some("p"), None).unwrap();
    let b = store.create_session("cli", Some("p"), None).unwrap();
    // Bump `a` by recording a message (updates its updated_at)
    store
        .record_message(&a, Role::User, "bump", None, None, None)
        .unwrap();

    let list = store.list_sessions(Some("p"), 10).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, a); // most recently updated first
    assert_eq!(list[1].id, b);
}

#[test]
fn test_transcript_messages_chronological() {
    let store = test_store();
    let s = store.create_session("cli", None, None).unwrap();
    let ids: Vec<_> = (0..3)
        .map(|i| {
            store
                .record_message(&s, Role::User, &format!("{i}"), None, None, None)
                .unwrap()
        })
        .collect();

    let msgs = store.list_session_messages(&s, 10, 0).unwrap();
    let got: Vec<_> = msgs.iter().map(|m| m.id.clone()).collect();
    assert_eq!(got, ids);
}

// ── Hook telemetry ─────────────────────────────────────────────────

fn insert(event: &str, duration_ms: i64, exit_code: i32) -> HookEventInsert {
    HookEventInsert {
        event: event.into(),
        project: None,
        session_id: None,
        tool_name: None,
        duration_ms: Some(duration_ms),
        exit_code,
        payload_size: None,
        note: None,
    }
}

#[test]
fn test_record_hook_event_persists() {
    let store = test_store();
    let id = store.record_hook_event(&insert("post", 12, 0)).unwrap();
    assert!(id > 0);
    assert_eq!(store.hook_event_count().unwrap(), 1);
}

#[test]
fn test_hook_events_recent_orders_newest_first_and_filters() {
    let store = test_store();
    store.record_hook_event(&insert("post", 10, 0)).unwrap();
    store.record_hook_event(&insert("end", 9, 0)).unwrap();
    store.record_hook_event(&insert("post", 20, 1)).unwrap();

    let all = store.hook_events_recent(10, None).unwrap();
    assert_eq!(all.len(), 3);
    // Newest first: third insert wins position 0.
    assert_eq!(all[0].event, "post");
    assert_eq!(all[0].exit_code, 1);

    let posts = store.hook_events_recent(10, Some("post")).unwrap();
    assert_eq!(posts.len(), 2);
    assert!(posts.iter().all(|r| r.event == "post"));

    let ends = store.hook_events_recent(10, Some("end")).unwrap();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].duration_ms, Some(9));
}

#[test]
fn test_hook_stats_buckets_by_event_and_computes_percentiles() {
    let store = test_store();
    // post: durations [10, 20, 30] — p50=20, p99=30
    store.record_hook_event(&insert("post", 10, 0)).unwrap();
    store.record_hook_event(&insert("post", 20, 0)).unwrap();
    store.record_hook_event(&insert("post", 30, 1)).unwrap();
    // end: single 9ms success
    store.record_hook_event(&insert("end", 9, 0)).unwrap();

    // Use a wide window so all rows fall inside.
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let stats = store.hook_stats(&cutoff).unwrap();
    let by_event: std::collections::HashMap<_, _> =
        stats.into_iter().map(|r| (r.event.clone(), r)).collect();

    let post = &by_event["post"];
    assert_eq!(post.count, 3);
    assert_eq!(post.error_count, 1);
    assert_eq!(post.p50_duration_ms, 20);
    assert_eq!(post.p99_duration_ms, 30);

    let end = &by_event["end"];
    assert_eq!(end.count, 1);
    assert_eq!(end.error_count, 0);
    assert_eq!(end.p50_duration_ms, 9);
}

#[test]
fn test_prune_hook_events_drops_old_rows_only() {
    let store = test_store();
    store.record_hook_event(&insert("post", 10, 0)).unwrap();
    // Cutoff in the future → wipes everything.
    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let n = store.prune_hook_events(&future).unwrap();
    assert_eq!(n, 1);
    assert_eq!(store.hook_event_count().unwrap(), 0);

    // Re-insert, then prune with a past cutoff → keeps row.
    store.record_hook_event(&insert("post", 10, 0)).unwrap();
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let n = store.prune_hook_events(&past).unwrap();
    assert_eq!(n, 0);
    assert_eq!(store.hook_event_count().unwrap(), 1);
}

// ── code_areas (issue #196) ────────────────────────────────────────

#[test]
fn test_upsert_code_area_inserts_then_increments_touch_count() {
    let store = test_store();
    store
        .upsert_code_area("proj", "src/foo.rs", None, Some("s1"), Some("Edit"))
        .unwrap();
    let after_first = store.list_code_areas(None, None, None, 10).unwrap();
    assert_eq!(after_first.len(), 1);
    assert_eq!(after_first[0].touch_count, 1);
    assert_eq!(after_first[0].project, "proj");
    assert_eq!(after_first[0].file_path, "src/foo.rs");

    // Same path again — touch_count++ and last_touched_at updates.
    let first_touched_at = after_first[0].first_touched_at;
    store
        .upsert_code_area("proj", "src/foo.rs", None, Some("s2"), Some("Write"))
        .unwrap();
    let after_second = store.list_code_areas(None, None, None, 10).unwrap();
    assert_eq!(after_second.len(), 1, "no duplicate row on re-touch");
    assert_eq!(after_second[0].touch_count, 2);
    // first_touched_at is preserved across re-touches.
    assert_eq!(after_second[0].first_touched_at, first_touched_at);
    // session_id / tool_name are refreshed to the latest.
    assert_eq!(after_second[0].session_id.as_deref(), Some("s2"));
    assert_eq!(after_second[0].tool_name.as_deref(), Some("Write"));
}

#[test]
fn test_upsert_code_area_preserves_existing_description_when_passed_none() {
    let store = test_store();
    store
        .upsert_code_area("proj", "f.rs", Some("initial note"), None, None)
        .unwrap();
    store
        .upsert_code_area("proj", "f.rs", None, None, None)
        .unwrap();
    let rows = store.list_code_areas(None, None, None, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].description.as_deref(), Some("initial note"));
}

#[test]
fn test_upsert_code_area_overwrites_description_when_passed_some() {
    let store = test_store();
    store
        .upsert_code_area("proj", "f.rs", Some("v1"), None, None)
        .unwrap();
    store
        .upsert_code_area("proj", "f.rs", Some("v2"), None, None)
        .unwrap();
    let rows = store.list_code_areas(None, None, None, 10).unwrap();
    assert_eq!(rows[0].description.as_deref(), Some("v2"));
}

#[test]
fn test_list_code_areas_filters_by_project_and_path_suffix() {
    let store = test_store();
    store
        .upsert_code_area("alpha", "src/a.rs", None, None, None)
        .unwrap();
    store
        .upsert_code_area("alpha", "src/b.rs", None, None, None)
        .unwrap();
    store
        .upsert_code_area("beta", "src/a.rs", None, None, None)
        .unwrap();

    let only_alpha = store
        .list_code_areas(Some("alpha"), None, None, 10)
        .unwrap();
    assert_eq!(only_alpha.len(), 2);

    // Suffix match catches both alpha/src/a.rs and beta/src/a.rs.
    let any_a = store
        .list_code_areas(None, Some("src/a.rs"), None, 10)
        .unwrap();
    assert_eq!(any_a.len(), 2);
    for r in &any_a {
        assert!(r.file_path.ends_with("src/a.rs"));
    }

    // Project + path combine.
    let alpha_a = store
        .list_code_areas(Some("alpha"), Some("src/a.rs"), None, 10)
        .unwrap();
    assert_eq!(alpha_a.len(), 1);
    assert_eq!(alpha_a[0].project, "alpha");
}

#[test]
fn test_list_code_areas_filters_by_since_timestamp() {
    let store = test_store();
    store
        .upsert_code_area("p", "old.rs", None, None, None)
        .unwrap();
    // Cutoff one hour ahead skips everything we just inserted.
    let cutoff = chrono::Utc::now() + chrono::Duration::hours(1);
    let after = store.list_code_areas(None, None, Some(cutoff), 10).unwrap();
    assert!(after.is_empty());
    // Cutoff one hour behind keeps the row.
    let past = chrono::Utc::now() - chrono::Duration::hours(1);
    let after = store.list_code_areas(None, None, Some(past), 10).unwrap();
    assert_eq!(after.len(), 1);
}

#[test]
fn test_list_code_areas_orders_by_last_touched_desc() {
    let store = test_store();
    store
        .upsert_code_area("p", "first.rs", None, None, None)
        .unwrap();
    // Sleep so the second insert lands at a strictly later second.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    store
        .upsert_code_area("p", "second.rs", None, None, None)
        .unwrap();
    let rows = store.list_code_areas(None, None, None, 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].file_path, "second.rs");
    assert_eq!(rows[1].file_path, "first.rs");
}

#[test]
fn test_code_area_count_matches_unique_paths() {
    let store = test_store();
    assert_eq!(store.code_area_count().unwrap(), 0);
    store
        .upsert_code_area("p", "f.rs", None, None, None)
        .unwrap();
    store
        .upsert_code_area("p", "f.rs", None, None, None)
        .unwrap(); // re-touch
    store
        .upsert_code_area("p", "g.rs", None, None, None)
        .unwrap();
    assert_eq!(store.code_area_count().unwrap(), 2);
}
