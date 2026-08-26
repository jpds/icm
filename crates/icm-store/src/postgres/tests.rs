//! Test suite for the PostgreSQL backend (`postgres::tests`).

use super::*;

/// Audit regression: a keyword containing `%`/`_` was interpolated
/// straight into an ILIKE pattern unescaped, turning it into an
/// unintended wildcard.
#[test]
fn test_escape_like_wildcards() {
    assert_eq!(escape_like_wildcards("100%"), "100\\%");
    assert_eq!(escape_like_wildcards("snake_case"), "snake\\_case");
    assert_eq!(escape_like_wildcards("back\\slash"), "back\\\\slash");
    assert_eq!(escape_like_wildcards("plain"), "plain");
}

/// Audit regression: `apply_decay`'s raw multiplier goes negative for
/// `low` importance + low access count at factor < 0.5 (still inside
/// the CLI's own validated [0.0, 1.0) range). This reproduces the exact
/// arithmetic the SQL `GREATEST(0.0, ...)` clamp now guards, as a plain
/// Rust assertion (no live Postgres needed to prove the formula itself
/// would go negative without the clamp).
#[test]
fn test_apply_decay_formula_would_go_negative_without_clamp() {
    let factor: f64 = 0.4;
    let mult: f64 = 2.0; // low importance
    let access: f64 = 0.0;
    let raw = 1.0 - (1.0 - factor) * mult / (1.0 + access * 0.1);
    assert!(
        raw < 0.0,
        "expected the pre-clamp formula to go negative, got {raw}"
    );
    assert_eq!(
        raw.max(0.0),
        0.0,
        "GREATEST(0.0, ...) must clamp this to 0.0"
    );
}
