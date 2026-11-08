//! The compact markdown text block beside structuredContent (`mcp/text.py`, mcp.md 3): the same
//! facts, one per line, flags first. Untrusted text appears only inside an `<untrusted_text>` tag
//! whose body cannot close the tag.

use serde_json::{Map, Value};

use crate::envelope::{is_untrusted, py_number, truthy};

const FIRST_KEYS: [&str; 1] = ["flags"];
const SKIP_KEYS: [&str; 1] = ["trust"];

/// Python `str(x)` of a JSON scalar inside an f-string (`None` for null).
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => py_number(n),
        other => fmt(other),
    }
}

pub fn tag(node: &Value) -> String {
    let body = match node.get("text") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => py_str(v),
        None => String::new(),
    };
    let body = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let origin = node.get("origin").map_or_else(|| "None".to_string(), py_str);
    format!("<untrusted_text origin=\"{origin}\">{body}</untrusted_text>")
}

pub fn fmt(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        v if is_untrusted(v) => tag(v),
        Value::Object(m) => {
            let inner: Vec<String> = m
                .iter()
                .filter(|(k, v)| !v.is_null() && !SKIP_KEYS.contains(&k.as_str()))
                .map(|(k, v)| format!("{k}={}", fmt(v)))
                .collect();
            format!("{{{}}}", inner.join(" "))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(fmt).collect::<Vec<_>>().join(", ")),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Number(n) => py_number(n),
        Value::String(s) => s.replace('\n', " "),
    }
}

fn empty_list(v: &Value) -> bool {
    matches!(v, Value::Array(a) if a.is_empty())
}

fn fact_line(d: &Map<String, Value>) -> String {
    let body: Vec<String> = d
        .iter()
        .filter(|(k, v)| k.as_str() != "src" && !v.is_null() && !empty_list(v))
        .map(|(k, v)| format!("{k}={}", fmt(v)))
        .collect();
    let cite = match d.get("src") {
        Some(Value::Array(a)) => a.iter().map(py_str).collect::<Vec<_>>().join(","),
        Some(v) => py_str(v),
        None => "None".into(),
    };
    format!("- {} [{cite}]", body.join(" "))
}

fn lines(node: &Value, path: &str, out: &mut Vec<String>) {
    match node {
        Value::Object(m) => {
            if m.get("src").is_some_and(truthy) {
                out.push(fact_line(m));
                return;
            }
            if is_untrusted(node) {
                out.push(format!("- {path}: {}", tag(node)));
                return;
            }
            let keys = FIRST_KEYS
                .iter()
                .filter(|k| m.contains_key(**k))
                .map(|k| k.to_string())
                .chain(m.keys().filter(|k| !FIRST_KEYS.contains(&k.as_str())).cloned());
            for k in keys {
                let Some(v) = m.get(&k) else { continue };
                match v {
                    Value::Object(_) | Value::Array(_) => {
                        if truthy(v) {
                            out.push(if path.is_empty() {
                                format!("{k}:")
                            } else {
                                format!("{path}.{k}:")
                            });
                            lines(v, &k, out);
                        } else if v.is_array() {
                            out.push(format!("{k}: none"));
                        }
                    }
                    Value::Null => {}
                    other => out.push(format!("{k}: {}", fmt(other))),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                let flat = item
                    .as_object()
                    .is_some_and(|m| !m.values().any(|v| matches!(v, Value::Array(a) if !a.is_empty())));
                if flat && !item.get("src").is_some_and(truthy) && !is_untrusted(item) {
                    let m = item.as_object().cloned().unwrap_or_default();
                    let parts: Vec<String> = m
                        .iter()
                        .filter(|(_, v)| !v.is_null() && !empty_list(v))
                        .map(|(k, v)| format!("{k}={}", fmt(v)))
                        .collect();
                    out.push(format!("- {}", parts.join(" ")));
                } else if item.is_object() || item.is_array() {
                    lines(item, path, out);
                } else {
                    out.push(format!("- {}", fmt(item)));
                }
            }
        }
        _ => {}
    }
}

/// The text block of one envelope.
pub fn render(env: &Value, tool: &str) -> String {
    let get = |k: &str| env.get(k).cloned().unwrap_or(Value::Null);
    let mut out = vec![format!(
        "OwnHealth {tool} · profile {} · as of {} · lang {}",
        py_str(&get("profile")),
        py_str(&get("as_of")),
        py_str(&get("lang"))
    )];
    let data = get("data");
    let empty = Value::Object(Map::new());
    lines(if truthy(&data) { &data } else { &empty }, "", &mut out);
    let w = get("withheld");
    let by_scope: Vec<String> = w
        .get("by_scope")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(py_str).collect())
        .unwrap_or_default();
    if w.get("sensitive").is_some_and(truthy) || w.get("minor_rule").is_some_and(truthy) || !by_scope.is_empty() {
        let num = |k: &str| w.get(k).map_or_else(|| "0".to_string(), py_str);
        out.push(format!(
            "withheld: sensitive={} minor_rule={} by_scope={}",
            num("sensitive"),
            num("minor_rule"),
            if by_scope.is_empty() {
                "none".to_string()
            } else {
                by_scope.join(",")
            }
        ));
    }
    for key in ["limitations", "not_assessed", "evidence"] {
        let items = get(key);
        if truthy(&items) {
            out.push(format!("{key}:"));
            lines(&items, key, &mut out);
        }
    }
    let dbv = get("db_versions");
    if truthy(&dbv) {
        out.push(format!("db_versions: {}", fmt(&dbv)));
    }
    let cursor = get("next_cursor");
    if truthy(&cursor) {
        out.push(format!(
            "next_cursor: {} (more results; pass it as cursor)",
            py_str(&cursor)
        ));
    }
    let notice = get("notice");
    out.push(if truthy(&notice) {
        py_str(&notice)
    } else {
        String::new()
    });
    out.join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_matches_python() {
        // text.render of this envelope in Python 0.1.1
        let env = json!({
            "profile": "alex", "as_of": "2026-11-07", "lang": "en",
            "data": {
                "resolved": {"alias_groups": ["ldl_c"], "codes": [], "method": "code"},
                "results": [{"name": {"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "LDL <b>"},
                             "value": 97, "unit": "mg/dL", "range": {"high": 130}, "flag": null, "src": "src_a"},
                            {"x": 1.5, "y": [], "z": {"a": null, "trust": "t"}}],
                "flags": [], "empty": {}, "n": null, "ok": true
            },
            "withheld": {"sensitive": 1, "by_scope": [], "minor_rule": 0},
            "limitations": ["Flags use each lab's own printed range."],
            "not_assessed": [], "evidence": [],
            "db_versions": {"clinvar": "2026-10-28"},
            "next_cursor": "abc", "notice": "N."
        });
        let want = "OwnHealth t · profile alex · as of 2026-11-07 · lang en\n\
flags: none\n\
resolved:\n\
resolved.alias_groups:\n\
- ldl_c\n\
codes: none\n\
method: code\n\
results:\n\
- name=<untrusted_text origin=\"pdf_text_layer\">LDL &lt;b&gt;</untrusted_text> value=97 unit=mg/dL range={high=130} [src_a]\n\
- x=1.5 z={}\n\
ok: true\n\
withheld: sensitive=1 minor_rule=0 by_scope=none\n\
limitations:\n\
- Flags use each lab's own printed range.\n\
db_versions: {clinvar=2026-10-28}\n\
next_cursor: abc (more results; pass it as cursor)\n\
N.";
        assert_eq!(render(&env, "t"), want);
    }
}
