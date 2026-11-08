//! get_health_status (`mcp/tools/status.py`, mcp.md 4, 5.1): the rule pack's deterministic snapshot
//! of one profile. The body comes from lane E, hashed before anything else happens; language, units
//! and identity mode are applied after, so every token gets the same `state_hash`. Sections for
//! domains the token cannot read are dropped and named in `withheld.by_scope`.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::{Answer, Call, ToolResult, ToolSpec};
use crate::envelope::{truthy, wrap_plain};

pub const MAX_STATE_BYTES: usize = 2048;

const ENVELOPE_FIELDS: [&str; 6] = [
    "limitations",
    "not_assessed",
    "evidence",
    "db_versions",
    "withheld",
    "notice",
];
const SECTION_SCOPE: [(&str, &str); 3] = [
    ("medications", "read:records"),
    ("supplements", "read:records"),
    ("active_conditions", "read:records"),
];
const FRESHNESS_SCOPE: [(&str, &str); 5] = [
    ("labs", "read:labs"),
    ("vitals", "read:vitals"),
    ("wearables", "read:wearables"),
    ("records", "read:records"),
    ("genome", "read:genome"),
];
/// Keep a 5-year profile under 2 KB whole without paging: a flag keeps its template text and drops
/// the structured copies of what the text says; a stale or never-measured gap keeps its structured
/// fields and drops the text that repeats them.
const FLAG_FIELDS: [&str; 6] = ["rank", "kind", "rule", "code", "text", "src"];
const GAP_DROP: [&str; 2] = ["stale", "never_measured"];
/// Each person note is quoted this far in the status; the whole note stays under its src.
const NOTE_CHARS: usize = 200;
const GENOME_FLAG_KINDS: [&str; 4] = ["gene_drug", "gene_lab", "GENE_DRUG", "GENE_LAB"];

const DESCRIPTION: &str = "The deterministic health snapshot of one profile: flags from the rule pack (out of the lab's own \
range, significant change, trend, stale, never measured, gene x drug), current medications and \
conditions, gaps and corrections, each fact with its src id. Call this first for any 'how am I doing' \
or 'what should I ask my doctor' question. Quote flags with their src ids; drill down with get_trend or \
search_observations only for the flags that matter. never_measured and stale are gaps: missing is not \
normal. Keep state_hash: the same hash later means nothing changed. Do not restate a flag as a \
diagnosis; use the template text or the cited range. Information, not diagnosis.";

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "profile": {"type": "string", "description": "Profile slug or 'me'. Omitted: the token's default."},
            "as_of": {"type": "string", "format": "date", "description": "What the store knew on this date."},
            "cursor": {"type": "string"},
        },
        "additionalProperties": false,
    })
}

pub static SPEC: ToolSpec = ToolSpec {
    name: "get_health_status",
    title: "Health status",
    description: DESCRIPTION,
    input_schema: schema,
    any_scope: &["read:summary"],
    all_scopes: &[],
    profile_bound: true,
    read_only: true,
};

/// (data, envelope fields) of the rendered state for the call's profile, scope-trimmed.
pub async fn status_data(call: &mut Call<'_>) -> ToolResult<(Map<String, Value>, Map<String, Value>)> {
    let pid = call.profile_id()?;
    let body = call.svc.build_state(&mut *call.conn, pid, call.as_of.day).await?;
    let rendered = call.svc.render(&body, &call.lang, &call.units, &call.mode)?;
    let mut rendered = match rendered {
        Value::Object(mut m) => match m.shift_remove("data") {
            Some(Value::Object(d)) => d,
            Some(_) => Map::new(),
            None => m,
        },
        _ => Map::new(),
    };
    let mut extra = Map::new();
    for k in ENVELOPE_FIELDS {
        if let Some(v) = rendered.shift_remove(k) {
            extra.insert(k.into(), v);
        }
    }
    for k in ["profile", "as_of", "lang", "identity_mode", "generated_at", "schema"] {
        rendered.shift_remove(k);
    }
    for key in ["medications", "supplements", "active_conditions"] {
        wrap_plain(rendered.get_mut(key), "name", "fhir_note");
    }
    wrap_plain(rendered.get_mut("notes"), "text", "person_note");
    if let Some(Value::Array(notes)) = rendered.get_mut("notes") {
        // a note holds up to 4000 chars; the page must stay under 2 KB
        for note in notes.iter_mut() {
            let Some(t) = note.get_mut("text").and_then(Value::as_object_mut) else {
                continue;
            };
            if let Some(Value::String(s)) = t.get("text")
                && s.chars().count() > NOTE_CHARS
            {
                let cut: String = s.chars().take(NOTE_CHARS).collect::<String>() + "…";
                t.insert("text".into(), Value::String(cut));
                t.insert("truncated".into(), Value::Bool(true));
            }
        }
    }
    if !rendered.contains_key("state_hash") {
        rendered.insert(
            "state_hash".into(),
            body.get("state_hash").cloned().unwrap_or(Value::Null),
        );
    }
    if !rendered.contains_key("rules_version") {
        let rv = body
            .get("rules_version")
            .filter(|v| truthy(v))
            .or_else(|| body.get("rules"))
            .cloned()
            .unwrap_or(Value::Null);
        rendered.insert("rules_version".into(), rv);
    }

    let scopes = call.scopes();
    let mut by_scope: BTreeSet<String> = BTreeSet::new();
    for (key, scope) in SECTION_SCOPE {
        if rendered.contains_key(key) && !scopes.contains(scope) {
            rendered.shift_remove(key);
            by_scope.insert(crate::gate::scope_label(scope));
        }
    }
    if !scopes.contains("read:genome")
        && let Some(Value::Array(flags)) = rendered.get_mut("flags")
    {
        let before = flags.len();
        flags.retain(|f| {
            let Some(m) = f.as_object() else { return true };
            let kind = match m.get("kind") {
                Some(k) => k.as_str(),
                None => m.get("reason").and_then(Value::as_str),
            };
            !kind.is_some_and(|k| GENOME_FLAG_KINDS.contains(&k))
        });
        if flags.len() != before {
            by_scope.insert("genome".into());
        }
    }
    if let Some(Value::Object(fresh)) = rendered.get_mut("data_freshness") {
        for (k, scope) in FRESHNESS_SCOPE {
            if fresh.contains_key(k) && !scopes.contains(scope) {
                fresh.shift_remove(k);
                by_scope.insert(k.into());
            }
        }
    }
    if let Some(Value::Array(flags)) = rendered.get_mut("flags") {
        for f in flags.iter_mut() {
            let Some(m) = f.as_object() else { continue };
            if !m.get("text").is_some_and(truthy) {
                continue;
            }
            let mut kept = Map::new();
            for k in FLAG_FIELDS {
                if let Some(v) = m.get(k) {
                    kept.insert(k.into(), v.clone());
                }
            }
            *f = Value::Object(kept);
        }
    }
    for key in GAP_DROP {
        if let Some(Value::Array(items)) = rendered.get_mut(key) {
            for item in items.iter_mut() {
                if let Some(m) = item.as_object_mut()
                    && m.get("name").is_some_and(truthy)
                {
                    m.shift_remove("text");
                }
            }
        }
    }
    let mut withheld = match extra.shift_remove("withheld") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let mut all: BTreeSet<String> = withheld
        .get("by_scope")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    all.extend(by_scope);
    withheld.insert("by_scope".into(), json!(all));
    extra.insert("withheld".into(), Value::Object(withheld));
    Ok((rendered, extra))
}

fn list_of(extra: &Map<String, Value>, key: &str) -> Vec<Value> {
    match extra.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

pub async fn get_health_status(call: &mut Call<'_>) -> ToolResult<Answer> {
    let (data, extra) = status_data(call).await?;
    let pageable = matches!(data.get("flags"), Some(Value::Array(_))).then_some("flags");
    let mut a = Answer::new(data);
    a.pageable = pageable;
    a.budget = 2000;
    a.max_bytes = Some(MAX_STATE_BYTES);
    a.limitations = list_of(&extra, "limitations");
    a.not_assessed = list_of(&extra, "not_assessed");
    a.evidence = list_of(&extra, "evidence");
    a.db_versions = match extra.get("db_versions") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    a.withheld = extra.get("withheld").cloned();
    a.notice = extra.get("notice").and_then(Value::as_str).map(str::to_string);
    Ok(a)
}
