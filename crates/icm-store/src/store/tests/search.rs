//! Per-domain tests for the SQLite backend (`store::tests`).

use super::*;

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
