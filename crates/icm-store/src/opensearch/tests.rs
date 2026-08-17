//! Test suite for the OpenSearch backend (`opensearch::tests`).

use super::*;

/// Audit regression: a memory id containing reserved URL characters
/// (e.g. `/`, `..`, `?`) was interpolated straight into the `_doc/{id}`
/// REST path, letting an attacker-controlled id redirect the request to
/// a different document/endpoint.
#[test]
fn test_url_encode_path_segment() {
    assert_eq!(
        url_encode_path_segment("abc-DEF_123.~"),
        "abc-DEF_123.~",
        "unreserved chars must pass through unchanged"
    );
    assert_eq!(url_encode_path_segment("a/b"), "a%2Fb");
    assert_eq!(url_encode_path_segment("../secret"), "..%2Fsecret");
    assert_eq!(url_encode_path_segment("id?x=1"), "id%3Fx%3D1");
    assert_eq!(url_encode_path_segment("id#frag"), "id%23frag");
    assert_eq!(url_encode_path_segment("a b"), "a%20b");
}

/// Audit regression: `apply_decay`'s Painless script computed a raw
/// multiplier that goes negative for low-importance/low-access memories
/// at factor<0.5 (still inside the CLI's own validated range). This
/// mirrors the exact formula now wrapped in `Math.max(0.0, ...)`.
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
        "Math.max(0.0, ...) must clamp this to 0.0"
    );
}
