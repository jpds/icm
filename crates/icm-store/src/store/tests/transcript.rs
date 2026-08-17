//! Per-domain tests for the SQLite backend (`store::tests`).

use super::*;

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

// Hook telemetry
