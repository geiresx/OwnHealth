//! check_drug (`mcp/tools/drug.py`, mcp.md 5.6): PGx, phenoconversion by current medications, and
//! the labs a prescriber looks at, joined for one drug. Lane E builds the answer; here each part
//! stays behind its scope, whatever the backing service returns, and the missing parts are named
//! in `withheld.by_scope`.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::{Answer, Call, ToolResult, ToolSpec};
use crate::envelope::{dumps, wrap_plain};

const NOTICE: &str = "Information from your records, not medical advice. Discuss with your prescriber.";
/// data key -> scopes it needs, all of them.
const PART_SCOPES: [(&str, &[&str]); 5] = [
    ("pgx", &["read:genome"]),
    // each entry applies to the profile's phenotype, so it is genome data
    ("guidance", &["read:genome"]),
    ("phenoconversion", &["read:genome", "read:records"]),
    ("current_medications_relevant", &["read:records"]),
    ("relevant_labs", &["read:labs"]),
];
const ENVELOPE_FIELDS: [&str; 6] = [
    "limitations",
    "not_assessed",
    "db_versions",
    "withheld",
    "evidence",
    "notice",
];

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "profile": {"type": "string"},
            "drug": {
                "type": "string",
                "minLength": 1,
                "maxLength": 100,
                "description": "Generic or brand name in es/en, or a CPIC drug id",
            },
            "as_of": {"type": "string"},
            "cursor": {"type": "string", "description": "next_cursor of the previous answer: more relevant_labs"},
        },
        "required": ["drug"],
        "additionalProperties": false,
    })
}

pub static SPEC: ToolSpec = ToolSpec {
    name: "check_drug",
    title: "Check a drug against the record",
    description: "Everything the record says about one drug for one profile, in one call: pharmacogenomic guidance \
(CPIC, DPWG) for the profile's genotypes, current medications that change a gene's function \
(phenoconversion), current medications that matter for the drug, and the labs a prescriber would look \
at. Guidance is quoted with its source; it is followed by 'Discuss with your prescriber.' Parts this \
token cannot read are named in withheld.by_scope. Drug-drug interactions beyond phenoconversion are \
not assessed.",
    input_schema: schema,
    any_scope: &["read:genome", "read:records", "read:labs"],
    all_scopes: &[],
    profile_bound: true,
    read_only: true,
};

fn list_of(extra: &Map<String, Value>, key: &str) -> Vec<Value> {
    match extra.get(key) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

pub async fn check_drug(call: &mut Call<'_>) -> ToolResult<Answer> {
    let scopes = call.scopes();
    let pid = call.profile_id()?;
    let drug = call.arg_str("drug").unwrap_or_default().to_string();
    let raw = call
        .svc
        .check_drug(&mut *call.conn, pid, &drug, &scopes, &call.lang, call.as_of.day)
        .await?;
    let mut raw = match raw {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let mut extra = Map::new();
    for k in ENVELOPE_FIELDS {
        if let Some(v) = raw.shift_remove(k) {
            extra.insert(k.into(), v);
        }
    }
    wrap_plain(raw.get_mut("current_medications_relevant"), "name", "fhir_note");
    wrap_plain(raw.get_mut("phenoconversion"), "by", "fhir_note");
    // lane E's sentence quotes the medication name, which is free text from imports or people
    wrap_plain(raw.get_mut("phenoconversion"), "text", "fhir_note");
    let mut by_scope: BTreeSet<String> = extra
        .get("withheld")
        .and_then(|w| w.get("by_scope"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    for (key, need) in PART_SCOPES {
        let missing: Vec<&str> = need.iter().copied().filter(|s| !scopes.contains(*s)).collect();
        if !missing.is_empty() {
            raw.shift_remove(key);
            by_scope.extend(missing.iter().map(|s| crate::gate::scope_label(s)));
        }
    }
    let mut withheld = match extra.get("withheld") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    withheld.insert("by_scope".into(), json!(by_scope));
    let mut not_assessed = list_of(&extra, "not_assessed");
    if !dumps(&Value::Array(not_assessed.clone())).contains("drug-drug interactions") {
        not_assessed.push(json!(
            "drug-drug interactions beyond phenoconversion (no open-licence source configured)"
        ));
    }
    let pageable = matches!(raw.get("relevant_labs"), Some(Value::Array(_))).then_some("relevant_labs");
    let mut a = Answer::new(raw);
    a.pageable = pageable;
    a.budget = 4000;
    a.limitations = list_of(&extra, "limitations");
    a.not_assessed = not_assessed;
    a.db_versions = match extra.get("db_versions") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    a.withheld = Some(Value::Object(withheld));
    a.evidence = list_of(&extra, "evidence");
    a.notice = Some(NOTICE.into());
    Ok(a)
}
