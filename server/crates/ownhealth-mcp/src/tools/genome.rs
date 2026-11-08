//! get_genome_findings and lookup_variant (`mcp/tools/genome.py`, mcp.md 5.4, 5.5), backed by lane D.
//! Both need read:genome. Sensitive findings need the profile's agents switch plus read:genome and
//! read:sensitive; the minor rule withholds adult-onset, sensitive and carrier findings for a minor.
//! Withheld findings are counted, never described.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use super::{Answer, Call, ToolResult, ToolSpec};
use crate::envelope::ToolError;
use crate::gate::lane_gate;
use crate::limits::REGION_MAX_BP;
use crate::services::{FindingsArgs, LookupError, VariantArgs};

const GENOME_LIMITATION: &str =
    "Research-grade analysis, not a clinical laboratory report. Confirm actionable findings with a clinical test.";
const SUMMARY_FIELDS: [&str; 17] = [
    "id",
    "category",
    "gene",
    "diplotype",
    "phenotype",
    "activity_score",
    "rsid",
    "hgvs",
    "zygosity",
    "clinvar",
    "evidence",
    "confidence",
    "call_state",
    "verification",
    "drugs",
    "text",
    "src",
];
/// Checked as vocabulary at import (genome/schema.py).
const VALIDATED_VALUE_KEYS: [&str; 2] = ["phenotype", "diplotype"];

#[allow(clippy::unwrap_used)]
static REGION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^chr([0-9XYM]+):(\d+)-(\d+)$").unwrap());

/// Free text from a facts file (`genome.model.untrusted_fact_text`).
fn fact_text(v: &Value) -> Value {
    match v {
        Value::String(s) => json!({"trust": "untrusted_text", "origin": "genome_facts", "text": s}),
        Value::Null => Value::Null,
        other => json!({"trust": "untrusted_text", "origin": "genome_facts", "text": other}),
    }
}

fn wrap_strings(v: &Value) -> Value {
    match v {
        Value::String(_) => fact_text(v),
        Value::Array(a) => Value::Array(a.iter().map(wrap_strings).collect()),
        other => other.clone(),
    }
}

/// The full finding for an agent: the facts file's prose (condition name, value strings) wrapped.
fn full(f: &Map<String, Value>) -> Value {
    let mut out = f.clone();
    if let Some(Value::Object(c)) = f.get("condition") {
        let mut c = c.clone();
        let name = fact_text(c.get("name").unwrap_or(&Value::Null));
        c.insert("name".into(), name);
        out.insert("condition".into(), Value::Object(c));
    }
    if let Some(Value::Object(v)) = f.get("value") {
        let wrapped: Map<String, Value> = v
            .iter()
            .map(|(k, x)| {
                let x = if VALIDATED_VALUE_KEYS.contains(&k.as_str()) {
                    x.clone()
                } else {
                    wrap_strings(x)
                };
                (k.clone(), x)
            })
            .collect();
        out.insert("value".into(), Value::Object(wrapped));
    }
    Value::Object(out)
}

fn findings_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "profile": {"type": "string"},
            "category": {"enum": ownhealth_genome::CATEGORIES},
            "gene": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,20}$"},
            "min_evidence": {"enum": ownhealth_genome::EVIDENCE, "default": "E2"},
            "detail": {"enum": ["summary", "full"], "default": "summary"},
            "cursor": {"type": "string"},
            "as_of": {"type": "string"},
        },
        "additionalProperties": false,
    })
}

fn lookup_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "profile": {"type": "string"},
            "rsid": {"type": "string", "pattern": "^rs\\d+$"},
            "hgvs": {"type": "string", "maxLength": 120, "pattern": "^[A-Za-z0-9_.:>+*()\\-]+$"},
            "region": {"type": "string", "pattern": "^chr[0-9XYM]+:\\d+-\\d+$"},
            "cursor": {"type": "string"},
        },
        "oneOf": [{"required": ["rsid"]}, {"required": ["hgvs"]}, {"required": ["region"]}],
        "additionalProperties": false,
    })
}

pub static FINDINGS_SPEC: ToolSpec = ToolSpec {
    name: "get_genome_findings",
    title: "Genome findings",
    description: "Curated findings from the profile's OwnGenome genome-facts file: pharmacogenes, pathogenic variants, \
carrier status, HLA, risk markers, each with evidence E1-E4, confidence and call_state (ref, alt, \
no_call, not_on_array, not_covered). Filter by category or gene. E4 and traits are left out unless \
asked for. not_assessed lists what the source could not test: not assessed is unknown, not negative. \
withheld.sensitive counts findings behind the profile's opt-in; their content is never shown. \
Research-grade unless a clinical lab confirmed it.",
    input_schema: findings_schema,
    any_scope: &["read:genome"],
    all_scopes: &[],
    profile_bound: true,
    read_only: true,
};

pub static LOOKUP_SPEC: ToolSpec = ToolSpec {
    name: "lookup_variant",
    title: "Look up one genome position",
    description: "What the profile's genome says at one position (rsID, HGVS or a region up to 1 Mb), read from the \
tabix-indexed VCF. call_state is always one of ref, alt, no_call, not_on_array, not_covered; \
not_on_array and not_covered mean unknown, not negative. Positions inside a gene behind the profile's \
sensitive opt-in come back withheld. Capped per token per day; quota shows what is left.",
    input_schema: lookup_schema,
    any_scope: &["read:genome_raw"],
    all_scopes: &["read:genome"],
    profile_bound: true,
    read_only: true,
};

fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

pub async fn get_genome_findings(call: &mut Call<'_>) -> ToolResult<Answer> {
    let pid = call.profile_id()?;
    let gate = lane_gate(call.perm()?, call.gate()?);
    let q = FindingsArgs {
        category: call.arg_str("category").map(str::to_string),
        gene: call.arg_str("gene").map(str::to_string),
        min_evidence: call.arg_str("min_evidence").unwrap_or("E2").to_string(),
        as_of: call.as_of.day,
    };
    let found = call.svc.list_findings(&mut *call.conn, pid, &gate, &q).await?;
    let mut findings: Vec<Map<String, Value>> = found
        .get("findings")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|f| f.as_object().cloned()).collect())
        .unwrap_or_default();
    if call.arg_str("category") != Some("trait") {
        findings.retain(|f| f.get("category").and_then(Value::as_str) != Some("trait"));
    }
    let detail_full = call.arg_str("detail") == Some("full");
    let findings: Vec<Value> = if detail_full {
        findings.iter().map(full).collect()
    } else {
        findings
            .iter()
            .map(|f| {
                let mut m = Map::new();
                for k in SUMMARY_FIELDS {
                    if let Some(v) = f.get(k) {
                        m.insert(k.into(), v.clone());
                    }
                }
                Value::Object(m)
            })
            .collect()
    };
    let empty = findings.is_empty();
    let mut data = Map::new();
    data.insert("assay".into(), found.get("assay").cloned().unwrap_or(Value::Null));
    data.insert("findings".into(), Value::Array(findings));
    if empty {
        data.insert("no_findings".into(), Value::Bool(true));
    }
    let mut a = Answer::new(data);
    a.pageable = Some("findings");
    a.budget = if detail_full { 15000 } else { 4000 };
    a.not_assessed = found
        .get("not_assessed")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    a.withheld = Some(json!({
        "sensitive": int_of(found.get("withheld_sensitive")),
        "minor_rule": int_of(found.get("withheld_minor")),
        "by_scope": [],
    }));
    a.db_versions = found
        .get("db_versions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    a.limitations = vec![json!(GENOME_LIMITATION)];
    Ok(a)
}

pub async fn lookup_variant(call: &mut Call<'_>) -> ToolResult<Answer> {
    let cap = call.limits.genome_raw_lookups_per_day;
    if call.usage.lookups >= cap {
        return Err(ToolError::fix(
            "genome_lookup_cap",
            format!("This token used its {cap} genome lookups for today."),
            "It resets at 00:00 in the install's timezone.",
        )
        .with("daily_cap", json!(cap))
        .into());
    }
    let region = call.arg_str("region").map(str::to_string);
    if let Some(r) = &region
        && let Some(m) = REGION_RE.captures(r)
    {
        let start: i64 = m[2].parse().unwrap_or(i64::MAX);
        let end: i64 = m[3].parse().unwrap_or(i64::MAX);
        if end < start || end.saturating_sub(start).saturating_add(1) > REGION_MAX_BP {
            return Err(ToolError::fix(
                "bad_input",
                "A region must run forward and span at most 1 Mb.",
                "Split the region.",
            )
            .into());
        }
    }
    let pid = call.profile_id()?;
    let gate = lane_gate(call.perm()?, call.gate()?);
    let q = VariantArgs {
        rsid: call.arg_str("rsid").map(str::to_string),
        hgvs: call.arg_str("hgvs").map(str::to_string),
        region: region.clone(),
        cursor: call.inner_cursor.as_ref().map(|c| match c {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
    };
    let blobs = call.rt.blobs.clone();
    let data = match call.svc.lookup_variant(&mut *call.conn, &blobs, pid, &gate, &q).await {
        Ok(v) => v,
        Err(LookupError::Query { code, message, fix }) => {
            return Err(ToolError {
                code,
                message,
                fix,
                extra: Map::new(),
            }
            .into());
        }
        Err(LookupError::Other(e)) => return Err(e.into()),
    };
    let mut data = match data {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let inner_next = data.shift_remove("next_cursor").filter(|v| !v.is_null());
    let mut bits = Map::new();
    for k in ["limitations", "db_versions", "withheld", "not_assessed"] {
        if let Some(v) = data.shift_remove(k) {
            bits.insert(k.into(), v);
        }
    }
    // lane D answers a withheld site with withheld="withheld_sensitive" | "withheld_minor": keep the
    // word in data and count it in the envelope's withheld block
    if let Some(Value::String(why)) = bits.get("withheld").cloned() {
        let counted = if why == "withheld_minor" {
            json!({"minor_rule": 1})
        } else {
            json!({"sensitive": 1})
        };
        data.insert("withheld".into(), Value::String(why));
        bits.insert("withheld".into(), counted);
    }
    data.insert(
        "quota".into(),
        json!({"used_today": call.usage.lookups + 1, "daily_cap": cap}),
    );
    let mut limitations: Vec<Value> = bits
        .get("limitations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let untested = data.get("calls").and_then(Value::as_array).is_some_and(|calls| {
        calls.iter().any(|c| {
            matches!(
                c.get("call_state").and_then(Value::as_str),
                Some("not_on_array" | "not_covered")
            )
        })
    });
    if untested {
        limitations.push(json!(
            "Not tested by this source at this position. Unknown, not negative."
        ));
    }
    let mut a = Answer::new(data);
    a.budget = if region.is_some() { 2000 } else { 800 };
    a.limitations = limitations;
    a.db_versions = bits
        .get("db_versions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    a.withheld = bits.get("withheld").cloned().filter(|v| v.is_object());
    a.not_assessed = bits
        .get("not_assessed")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    a.counters.lookups = 1;
    a.inner_next_cursor = inner_next;
    Ok(a)
}
