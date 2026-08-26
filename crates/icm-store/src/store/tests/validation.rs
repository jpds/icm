//! Per-domain tests for the SQLite backend (`store::tests`).

use super::*;

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
