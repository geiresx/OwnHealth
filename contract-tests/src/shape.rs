//! JSON shapes: the keys and value types of an answer, with ids, timestamps and values left out,
//! compared against a golden file recorded from the Python 0.1.1 backend (`golden/<name>.json`).
//!
//! A shape keeps what the UI depends on and the backends must agree on: every key, whether a
//! number prints as an integer or a float (Python prints a Decimal of scale 0 as an integer),
//! whether a datetime ends in `Z` or `+00:00`, null versus a value. Array elements merge into one
//! shape (a key some elements lack is marked `absent` there). Object keys that are ids (uuids, src
//! ids) collapse into `<uuid>` / `<src>` so the shape stays the same from one fresh database to the next.
//!
//! The goldens were recorded from the Python 0.1.1 backend. Re-record them only for an intended API
//! change, with `OWNHEALTH_CONTRACT_RECORD=1 contract-tests/run.sh rust shapes`.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use anyhow::{Context, bail};
use regex::Regex;
use serde_json::{Map, Value, json};

static UUID: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").ok()
});
static SRC: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^src_[a-z0-9_]+$").ok());
static DATE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").ok());
static DT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,6})?(Z|[+-]\d{2}:\d{2})?$").ok()
});
static SHA: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^sha256:[0-9a-f]{64}$").ok());
static HEX64: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").ok());

fn is(re: &LazyLock<Option<Regex>>, s: &str) -> bool {
    re.as_ref().is_some_and(|r| r.is_match(s))
}

/// The type name of a string: its format when it has one the backends must keep.
fn string_kind(s: &str) -> String {
    if is(&UUID, s) {
        "uuid".into()
    } else if is(&SRC, s) {
        "src_id".into()
    } else if is(&DATE, s) {
        "date".into()
    } else if is(&DT, s) {
        let tail = &s[19..];
        let zone = if tail.ends_with('Z') {
            "Z"
        } else if tail.contains('+') || tail.contains('-') {
            "offset"
        } else {
            "naive"
        };
        format!("datetime_{zone}")
    } else if is(&SHA, s) {
        "sha256_tagged".into()
    } else if is(&HEX64, s) {
        "sha256_hex".into()
    } else {
        "string".into()
    }
}

fn key_kind(k: &str) -> String {
    if is(&UUID, k) {
        "<uuid>".into()
    } else if is(&SRC, k) {
        "<src>".into()
    } else {
        k.to_string()
    }
}

/// Keys whose string values are part of the contract (codes and enums the UI switches on): their
/// shape keeps the value itself, as `=value`.
pub const ENUM_KEYS: &[&str] = &[
    "accepted_by",
    "action",
    "actor_kind",
    "agg",
    "allowance",
    "assay",
    "auth_kind",
    "call_state",
    "categories",
    "category",
    "doc_type",
    "evidence",
    "identity",
    "identity_mode",
    "interpretation",
    "kind",
    "lang",
    "locale",
    "locks",
    "method",
    "origin",
    "reason",
    "reasons",
    "relationship",
    "resolution",
    "result_kind",
    "role",
    "scopes",
    "sex_at_birth",
    "status",
    "trust",
    "type",
    "unit_pref",
    "units",
    "verification",
    "via",
    "why_not",
    "zygosity",
];

/// The shape of a JSON value.
pub fn shape(v: &Value) -> Value {
    shape_in(None, v)
}

fn shape_in(key: Option<&str>, v: &Value) -> Value {
    match v {
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(n) => json!(if n.is_f64() { "float" } else { "int" }),
        Value::String(s) if key.is_some_and(|k| ENUM_KEYS.contains(&k)) && s.len() <= 60 => {
            json!(format!("={s}"))
        }
        Value::String(s) => json!(string_kind(s)),
        Value::Array(a) => {
            let mut merged: Option<Value> = None;
            for x in a {
                let s = shape_in(key, x);
                merged = Some(match merged {
                    None => s,
                    Some(m) => merge(&m, &s),
                });
            }
            json!({"[]": merged.unwrap_or(json!("empty"))})
        }
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, x) in o {
                let kk = key_kind(k);
                let s = shape_in(Some(k), x);
                let s = match out.get(&kk) {
                    Some(prev) => merge(prev, &s),
                    None => s,
                };
                out.insert(kk, s);
            }
            sort_keys(Value::Object(out))
        }
    }
}

fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<_> = o.into_iter().collect();
            keys.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(keys.into_iter().map(|(k, v)| (k, sort_keys(v))).collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
        x => x,
    }
}

/// The alternatives of a shape: a union is `{"|": [...]}`.
fn alts(v: &Value) -> Vec<Value> {
    match v.get("|").and_then(Value::as_array) {
        Some(a) => a.clone(),
        None => vec![v.clone()],
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Object(o) if o.contains_key("[]") => "array",
        Value::Object(_) => "object",
        _ => "scalar",
    }
}

/// Merge two shapes: objects merge key by key, arrays merge their element shapes, anything else
/// becomes a union.
pub fn merge(a: &Value, b: &Value) -> Value {
    let mut objects: Option<Value> = None;
    let mut arrays: Option<Value> = None;
    let mut scalars: BTreeSet<String> = BTreeSet::new();
    for x in alts(a).into_iter().chain(alts(b)) {
        match kind_of(&x) {
            "object" => {
                objects = Some(match objects {
                    None => x,
                    Some(o) => merge_objects(&o, &x),
                })
            }
            "array" => {
                arrays = Some(match arrays {
                    None => x,
                    Some(prev) => {
                        let (p, n) = (&prev["[]"], &x["[]"]);
                        let el = if p == "empty" {
                            n.clone()
                        } else if n == "empty" {
                            p.clone()
                        } else {
                            merge(p, n)
                        };
                        json!({"[]": el})
                    }
                })
            }
            _ => {
                scalars.insert(x.as_str().unwrap_or_default().to_string());
            }
        }
    }
    let mut out: Vec<Value> = scalars.into_iter().map(Value::String).collect();
    out.extend(arrays);
    out.extend(objects);
    if out.len() == 1 {
        out.remove(0)
    } else {
        json!({"|": out})
    }
}

fn merge_objects(a: &Value, b: &Value) -> Value {
    let (Some(ao), Some(bo)) = (a.as_object(), b.as_object()) else {
        return a.clone();
    };
    let keys: BTreeSet<&String> = ao.keys().chain(bo.keys()).collect();
    let absent = json!("absent");
    let mut out = Map::new();
    for k in keys {
        let v = match (ao.get(k), bo.get(k)) {
            (Some(x), Some(y)) => merge(x, y),
            (Some(x), None) | (None, Some(x)) => merge(x, &absent),
            (None, None) => absent.clone(),
        };
        out.insert(k.clone(), v);
    }
    Value::Object(out)
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("golden")
        .join(format!("{name}.json"))
}

/// Compare the shape of `v` with `golden/<name>.json`; with OWNHEALTH_CONTRACT_RECORD=1, write it
/// instead (only for an intended API change: the goldens were recorded from 0.1.1).
pub fn assert_shape(name: &str, v: &Value) -> anyhow::Result<()> {
    let got = shape(v);
    let path = golden_path(name);
    if std::env::var("OWNHEALTH_CONTRACT_RECORD").is_ok_and(|x| x == "1") {
        std::fs::create_dir_all(path.parent().context("no parent")?)?;
        std::fs::write(&path, serde_json::to_string_pretty(&got)? + "\n")?;
        return Ok(());
    }
    let want: Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("no golden {}", path.display()))?,
    )?;
    if got != want {
        let mut diffs = Vec::new();
        diff("$", &want, &got, &mut diffs);
        bail!(
            "shape of {name} differs from the golden ({} differences):\n  {}",
            diffs.len(),
            diffs.join("\n  ")
        );
    }
    Ok(())
}

fn diff(path: &str, want: &Value, got: &Value, out: &mut Vec<String>) {
    if want == got {
        return;
    }
    match (want.as_object(), got.as_object()) {
        (Some(w), Some(g)) if !w.contains_key("|") && !g.contains_key("|") => {
            let keys: BTreeSet<&String> = w.keys().chain(g.keys()).collect();
            for k in keys {
                match (w.get(k), g.get(k)) {
                    (Some(a), Some(b)) => diff(&format!("{path}.{k}"), a, b, out),
                    (Some(a), None) => out.push(format!("{path}.{k}: missing (golden {a})")),
                    (None, Some(b)) => out.push(format!("{path}.{k}: unexpected key ({b})")),
                    (None, None) => {}
                }
            }
        }
        _ => out.push(format!("{path}: golden {want}, got {got}")),
    }
}

/// Compare `v` itself (not its shape) with `golden/<name>.exact.json`, for static answers such
/// as tools/list; records like `assert_shape`.
pub fn assert_exact(name: &str, v: &Value) -> anyhow::Result<()> {
    let path = golden_path(&format!("{name}.exact"));
    if std::env::var("OWNHEALTH_CONTRACT_RECORD").is_ok_and(|x| x == "1") {
        std::fs::create_dir_all(path.parent().context("no parent")?)?;
        std::fs::write(&path, serde_json::to_string_pretty(v)? + "\n")?;
        return Ok(());
    }
    let want: Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("no golden {}", path.display()))?,
    )?;
    if &want != v {
        let mut diffs = Vec::new();
        diff_values("$", &want, v, &mut diffs);
        bail!(
            "{name} differs from the golden ({} differences):\n  {}",
            diffs.len(),
            diffs.join("\n  ")
        );
    }
    Ok(())
}

fn diff_values(path: &str, want: &Value, got: &Value, out: &mut Vec<String>) {
    if want == got {
        return;
    }
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            let keys: BTreeSet<&String> = w.keys().chain(g.keys()).collect();
            for k in keys {
                match (w.get(k), g.get(k)) {
                    (Some(a), Some(b)) => diff_values(&format!("{path}.{k}"), a, b, out),
                    (Some(_), None) => out.push(format!("{path}.{k}: missing")),
                    (None, Some(b)) => out.push(format!("{path}.{k}: unexpected {b}")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(w), Value::Array(g)) if w.len() == g.len() => {
            for (i, (a, b)) in w.iter().zip(g).enumerate() {
                diff_values(&format!("{path}[{i}]"), a, b, out);
            }
        }
        _ => {
            let cut = |v: &Value| {
                let s = v.to_string();
                if s.len() > 300 {
                    format!("{}...", &s[..s.floor_char_boundary(300)])
                } else {
                    s
                }
            };
            out.push(format!("{path}: golden {}, got {}", cut(want), cut(got)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_keep_number_kinds_formats_and_merge_arrays() {
        let v = json!({
            "a": 1, "b": 1.5, "c": "2026-11-08T10:00:00Z", "d": "2026-11-08T10:00:00.123+00:00",
            "e": [{"x": 1}, {"x": null, "y": "src_a07"}], "f": [], "g": {"0b1c2d3e-0000-4000-8000-000000000000": true}
        });
        let s = shape(&v);
        assert_eq!(s["a"], "int");
        assert_eq!(s["b"], "float");
        assert_eq!(s["c"], "datetime_Z");
        assert_eq!(s["d"], "datetime_offset");
        assert_eq!(s["e"]["[]"]["x"], json!({"|": ["int", "null"]}));
        assert_eq!(s["e"]["[]"]["y"], json!({"|": ["absent", "src_id"]}));
        assert_eq!(s["f"], json!({"[]": "empty"}));
        assert_eq!(s["g"]["<uuid>"], "bool");
        // a different number kind is a different shape: the check can fail
        assert_ne!(shape(&json!({"a": 1.0})), shape(&json!({"a": 1})));
    }
}
