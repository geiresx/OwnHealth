//! RFC 8785 canonical JSON, the bytes the state hash is taken over (0.1.1 tests/state/test_canonical.py).
//! Dates, UUIDs and decimals are strings by the time they reach JSON in Rust; floats are refused.

use ownhealth_rules::{canonical_json, state_hash};
use serde_json::json;

#[test]
fn keys_sorted_no_whitespace() -> anyhow::Result<()> {
    assert_eq!(
        canonical_json(&json!({"b": 1, "a": [true, null, "x"]}))?,
        br#"{"a":[true,null,"x"],"b":1}"#
    );
    Ok(())
}

/// RFC 8785 3.2.3: in UTF-16 the emoji is the surrogate pair D83D DE00, so it sorts between U+20AC
/// and U+FB33; by code point it would sort last.
#[test]
fn keys_sort_by_utf16_code_units() -> anyhow::Result<()> {
    let out = canonical_json(&json!({"\u{fb33}": 1, "\u{1f600}": 2, "€": 3}))?;
    assert_eq!(String::from_utf8(out)?, "{\"€\":3,\"\u{1f600}\":2,\"\u{fb33}\":1}");
    Ok(())
}

#[test]
fn decimal_strings_keep_printed_precision() -> anyhow::Result<()> {
    let out = canonical_json(&json!({"v": "0.90", "d": "2026-11-07", "u": "00000000-0000-0000-0000-000000000001"}))?;
    assert_eq!(
        out,
        br#"{"d":"2026-11-07","u":"00000000-0000-0000-0000-000000000001","v":"0.90"}"#
    );
    Ok(())
}

#[test]
fn unicode_is_not_escaped() -> anyhow::Result<()> {
    assert_eq!(canonical_json(&json!({"n": "Lucía"}))?, "{\"n\":\"Lucía\"}".as_bytes());
    Ok(())
}

#[test]
fn floats_are_refused() {
    assert!(canonical_json(&json!({"v": 0.1})).is_err());
}

#[test]
fn state_hash_ignores_its_own_field_and_sees_every_other_change() -> anyhow::Result<()> {
    let body = json!({"a": "1", "flags": [{"src": "src_a12"}]});
    let h = state_hash(&body)?;
    assert!(h.starts_with("sha256:") && h.len() == 71);
    assert_eq!(
        state_hash(&json!({"a": "1", "flags": [{"src": "src_a12"}], "state_hash": "sha256:whatever"}))?,
        h
    );
    assert_ne!(state_hash(&json!({"a": "1", "flags": [{"src": "src_a13"}]}))?, h);
    Ok(())
}
