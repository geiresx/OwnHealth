//! Canonical JSON per RFC 8785 (JCS) for the values a state body holds, byte for byte the output of
//! `ownhealth/state/canonical.py`, and the `state_hash` over it.
//!
//! Keys sorted by UTF-16 code units, no whitespace, the escaping of Python's
//! `json.dumps(s, ensure_ascii=False)`. The body is a `serde_json::Value` whose decimals, dates,
//! datetimes and UUIDs are already strings: build them with `pyjson::py_decimal_str` (a Decimal
//! keeps its printed precision: `"0.90"`), `pyjson::py_date`, `pyjson::py_isoformat`. Floats are
//! refused: a float in the hash would make it depend on binary rounding.

use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CanonError {
    #[error("floats never enter the state hash; use a decimal string or an integer ({0})")]
    Float(String),
}

impl From<CanonError> for crate::Error {
    fn from(e: CanonError) -> Self {
        crate::Error::Invalid(e.to_string())
    }
}

/// `json.dumps(s, ensure_ascii=False)`.
pub fn py_json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn enc(v: &Value, out: &mut String) -> Result<(), CanonError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                out.push_str(&n.to_string());
            } else {
                return Err(CanonError::Float(n.to_string()));
            }
        }
        Value::String(s) => py_json_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                enc(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                py_json_str(k, out);
                out.push(':');
                enc(&map[k.as_str()], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// The canonical bytes of a value.
pub fn canonical_json(v: &Value) -> Result<Vec<u8>, CanonError> {
    let mut out = String::new();
    enc(v, &mut out)?;
    Ok(out.into_bytes())
}

/// `"sha256:" + sha256(canonical_json(body without its own "state_hash"))`, as `state.state_hash`.
pub fn state_hash(body: &Value) -> Result<String, CanonError> {
    let bytes = match body {
        Value::Object(map) => {
            let mut clean = map.clone();
            clean.shift_remove("state_hash");
            canonical_json(&Value::Object(clean))?
        }
        other => canonical_json(other)?,
    };
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn sorts_by_utf16_and_escapes_like_python() {
        // U+FB01 sorts after U+1F600 in UTF-16 (0xD83D...) but before it by code point.
        let v = json!({"b": 1, "a": [true, null, "x\n\u{1f}\"\\é"], "\u{fb01}": 0, "\u{1f600}": 1});
        let got = String::from_utf8(canonical_json(&v).expect("canon")).expect("utf8");
        assert_eq!(
            got,
            "{\"a\":[true,null,\"x\\n\\u001f\\\"\\\\é\"],\"b\":1,\"\u{1f600}\":1,\"\u{fb01}\":0}"
        );
    }

    #[test]
    fn floats_are_refused() {
        assert!(canonical_json(&json!({"x": 0.5})).is_err());
        assert!(canonical_json(&json!({"x": 5})).is_ok());
    }

    #[test]
    fn the_hash_skips_its_own_field() {
        let a = state_hash(&json!({"x": "1", "state_hash": "sha256:old"})).expect("hash");
        let b = state_hash(&json!({"x": "1"})).expect("hash");
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:") && a.len() == 71);
    }
}
