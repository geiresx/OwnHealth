//! The MCP response envelope (`mcp/envelope.py`, mcp.md 3, 9.1): the common shape, Python's compact
//! JSON (byte for byte, so budgets and hashes match the Python server), the token estimate, signed
//! cursors and the walkers that count facts and collect src ids.

use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub use ownhealth_core::text::{NOTICE, is_untrusted, untrusted};

/// Every envelope carries these keys, in this order (mcp.md 3).
pub const ENVELOPE_KEYS: [&str; 14] = [
    "profile",
    "as_of",
    "generated_at",
    "lang",
    "identity_mode",
    "data",
    "evidence",
    "limitations",
    "not_assessed",
    "withheld",
    "db_versions",
    "next_cursor",
    "budget",
    "notice",
];

/// A tool execution error (`isError: true`) with a code, a one-line message and a fix hint.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub fix: Option<String>,
    pub extra: Map<String, Value>,
}

impl ToolError {
    pub fn new(code: &str, message: impl Into<String>, fix: Option<&str>) -> ToolError {
        ToolError {
            code: code.into(),
            message: message.into(),
            fix: fix.map(str::to_string),
            extra: Map::new(),
        }
    }

    pub fn fix(code: &str, message: impl Into<String>, fix: impl Into<String>) -> ToolError {
        ToolError {
            code: code.into(),
            message: message.into(),
            fix: Some(fix.into()),
            extra: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: Value) -> ToolError {
        self.extra.insert(key.into(), value);
        self
    }

    /// `{code, message, fix, **extra}`: the structuredContent of the error result.
    pub fn body(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("code".into(), json!(self.code));
        m.insert("message".into(), json!(self.message));
        m.insert("fix".into(), json!(self.fix));
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        m
    }

    /// `{"isError": true, **body}` (Python's `as_dict`).
    pub fn as_dict(&self) -> Value {
        let mut m = Map::new();
        m.insert("isError".into(), Value::Bool(true));
        m.extend(self.body());
        Value::Object(m)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

pub fn empty_withheld() -> Value {
    json!({"sensitive": 0, "by_scope": [], "minor_rule": 0})
}

/// The envelope fields besides profile, as_of, lang, identity_mode and data.
#[derive(Debug, Clone, Default)]
pub struct Fields {
    pub generated_at: Option<String>,
    pub evidence: Vec<Value>,
    pub limitations: Vec<Value>,
    pub not_assessed: Vec<Value>,
    pub withheld: Option<Map<String, Value>>,
    pub db_versions: Map<String, Value>,
    pub next_cursor: Option<String>,
    pub budget: Option<Value>,
    pub notice: Option<String>,
}

/// `datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")`.
pub fn now_z() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The common response shape, keys in `ENVELOPE_KEYS` order.
pub fn envelope(profile: Value, as_of: &str, lang: &str, identity_mode: &str, data: Value, f: Fields) -> Value {
    let mut withheld = match empty_withheld() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    for (k, v) in f.withheld.unwrap_or_default() {
        withheld.insert(k, v);
    }
    let mut by_scope: Vec<String> = withheld
        .get("by_scope")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    by_scope.sort();
    by_scope.dedup();
    withheld.insert("by_scope".into(), json!(by_scope));
    let notice = f.notice.filter(|n| !n.is_empty()).unwrap_or_else(|| NOTICE.to_string());
    let mut env = Map::new();
    env.insert("profile".into(), profile);
    env.insert("as_of".into(), json!(as_of));
    env.insert(
        "generated_at".into(),
        json!(f.generated_at.filter(|g| !g.is_empty()).unwrap_or_else(now_z)),
    );
    env.insert("lang".into(), json!(lang));
    env.insert("identity_mode".into(), json!(identity_mode));
    env.insert("data".into(), data);
    env.insert("evidence".into(), Value::Array(f.evidence));
    env.insert("limitations".into(), Value::Array(f.limitations));
    env.insert("not_assessed".into(), Value::Array(f.not_assessed));
    env.insert("withheld".into(), Value::Object(withheld));
    env.insert("db_versions".into(), Value::Object(f.db_versions));
    env.insert("next_cursor".into(), json!(f.next_cursor));
    env.insert(
        "budget".into(),
        f.budget.unwrap_or_else(|| json!({"tokens_est": 0, "truncated": false})),
    );
    env.insert("notice".into(), json!(notice));
    Value::Object(env)
}

// ---------------------------------------------------------------- Python JSON ----

/// Python's `repr(float)`: the shortest digits that round-trip, fixed notation for decimal
/// exponents -4..16, else `1e+16` / `1e-05`.
pub fn py_float(f: f64) -> String {
    if !f.is_finite() {
        return if f.is_nan() {
            "NaN".into()
        } else if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    let s = format!("{f:e}");
    let (mant, exp) = s.split_once('e').unwrap_or((s.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (sign, mant) = match mant.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let body = if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                format!("{digits}{}.0", "0".repeat(int_len - digits.len()))
            } else {
                format!("{}.{}", &digits[..int_len], &digits[int_len..])
            }
        } else {
            format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { "-" } else { "+" };
        format!("{first}{frac}e{esign}{:02}", exp.abs())
    };
    format!("{sign}{body}")
}

/// `str()` of a JSON number as Python prints it (int, or float repr).
pub fn py_number(n: &serde_json::Number) -> String {
    if n.is_f64() {
        py_float(n.as_f64().unwrap_or_default())
    } else {
        n.to_string()
    }
}

fn push_py_string(out: &mut String, s: &str) {
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

fn push_py_json(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&py_number(n)),
        Value::String(s) => push_py_string(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_py_json(out, x);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_py_string(out, k);
                out.push(':');
                push_py_json(out, x);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(obj, ensure_ascii=False, separators=(",", ":"))`, byte for byte.
pub fn dumps(v: &Value) -> String {
    let mut out = String::new();
    push_py_json(&mut out, v);
    out
}

/// `len(json)/3.5` rounded up, the budget unit of mcp.md 3 (Python counts characters).
pub fn estimate_tokens(v: &Value) -> usize {
    tokens_of(&dumps(v))
}

/// A string is measured as it is.
pub fn tokens_of(s: &str) -> usize {
    (s.chars().count() as f64 / 3.5).ceil() as usize
}

/// `datetime` in the envelope: UTC, seconds, `Z` (`jsonable`).
pub fn z_time(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ------------------------------------------------------------------- cursors ----

const B64: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

fn mac(secret: &str, body: &[u8]) -> Vec<u8> {
    let mut key = Sha256::new();
    key.update(b"ownhealth.mcp.cursor:");
    key.update(secret.as_bytes());
    let key = key.finalize();
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(&key).unwrap_or_else(|_| {
        // HMAC accepts keys of any length; this branch cannot run.
        <Hmac<Sha256> as KeyInit>::new_from_slice(&[0u8; 32]).unwrap_or_else(|_| unreachable!())
    });
    m.update(body);
    m.finalize().into_bytes()[..16].to_vec()
}

/// A short stable hash of the query a cursor belongs to (tool, token, profile, arguments).
pub fn query_hash(parts: &[Value]) -> String {
    let s = dumps(&Value::Array(parts.to_vec()));
    hex::encode(Sha256::digest(s.as_bytes()))[..24].to_string()
}

/// Opaque, stateless, signed: base64url of `{q, as_of, k}` plus an HMAC (mcp.md 3).
pub fn encode_cursor(secret: &str, query_hash: &str, as_of: &str, last_key: &Value) -> String {
    let body = dumps(&json!({"q": query_hash, "as_of": as_of, "k": last_key}));
    format!(
        "{}.{}",
        B64.encode(body.as_bytes()),
        B64.encode(mac(secret, body.as_bytes()))
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorError(pub &'static str);

/// `(as_of, last_key)` for a cursor this server signed for this exact query.
pub fn decode_cursor(secret: &str, cursor: &str, query_hash: &str) -> Result<(Option<String>, Value), CursorError> {
    let (body_s, mac_s) = cursor.split_once('.').ok_or(CursorError("malformed cursor"))?;
    let body = B64.decode(body_s).map_err(|_| CursorError("malformed cursor"))?;
    let got = B64.decode(mac_s).map_err(|_| CursorError("malformed cursor"))?;
    let want = mac(secret, &body);
    if !constant_eq(&got, &want) {
        return Err(CursorError("cursor signature does not match"));
    }
    let data: Value = serde_json::from_slice(&body).map_err(|_| CursorError("malformed cursor"))?;
    if data.get("q").and_then(Value::as_str) != Some(query_hash) {
        return Err(CursorError("cursor belongs to a different query or token"));
    }
    let as_of = data.get("as_of").and_then(Value::as_str).map(str::to_string);
    Ok((as_of, data.get("k").cloned().unwrap_or(Value::Null)))
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ----------------------------------------------------------------- walkers ----

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

pub fn src_values(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn walk(v: &Value, f: &mut dyn FnMut(&Value)) {
    f(v);
    match v {
        Value::Object(m) => m.values().for_each(|x| walk(x, f)),
        Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
        _ => {}
    }
}

/// Every src id cited anywhere in `v`, in first-seen order.
pub fn collect_src_ids(v: &Value) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut set = std::collections::HashSet::new();
    walk(v, &mut |node| {
        if let Some(src) = node.as_object().and_then(|m| m.get("src")) {
            for s in src_values(src) {
                if set.insert(s.clone()) {
                    seen.push(s);
                }
            }
        }
    });
    seen
}

/// A fact is one row with a src id (mcp.md 9.4).
pub fn count_facts(v: &Value) -> usize {
    let mut n = 0;
    walk(v, &mut |node| {
        if node.as_object().and_then(|m| m.get("src")).is_some_and(truthy) {
            n += 1;
        }
    });
    n
}

/// Wrap `item[key]` in place when another service handed it over as a bare string: record names
/// are free text from imports or people and go out wrapped.
pub fn wrap_plain(items: Option<&mut Value>, key: &str, origin: &str) {
    let Some(Value::Array(items)) = items else {
        return;
    };
    for item in items.iter_mut() {
        if let Some(m) = item.as_object_mut()
            && let Some(Value::String(s)) = m.get(key)
        {
            let wrapped = json!({"trust": "untrusted_text", "origin": origin, "text": s});
            m.insert(key.into(), wrapped);
        }
    }
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_as_python_repr() {
        for (f, want) in [
            (420.0, "420.0"),
            (1.62, "1.62"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-2.5e-7, "-2.5e-07"),
            (123456.789, "123456.789"),
            (0.0, "0.0"),
            (-1.0, "-1.0"),
            (1e100, "1e+100"),
        ] {
            assert_eq!(py_float(f), want, "{f}");
        }
    }

    #[test]
    fn dumps_matches_python_compact_json() {
        let v = json!({"a": [1, 2.0, "é\n\"x\"\u{1}"], "b": null, "c": true});
        assert_eq!(
            dumps(&v),
            "{\"a\":[1,2.0,\"é\\n\\\"x\\\"\\u0001\"],\"b\":null,\"c\":true}"
        );
    }

    #[test]
    fn cursor_round_trip_and_tamper() {
        let q = query_hash(&[json!("t"), json!("tok"), Value::Null, json!({"a": 1})]);
        let c = encode_cursor("s3cret", &q, "2026-11-07T10:00:00+00:00", &json!({"off": 5}));
        let (as_of, k) = decode_cursor("s3cret", &c, &q).unwrap();
        assert_eq!(as_of.as_deref(), Some("2026-11-07T10:00:00+00:00"));
        assert_eq!(k, json!({"off": 5}));
        assert_eq!(
            decode_cursor("other", &c, &q).unwrap_err().0,
            "cursor signature does not match"
        );
        assert_eq!(
            decode_cursor("s3cret", &c, "x").unwrap_err().0,
            "cursor belongs to a different query or token"
        );
        assert_eq!(decode_cursor("s3cret", "nodot", &q).unwrap_err().0, "malformed cursor");
    }

    #[test]
    fn cursor_signed_by_python_verifies() {
        // envelope.encode_cursor("k", "abc", "2026-11-07T10:00:00+00:00", {"off": 3}) in Python 0.1.1
        let py =
            "eyJxIjoiYWJjIiwiYXNfb2YiOiIyMDI2LTExLTA3VDEwOjAwOjAwKzAwOjAwIiwiayI6eyJvZmYiOjN9fQ.bkWbHvMau1T37uA8Sx8GHA";
        let c = encode_cursor("k", "abc", "2026-11-07T10:00:00+00:00", &json!({"off": 3}));
        assert_eq!(c, py);
        // envelope.query_hash("t", "tok", None, {"a": 1})
        assert_eq!(
            query_hash(&[json!("t"), json!("tok"), Value::Null, json!({"a": 1})]),
            "550bb5c537e9ff4e5b1987b1"
        );
    }

    #[test]
    fn tool_error_shape() {
        assert_eq!(
            ToolError::fix("x", "m", "f").as_dict(),
            json!({"isError": true, "code": "x", "message": "m", "fix": "f"})
        );
    }

    #[test]
    fn walkers() {
        let v = json!({"a": {"src": "src_a"}, "b": [{"src": ["src_b", "src_a"]}, {"src": ""}]});
        assert_eq!(collect_src_ids(&v), vec!["src_a", "src_b", ""]);
        assert_eq!(count_facts(&v), 2);
    }
}
