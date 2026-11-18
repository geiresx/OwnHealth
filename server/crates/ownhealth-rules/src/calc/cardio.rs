//! The cardiovascular risk calculators: SCORE2 and SCORE2-OP by risk region, and PREVENT
//! (docs/calculators/score2.md, prevent.md). Each reads the newest lipid draw (total and HDL
//! cholesterol) and, beside it, what the labs do not hold:
//!
//! - systolic pressure: the newest office reading (an observation of bp_systolic) in the 12 months up
//!   to `as_of`, else the mean of the last 30 days of wearable daily means, shown with its window;
//! - smoking status: the newest smoking_status observation (LOINC 72166-2); an answer that is not one
//!   of the known SNOMED CT codes is refused, never passed over for an older one;
//! - eGFR (PREVENT, and SCORE2's chronic kidney disease rule): the newest CKD-EPI 2021 result of the
//!   same document, with the creatinine's src;
//! - recorded conditions (`conditions.yaml`) and active medicines (`drug_classes.yaml`);
//! - the profile's country, for SCORE2's region (`score2_regions.yaml`).
//!
//! Each source is read only when the caller's scopes allow it: conditions, medicines and the smoking
//! status need read:records, the office pressure and the smoking status read:vitals (the two maps of
//! observation categories differ on `survey`), the wearable mean read:wearables. A caller without
//! read:records or read:vitals gets each risk result as `not_assessed`, `scope_missing`, with no input
//! line; without read:wearables the wearable mean is not read.
//!
//! Age is taken on `as_of`, and so is the result's date. A rule the guideline gives is a refusal with
//! a reason, a missing input is `not_assessed` naming it; neither carries a value. An input line says
//! what the record holds: "no diabetes recorded", never "no diabetes".

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{Duration, NaiveDate};
use ownhealth_core::access::Scopes;
use ownhealth_core::records::{active_medications, list_records};
use ownhealth_core::{Error, Result};
use ownhealth_normalize::tables::fold;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection, Row};
use uuid::Uuid;

use super::prevent::{self, Outcome};
use super::score2::{self, Scale};
use super::{Calc, Data, Draw, Obs, Person, Refusal, Sex, input_json, item, refused, units};
use crate::cited::CitedError;
use crate::engine::{day_end, iso};
use crate::num::{from_f64, num, quantize};
use ownhealth_core::lifecycle::visible_sql;

/// The calculators this module computes.
pub const IDS: [&str; 4] = ["score2", "score2_op", "prevent_cvd", "prevent_ascvd"];
const CONDITION_IDS: [&str; 5] = ["ascvd", "diabetes", "ckd", "fh", "heart_failure"];
const DRUG_IDS: [&str; 2] = ["statin", "antihypertensive"];
const REGIONS: [&str; 4] = ["low", "moderate", "high", "very_high"];
/// What SCORE2 refuses on (ESC 2021: not apparently healthy).
const SCORE2_SCOPE: [&str; 4] = ["ascvd", "diabetes", "ckd", "fh"];
/// What PREVENT refuses on (adults without cardiovascular disease).
const PREVENT_SCOPE: [&str; 2] = ["ascvd", "heart_failure"];
/// SNOMED CT answers to LOINC 72166-2 (tobacco smoking status), and what each means here.
pub const SMOKING: [(&str, &str); 8] = [
    ("77176002", "current"),
    ("449868002", "current"),
    ("428041000124106", "current"),
    ("428061000124105", "current"),
    ("428071000124103", "current"),
    ("65568007", "current"),
    ("8517006", "former"),
    ("266919005", "never"),
];
pub const SMOKING_LOINC: &str = "72166-2";
/// The value of a smoking status whose code is none of `SMOKING`.
pub const NOT_CODED: &str = "not_coded";
/// What a risk result reads beyond the labs, as scopes; without one of them it is not assessed.
pub const NEEDS: [&str; 2] = ["read:records", "read:vitals"];

/// The shipped files, compiled in.
pub const FILES: [(&str, &str); 3] = [
    (
        "score2_regions.yaml",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../data/calculators/score2_regions.yaml"
        )),
    ),
    (
        "drug_classes.yaml",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../data/calculators/drug_classes.yaml"
        )),
    ),
    (
        "conditions.yaml",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../data/calculators/conditions.yaml"
        )),
    ),
];

// ------------------------------------------------------------------ data ----

#[derive(Debug, Clone, Deserialize)]
struct RegionRow {
    region: String,
    countries: Vec<String>,
    score2: BTreeMap<String, [String; 2]>,
    score2_op: BTreeMap<String, [String; 2]>,
}

/// One SCORE2 risk region.
#[derive(Debug, Clone)]
pub struct Region {
    pub region: String,
    pub countries: Vec<String>,
    /// (model, sex) -> scale
    scales: BTreeMap<(String, String), Scale>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DrugClass {
    pub id: String,
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConditionClass {
    pub id: String,
    pub terms: Vec<String>,
    /// words that exclude a term only when they come before it in its clause ("riesgo de diabetes")
    #[serde(default)]
    pub not_terms: Vec<String>,
    /// words that exclude a term anywhere in its clause ("diabetes mellitus (gestational)")
    #[serde(default)]
    pub qualifiers: Vec<String>,
    #[serde(default)]
    pub icd10: Vec<String>,
}

/// The three cardiovascular data files, loaded.
#[derive(Debug, Clone)]
pub struct CvData {
    /// "score2_regions.yaml 0.1.0", one per file, in FILES order
    pub versions: Vec<String>,
    pub regions: Vec<Region>,
    pub drugs: Vec<DrugClass>,
    pub conditions: Vec<ConditionClass>,
}

fn rows_of(name: &str, text: &str) -> std::result::Result<(String, Vec<Value>), CitedError> {
    let f = crate::cited::load_bytes(std::path::Path::new(name), text.as_bytes())?;
    Ok((f.version, f.rows.into_iter().map(Value::Object).collect()))
}

fn dec(s: &str, what: &str) -> std::result::Result<Decimal, CitedError> {
    s.parse::<Decimal>()
        .map_err(|e| CitedError(format!("score2_regions.yaml: {what}: {e}")))
}

/// Read and check the three files: every row cited over https, every region, class and condition
/// known to the code, each country in one region only, each region with both sexes of both models.
pub fn parse(regions: &str, drugs: &str, conditions: &str) -> std::result::Result<CvData, CitedError> {
    let (v1, region_rows) = rows_of("score2_regions.yaml", regions)?;
    let (v2, drug_rows) = rows_of("drug_classes.yaml", drugs)?;
    let (v3, cond_rows) = rows_of("conditions.yaml", conditions)?;
    let mut out = CvData {
        versions: vec![
            format!("score2_regions.yaml {v1}"),
            format!("drug_classes.yaml {v2}"),
            format!("conditions.yaml {v3}"),
        ],
        regions: Vec::new(),
        drugs: Vec::new(),
        conditions: Vec::new(),
    };
    for r in region_rows {
        let row: RegionRow = serde_json::from_value(r).map_err(|e| CitedError(format!("score2_regions.yaml: {e}")))?;
        if !REGIONS.contains(&row.region.as_str()) {
            return Err(CitedError(format!(
                "score2_regions.yaml: unknown region `{}`",
                row.region
            )));
        }
        let mut scales = BTreeMap::new();
        for (model, by_sex) in [("score2", &row.score2), ("score2_op", &row.score2_op)] {
            for sex in ["female", "male"] {
                let pair = by_sex.get(sex).ok_or_else(|| {
                    CitedError(format!(
                        "score2_regions.yaml: {} has no {model} scale for {sex}",
                        row.region
                    ))
                })?;
                let what = format!("{} {model} {sex}", row.region);
                scales.insert(
                    (model.to_string(), sex.to_string()),
                    Scale {
                        scale1: dec(&pair[0], &what)?,
                        scale2: dec(&pair[1], &what)?,
                    },
                );
            }
        }
        for c in &row.countries {
            if let Some(other) = out.regions.iter().find(|x| x.countries.contains(c)) {
                return Err(CitedError(format!(
                    "score2_regions.yaml: {c} is in both {} and {}",
                    other.region, row.region
                )));
            }
        }
        out.regions.push(Region {
            region: row.region,
            countries: row.countries,
            scales,
        });
    }
    for r in drug_rows {
        let row: DrugClass = serde_json::from_value(r).map_err(|e| CitedError(format!("drug_classes.yaml: {e}")))?;
        if !DRUG_IDS.contains(&row.id.as_str()) {
            return Err(CitedError(format!("drug_classes.yaml: unknown class `{}`", row.id)));
        }
        out.drugs.push(DrugClass {
            id: row.id,
            names: row.names.iter().map(|n| fold(n)).collect(),
        });
    }
    for r in cond_rows {
        let row: ConditionClass = serde_json::from_value(r).map_err(|e| CitedError(format!("conditions.yaml: {e}")))?;
        if !CONDITION_IDS.contains(&row.id.as_str()) {
            return Err(CitedError(format!("conditions.yaml: unknown condition `{}`", row.id)));
        }
        out.conditions.push(row);
    }
    for id in DRUG_IDS {
        if !out.drugs.iter().any(|d| d.id == id) {
            return Err(CitedError(format!("drug_classes.yaml: no row for `{id}`")));
        }
    }
    for id in CONDITION_IDS {
        if !out.conditions.iter().any(|c| c.id == id) {
            return Err(CitedError(format!("conditions.yaml: no row for `{id}`")));
        }
    }
    Ok(out)
}

/// The shipped data, read once.
pub fn data() -> Result<&'static CvData> {
    static DATA: OnceLock<std::result::Result<CvData, String>> = OnceLock::new();
    DATA.get_or_init(|| parse(FILES[0].1, FILES[1].1, FILES[2].1).map_err(|e| e.0))
        .as_ref()
        .map_err(|e| Error::from(CitedError(e.clone())))
}

impl CvData {
    pub fn region_of(&self, country: &str) -> Option<&Region> {
        let c = country.trim().to_ascii_uppercase();
        self.regions.iter().find(|r| r.countries.contains(&c))
    }
}

// ------------------------------------------------------------------ matching ----

fn words(s: &str) -> Vec<String> {
    fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The clauses of a condition's text, as words. A statement ends at `.`, `;` or a new line; inside it,
/// clauses split at `,`, `/`, "con" and "with", never inside a bracket, since a bracket qualifies the
/// words before it ("diabetes mellitus (gestational)"). The clause right before a colon is a heading
/// that belongs to every clause after it ("Family history: stroke, diabetes").
fn clauses(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for statement in text.split(|c: char| ".;\n".contains(c)) {
        match statement.split_once(':') {
            Some((before, body)) => {
                let mut before = split_clauses(before, &[]);
                let heading = before.pop().unwrap_or_default();
                out.extend(before);
                out.extend(split_clauses(body, &heading));
            }
            None => out.extend(split_clauses(statement, &[])),
        }
    }
    out.retain(|c| !c.is_empty());
    out
}

/// One statement's clauses, each starting with `heading`: split at `,`, `/`, "con" and "with" outside
/// brackets.
fn split_clauses(body: &str, heading: &[String]) -> Vec<Vec<String>> {
    let mut out = vec![heading.to_vec()];
    let push = |seg: &mut String, inside: bool, out: &mut Vec<Vec<String>>| {
        for w in words(&std::mem::take(seg)) {
            if !inside && (w == "con" || w == "with") {
                out.push(heading.to_vec());
            } else {
                out.last_mut().expect("never empty").push(w);
            }
        }
    };
    let mut seg = String::new();
    let mut depth = 0usize;
    for ch in body.chars() {
        match ch {
            '(' | '[' => {
                push(&mut seg, depth > 0, &mut out);
                depth += 1;
            }
            ')' | ']' => {
                push(&mut seg, depth > 0, &mut out);
                depth = depth.saturating_sub(1);
            }
            ',' | '/' if depth == 0 => {
                push(&mut seg, false, &mut out);
                out.push(heading.to_vec());
            }
            _ => seg.push(ch),
        }
    }
    push(&mut seg, depth > 0, &mut out);
    out
}

/// Where `phrase` starts in `text` as whole words.
fn phrase_at(text: &[String], phrase: &str) -> Vec<usize> {
    let p = words(phrase);
    if p.is_empty() {
        return Vec::new();
    }
    text.windows(p.len())
        .enumerate()
        .filter(|(_, w)| *w == p.as_slice())
        .map(|(i, _)| i)
        .collect()
}

/// The medicine class a name or ingredient belongs to, if any.
pub fn drug_class<'a>(cv: &'a CvData, texts: &[&str]) -> Vec<&'a str> {
    let ws: Vec<String> = texts.iter().flat_map(|t| words(t)).collect();
    cv.drugs
        .iter()
        .filter(|c| c.names.iter().any(|n| ws.contains(n)))
        .map(|c| c.id.as_str())
        .collect()
}

fn is_icd10(system: &str) -> bool {
    let s = system.to_ascii_lowercase();
    s.contains("icd-10")
        || s.contains("icd10")
        || s.ends_with("2.16.840.1.113883.6.90")
        || s.ends_with("2.16.840.1.113883.6.3")
}

/// The condition classes a recorded condition belongs to.
pub fn condition_classes<'a>(cv: &'a CvData, text: &str, system: Option<&str>, code: Option<&str>) -> Vec<&'a str> {
    let parts = clauses(text);
    let code = code.map(|c| c.replace('.', "").to_ascii_uppercase());
    cv.conditions
        .iter()
        .filter(|c| {
            // an exclusion holds only inside its own clause, and a `not_terms` one only before the term:
            // "riesgo de diabetes" is not diabetes, but "diabetes con riesgo de complicaciones" and
            // "diabetes mellitus tipo 2: riesgo de hipoglucemia" are. A qualifier inside a longer
            // `not_terms` phrase is that phrase: "family history" excludes its whole clause, but
            // "family history of stroke" only what follows it, and "no family history" nothing before it
            let qualified = |ws: &[String], q: &str| {
                let ql = words(q).len();
                phrase_at(ws, q).into_iter().any(|at| {
                    !c.not_terms.iter().any(|n| {
                        let nl = words(n).len();
                        nl > ql && phrase_at(ws, n).into_iter().any(|s| s <= at && at + ql <= s + nl)
                    })
                })
            };
            let by_text = parts.iter().any(|ws| {
                !c.qualifiers.iter().any(|q| qualified(ws, q))
                    && c.terms.iter().flat_map(|t| phrase_at(ws, t)).any(|at| {
                        !c.not_terms
                            .iter()
                            .any(|n| phrase_at(ws, n).into_iter().any(|before| before < at))
                    })
            });
            let by_code = system.is_some_and(is_icd10)
                && code
                    .as_deref()
                    .is_some_and(|k| c.icd10.iter().any(|p| k.starts_with(&p.replace('.', ""))));
            by_text || by_code
        })
        .map(|c| c.id.as_str())
        .collect()
}

// ------------------------------------------------------------------ facts ----

/// One recorded fact: what it says, from when, and its source.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub value: String,
    pub date: Option<NaiveDate>,
    pub src: String,
    /// the words as recorded, for a smoking status whose code is not known
    pub printed: Option<String>,
}

/// Systolic pressure: an office reading, or a wearable mean over a window.
#[derive(Debug, Clone)]
pub enum Sbp {
    Office(Obs),
    Wearable {
        mean: Decimal,
        since: NaiveDate,
        until: NaiveDate,
        days: usize,
        /// the wearable_source the daily means came from: wearables have no src id
        source: i16,
    },
}

impl Sbp {
    fn value(&self) -> Option<Decimal> {
        match self {
            Sbp::Office(o) => o.value.filter(|_| o.comparator.is_none()),
            Sbp::Wearable { mean, .. } => Some(*mean),
        }
    }

    fn date(&self) -> NaiveDate {
        match self {
            Sbp::Office(o) => o.date,
            Sbp::Wearable { until, .. } => *until,
        }
    }
}

/// What the risk calculators read beside the labs.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub country: Option<String>,
    pub smoking: Option<Fact>,
    pub sbp: Option<Sbp>,
    /// condition class -> the newest matching condition
    pub conditions: BTreeMap<String, Fact>,
    /// medicine class -> the first matching active medicine
    pub drugs: BTreeMap<String, Fact>,
    /// the scopes of `NEEDS` the caller lacks: then nothing above but the country was read
    pub withheld: Vec<String>,
}

/// SBP: an office reading counts for 12 months; the wearable mean covers the last 30 days.
const OFFICE_DAYS: i64 = 365;
const WEARABLE_DAYS: i64 = 30;

/// Read the facts of one profile on `as_of`, only from the sources `scopes` may read.
pub async fn facts(
    conn: &mut PgConnection,
    cv: &CvData,
    profile_id: Uuid,
    country: Option<&str>,
    as_of: NaiveDate,
    scopes: &Scopes,
) -> Result<Facts> {
    let mut out = Facts {
        country: country.map(str::trim).filter(|c| !c.is_empty()).map(str::to_string),
        withheld: NEEDS
            .iter()
            .filter(|s| !scopes.contains(**s))
            .map(|s| (*s).to_string())
            .collect(),
        ..Facts::default()
    };
    if !out.withheld.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "select o.alias_group, o.value_num, o.value_cmp, o.value_code, o.printed_value, \
         coalesce(o.unit_ucum, o.printed_unit) as unit, o.effective_at, o.src_id, o.method \
         from observation o where o.profile_id = $1 and {} and o.effective_at < $3 \
         and o.alias_group in ('smoking_status', 'bp_systolic') and cardinality(o.sensitivity) = 0 \
         order by o.effective_at desc, o.recorded_at desc, o.id desc",
        visible_sql("o", "$2")
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(None::<chrono::DateTime<chrono::Utc>>)
        .bind(day_end(as_of))
        .fetch_all(&mut *conn)
        .await?;
    let office_from = as_of - Duration::days(OFFICE_DAYS - 1);
    for r in rows {
        let group: String = r.try_get("alias_group")?;
        let effective: chrono::DateTime<chrono::Utc> = r.try_get("effective_at")?;
        let date = effective.date_naive();
        let src: String = r.try_get("src_id")?;
        if group == "smoking_status" && out.smoking.is_none() {
            // the newest answer counts, whatever its code: an unknown one is refused, not skipped
            let code: Option<String> = r.try_get("value_code")?;
            let known = code.as_deref().and_then(|c| SMOKING.iter().find(|(k, _)| *k == c));
            out.smoking = Some(Fact {
                value: known.map_or(NOT_CODED, |(_, v)| *v).to_string(),
                date: Some(date),
                src,
                printed: known
                    .is_none()
                    .then(|| r.try_get::<String, _>("printed_value"))
                    .transpose()?,
            });
        } else if group == "bp_systolic" && out.sbp.is_none() && date >= office_from {
            out.sbp = Some(Sbp::Office(Obs {
                group,
                value: r.try_get("value_num")?,
                comparator: r.try_get("value_cmp")?,
                printed: r.try_get("printed_value")?,
                unit: r.try_get("unit")?,
                date,
                src,
                method: r.try_get("method")?,
            }));
        }
    }
    if out.sbp.is_none() && scopes.contains("read:wearables") {
        let metrics = vec!["bp_systolic".to_string()];
        if let Some((source, days)) = crate::engine::wearables(&mut *conn, profile_id, as_of, &metrics, WEARABLE_DAYS)
            .await?
            .remove("bp_systolic")
        {
            let since = as_of - Duration::days(WEARABLE_DAYS - 1);
            let vals: Vec<(NaiveDate, Decimal)> = days
                .iter()
                .filter(|d| d.0 >= since)
                .filter_map(|d| d.1.and_then(from_f64).map(|v| (d.0, v)))
                .collect();
            if let Some(until) = vals.iter().map(|v| v.0).max() {
                let sum: Decimal = vals.iter().map(|v| v.1).sum();
                out.sbp = Some(Sbp::Wearable {
                    mean: quantize(sum / Decimal::from(vals.len()), 1),
                    since,
                    until,
                    days: vals.len(),
                    source,
                });
            }
        }
    }
    for c in list_records(&mut *conn, "condition", profile_id, None, false).await? {
        let s = |k: &str| c.get(k).and_then(Value::as_str);
        if matches!(s("verification_status"), Some("refuted" | "entered-in-error")) {
            continue;
        }
        let day = ["onset", "effective_at", "recorded_at"]
            .iter()
            .find_map(|k| s(k))
            .and_then(|v| NaiveDate::parse_from_str(v.get(..10)?, "%Y-%m-%d").ok());
        if day.is_some_and(|d| d > as_of) {
            continue;
        }
        for class in condition_classes(cv, s("text").unwrap_or_default(), s("code_system"), s("code")) {
            let newer = out.conditions.get(class).is_none_or(|f| f.date < day);
            if newer {
                out.conditions.insert(
                    class.to_string(),
                    Fact {
                        value: "recorded".into(),
                        date: day,
                        src: s("src_id").unwrap_or_default().to_string(),
                        printed: None,
                    },
                );
            }
        }
    }
    for m in active_medications(&mut *conn, profile_id, Some(as_of), None, Some("medication"), false).await? {
        let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or_default();
        let started = NaiveDate::parse_from_str(s("started"), "%Y-%m-%d").ok();
        for class in drug_class(cv, &[s("name"), s("ingredient")]) {
            out.drugs.entry(class.to_string()).or_insert_with(|| Fact {
                value: "recorded".into(),
                date: started,
                src: s("src_id").to_string(),
                printed: None,
            });
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ evaluating ----

/// The age ranges of the equations, inclusive.
fn age_range(id: &str) -> (i32, i32) {
    match id {
        "score2" => (40, 69),
        "score2_op" => (70, 89),
        _ => (30, 79),
    }
}

/// PREVENT's input ranges (Khan 2024): (group, unit as printed, low, high). Cholesterol in mmol/L
/// is the same range converted, as preventr 0.12.2 gives it.
const PREVENT_RANGES: [(&str, &str, i64, u32, i64, u32); 6] = [
    ("total_cholesterol", "mg/dL", 130, 0, 320, 0),
    ("total_cholesterol", "mmol/L", 336, 2, 828, 2),
    ("hdl_c", "mg/dL", 20, 0, 100, 0),
    ("hdl_c", "mmol/L", 52, 2, 259, 2),
    ("bp_systolic", "mm[Hg]", 90, 0, 200, 0),
    ("egfr", "mL/min/{1.73_m2}", 15, 0, 140, 0),
];

/// ESC 2021: markedly raised single risk factors put a person at high risk without SCORE2:
/// (group, unit as printed, limit, inclusive). TC > 8 mmol/L (310 mg/dL), LDL-C > 4.9 mmol/L
/// (190 mg/dL), SBP >= 180 mmHg.
const MARKED: [(&str, &str, i64, u32, bool); 5] = [
    ("total_cholesterol", "mmol/L", 8, 0, false),
    ("total_cholesterol", "mg/dL", 310, 0, false),
    ("ldl_c", "mmol/L", 49, 1, false),
    ("ldl_c", "mg/dL", 190, 0, false),
    ("bp_systolic", "mm[Hg]", 180, 0, true),
];

/// A unit as a person reads it, for the reason text.
fn shown_unit(u: &str) -> &str {
    match u {
        "mm[Hg]" => "mmHg",
        "mL/min/{1.73_m2}" => "mL/min/1.73 m²",
        other => other,
    }
}

fn fact_json(group: &str, f: Option<&Fact>) -> Value {
    let mut m = Map::new();
    m.insert("group".into(), json!(group));
    match f {
        Some(f) => {
            m.insert("value".into(), json!(f.value));
            m.insert("unit".into(), Value::Null);
            if let Some(d) = f.date {
                m.insert("date".into(), json!(iso(d)));
            }
            m.insert("src".into(), json!(f.src));
        }
        None => {
            m.insert("value".into(), json!("not_recorded"));
            m.insert("unit".into(), Value::Null);
        }
    }
    Value::Object(m)
}

fn sbp_json(s: &Sbp) -> Value {
    match s {
        Sbp::Office(o) => input_json(o, None),
        Sbp::Wearable {
            mean,
            since,
            until,
            days,
            source,
        } => json!({
            "group": "bp_systolic",
            "value": num(mean),
            "unit": "mm[Hg]",
            "date": iso(*until),
            "window": {"since": iso(*since), "until": iso(*until), "days": days},
            "wearable_source": source,
        }),
    }
}

/// The newest computed eGFR among the slice-9 items (creatinine-cystatin C first on a tie).
pub fn newest_egfr(items: &[Value]) -> Option<&Value> {
    items
        .iter()
        .filter(|i| i["status"] == "ok" && i["id"].as_str().is_some_and(|id| id.starts_with("ckd_epi_2021")))
        .max_by(|a, b| {
            (a["date"].as_str(), a["id"] == "ckd_epi_2021_cr_cys")
                .cmp(&(b["date"].as_str(), b["id"] == "ckd_epi_2021_cr_cys"))
        })
}

fn egfr_json(e: &Value) -> Value {
    json!({
        "group": "egfr",
        "value": e["value"],
        "unit": e["unit"],
        "date": e["date"],
        "src": e["src"][0],
        "calc": e["id"],
    })
}

/// The newest draw that holds total and HDL cholesterol from one day.
/// A draw's date is its newest value of any group, so the pair is chosen by its own date; the later
/// draw wins a tie.
fn lipid_draw(draws: &[Draw]) -> Option<(&Obs, &Obs)> {
    draws
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let tc = d.obs.get("total_cholesterol")?;
            let hdl = d.obs.get("hdl_c")?;
            (tc.date == hdl.date).then_some((i, tc, hdl))
        })
        .max_by_key(|(i, tc, _)| (tc.date, *i))
        .map(|(_, tc, hdl)| (tc, hdl))
}

/// A lab value read for a formula: (value in the formula's unit, the input line, a limitation).
type Lab = (Decimal, Value, Option<String>);

fn read_lab(data: &Data, o: &Obs) -> std::result::Result<Lab, Refusal> {
    let value = match (o.value, &o.comparator) {
        (Some(v), None) => v,
        _ => return Err(("not_a_number", json!({"group": o.group, "printed": o.printed}))),
    };
    let unit = o.unit.as_deref().unwrap_or_default();
    let row = units::find(&data.units, &o.group, unit)
        .ok_or_else(|| ("unit_unsupported", json!({"group": o.group, "unit": unit})))?;
    let v = units::convert(row, value).ok_or(("out_of_range", json!({})))?;
    if v <= Decimal::ZERO {
        return Err(("out_of_range", json!({})));
    }
    let converted = (row.divide != Decimal::ONE || row.unit != row.to).then_some((v, row.to.as_str()));
    Ok((v, input_json(o, converted), row.limitation.clone()))
}

fn src_of(inputs: &[Value]) -> Vec<Value> {
    inputs.iter().filter_map(|i| i.get("src").cloned()).collect()
}

fn refuse(calc: &Calc, status: &str, r: Refusal, as_of: NaiveDate, inputs: Vec<Value>) -> Vec<Value> {
    let mut v = refused(calc, status, r, Some(as_of), inputs);
    if let Some(m) = v.as_object_mut() {
        let src = src_of(
            m.get("inputs")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        );
        m.insert("src".into(), Value::Array(src));
    }
    vec![v]
}

/// The pure inputs of one risk result, read for the vector tests: printed lipids, then the rest.
#[derive(Debug, Clone, Copy)]
pub struct Factors<'a> {
    pub region: Option<&'a str>,
    pub smoker: bool,
    pub sbp: Decimal,
    pub diabetes: bool,
    pub egfr: Decimal,
    pub bp_treated: bool,
    pub statin: bool,
}

/// The formula on values already in its units. None when it cannot be computed.
pub fn compute(
    cv: &CvData,
    id: &str,
    tc: Decimal,
    hdl: Decimal,
    age: u32,
    sex: Sex,
    f: &Factors<'_>,
) -> Option<Decimal> {
    let sex_key = match sex {
        Sex::Female => "female",
        Sex::Male => "male",
    };
    match id {
        "score2" | "score2_op" => {
            let region = cv.regions.iter().find(|r| Some(r.region.as_str()) == f.region)?;
            let scale = *region.scales.get(&(id.to_string(), sex_key.to_string()))?;
            let i = score2::Inputs {
                age,
                sex,
                smoker: f.smoker,
                sbp: f.sbp,
                total_chol_mmol_l: tc,
                hdl_mmol_l: hdl,
            };
            if id == "score2" {
                score2::score2(&i, scale)
            } else {
                score2::score2_op(&i, scale)
            }
        }
        "prevent_cvd" | "prevent_ascvd" => {
            let outcome = if id == "prevent_cvd" {
                Outcome::Cvd
            } else {
                Outcome::Ascvd
            };
            prevent::risk(
                outcome,
                &prevent::Inputs {
                    age,
                    sex,
                    total_chol_mmol_l: tc,
                    hdl_mmol_l: hdl,
                    sbp: f.sbp,
                    diabetes: f.diabetes,
                    smoker: f.smoker,
                    egfr: f.egfr,
                    bp_treated: f.bp_treated,
                    statin: f.statin,
                },
            )
        }
        _ => None,
    }
}

/// The unrounded result for printed lipids `(group, value, unit)` through the units file: what the
/// vectors in `tests/fixtures/calc/` pin. Err names the group whose unit has no row.
pub fn raw(
    data: &Data,
    cv: &CvData,
    id: &str,
    printed: &[(&str, Decimal, &str)],
    age: u32,
    sex: Sex,
    f: &Factors<'_>,
) -> std::result::Result<Option<Decimal>, String> {
    let mut v = BTreeMap::new();
    for (group, value, unit) in printed {
        let row = units::find(&data.units, group, unit).ok_or_else(|| format!("{group} in {unit}"))?;
        v.insert(
            *group,
            units::convert(row, *value).ok_or_else(|| format!("{group} divide"))?,
        );
    }
    let (Some(tc), Some(hdl)) = (v.get("total_cholesterol"), v.get("hdl_c")) else {
        return Ok(None);
    };
    Ok(compute(cv, id, *tc, *hdl, age, sex, f))
}

/// The one item of a risk calculator on `as_of`, or none (SCORE2 below 70 gives no SCORE2-OP item,
/// and the other way round). `computed` holds the other calculators' items, for eGFR.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    data: &Data,
    cv: &CvData,
    calc: &Calc,
    draws: &[Draw],
    person: &Person,
    facts: &Facts,
    computed: &[Value],
    as_of: NaiveDate,
) -> Vec<Value> {
    let mut out = evaluate_one(data, cv, calc, draws, person, facts, computed, as_of);
    // a SCORE2 result names its region, refused or not, once the country gives one
    if calc.id.starts_with("score2")
        && let Some(r) = facts.country.as_deref().and_then(|c| cv.region_of(c))
    {
        for it in &mut out {
            if let Some(m) = it.as_object_mut() {
                m.insert("region".into(), json!(r.region));
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn evaluate_one(
    data: &Data,
    cv: &CvData,
    calc: &Calc,
    draws: &[Draw],
    person: &Person,
    facts: &Facts,
    computed: &[Value],
    as_of: NaiveDate,
) -> Vec<Value> {
    let id = calc.id.as_str();
    let is_score2 = id.starts_with("score2");
    let age = person.age_on(as_of);
    // SCORE2 below 70, SCORE2-OP from 70: one of the two, never both
    let applies = match (id, age) {
        ("score2", a) => a.is_none_or(|a| a < 70),
        ("score2_op", a) => a.is_some_and(|a| a >= 70),
        _ => true,
    };
    if !applies {
        return Vec::new();
    }
    // a caller who may not read conditions, medicines or vitals learns nothing of them here
    if !facts.withheld.is_empty() {
        let why = json!({"scopes": facts.withheld});
        return refuse(calc, "not_assessed", ("scope_missing", why), as_of, Vec::new());
    }
    // every line the result would read, gathered first so a refusal shows them too
    let lipids = lipid_draw(draws);
    let mut inputs: Vec<Value> = Vec::new();
    if let Some((tc, hdl)) = lipids {
        inputs.push(input_json(tc, None));
        inputs.push(input_json(hdl, None));
    }
    if let Some(s) = &facts.sbp {
        inputs.push(sbp_json(s));
    }
    if let Some(s) = &facts.smoking {
        let mut line =
            json!({"group": "smoking_status", "value": s.value, "unit": null, "date": s.date.map(iso), "src": s.src});
        if let (Some(p), Some(m)) = (&s.printed, line.as_object_mut()) {
            m.insert("printed".into(), json!(p));
        }
        inputs.push(line);
    }
    let egfr = newest_egfr(computed);
    if !is_score2 && let Some(e) = egfr {
        inputs.push(egfr_json(e));
    }
    let scope: &[&str] = if is_score2 { &SCORE2_SCOPE } else { &PREVENT_SCOPE };
    let mut shown: Vec<&str> = scope.to_vec();
    if !is_score2 {
        shown.insert(1, "diabetes");
    }
    for c in &shown {
        inputs.push(fact_json(c, facts.conditions.get(*c)));
    }
    if !is_score2 {
        for d in ["antihypertensive", "statin"] {
            inputs.push(fact_json(d, facts.drugs.get(d)));
        }
    }

    if person.sex.is_none() {
        return refuse(calc, "refused", ("sex_not_recorded", json!({})), as_of, inputs);
    }
    let Some(age) = age else {
        return refuse(calc, "refused", ("birth_date_not_recorded", json!({})), as_of, inputs);
    };
    let (low, high) = age_range(id);
    if age < low || age > high {
        let why = json!({"low": low, "high": high, "age": age, "on": iso(as_of)});
        return refuse(calc, "refused", ("age_out_of_range", why), as_of, inputs);
    }
    let mut region = None;
    if is_score2 {
        let Some(country) = facts.country.as_deref() else {
            return refuse(calc, "refused", ("country_not_recorded", json!({})), as_of, inputs);
        };
        match cv.region_of(country) {
            Some(r) => region = Some(r.region.as_str()),
            None => {
                let why = json!({"country": country});
                return refuse(calc, "refused", ("region_not_covered", why), as_of, inputs);
            }
        }
    }
    let code = if is_score2 {
        "outside_score2_scope"
    } else {
        "outside_prevent_scope"
    };
    if let Some(c) = scope.iter().find(|c| facts.conditions.contains_key(**c)) {
        return refuse(calc, "refused", (code, json!({"condition": c})), as_of, inputs);
    }
    if is_score2
        && let Some(e) = egfr
        && e["value"]
            .as_str()
            .and_then(|v| v.parse::<Decimal>().ok())
            .is_some_and(|v| v < Decimal::from(60))
    {
        let why = json!({"value": e["value"], "on": e["date"]});
        let mut with = inputs.clone();
        with.push(egfr_json(e));
        return refuse(calc, "refused", ("egfr_below_60", why), as_of, with);
    }
    let mut missing: Vec<&str> = Vec::new();
    if lipids.is_none() {
        let tc = draws.iter().any(|d| d.obs.contains_key("total_cholesterol"));
        let hdl = draws.iter().any(|d| d.obs.contains_key("hdl_c"));
        if tc && hdl {
            // the newest of each, first, so the person sees which draws they came from
            let newest = |g: &str| {
                draws
                    .iter()
                    .rev()
                    .find_map(|d| d.obs.get(g))
                    .map(|o| input_json(o, None))
            };
            let mut with: Vec<Value> = ["total_cholesterol", "hdl_c"]
                .iter()
                .filter_map(|g| newest(g))
                .collect();
            with.extend(inputs);
            let why = json!({"groups": calc.inputs});
            return refuse(calc, "refused", ("inputs_not_same_draw", why), as_of, with);
        }
        if !tc {
            missing.push("total_cholesterol");
        }
        if !hdl {
            missing.push("hdl_c");
        }
    }
    if facts.sbp.is_none() {
        missing.push("bp_systolic");
    }
    if facts.smoking.is_none() {
        missing.push("smoking_status");
    }
    if !is_score2 && egfr.is_none() {
        missing.push("egfr");
    }
    if !missing.is_empty() {
        return refuse(
            calc,
            "not_assessed",
            ("missing_input", json!({"groups": missing})),
            as_of,
            inputs,
        );
    }
    if let Some(s) = facts.smoking.as_ref().filter(|s| s.value == NOT_CODED) {
        let why = json!({"printed": s.printed.clone().unwrap_or_default(), "on": s.date.map(iso)});
        return refuse(calc, "refused", ("smoking_status_not_coded", why), as_of, inputs);
    }
    let (Some((tc_o, hdl_o)), Some(sbp)) = (lipids, &facts.sbp) else {
        return Vec::new();
    };
    let mut limitations: Vec<String> = Vec::new();
    let mut labs = Vec::new();
    for o in [tc_o, hdl_o] {
        match read_lab(data, o) {
            Ok(l) => labs.push(l),
            Err(r) => return refuse(calc, "refused", r, as_of, inputs),
        }
    }
    // the lipid lines with their converted values
    inputs[0] = labs[0].1.clone();
    inputs[1] = labs[1].1.clone();
    for l in labs.iter().filter_map(|l| l.2.clone()) {
        if !limitations.contains(&l) {
            limitations.push(l);
        }
    }
    let Some(sbp_v) = sbp.value() else {
        let why = json!({"group": "bp_systolic", "printed": match sbp { Sbp::Office(o) => o.printed.clone(), _ => String::new() }});
        return refuse(calc, "refused", ("not_a_number", why), as_of, inputs);
    };
    let sbp_unit = match sbp {
        Sbp::Office(o) => o.unit.clone().unwrap_or_default(),
        Sbp::Wearable { .. } => "mm[Hg]".into(),
    };
    if sbp_unit != "mm[Hg]" {
        let why = json!({"group": "bp_systolic", "unit": sbp_unit});
        return refuse(calc, "refused", ("unit_unsupported", why), as_of, inputs);
    }
    // a pressure of zero or below is no reading, and no equation takes it
    if sbp_v <= Decimal::ZERO {
        return refuse(calc, "refused", ("out_of_range", json!({})), as_of, inputs);
    }
    let egfr_v = egfr.and_then(|e| e["value"].as_str()?.parse::<Decimal>().ok());
    if is_score2 {
        // every LDL-C and total cholesterol on the newest day of its own, in any draw order: a newer
        // total cholesterol with no HDL beside it still counts for the rule, while the risk uses the pair
        let newest = |group: &str| -> Vec<&Obs> {
            let on = draws.iter().filter_map(|d| d.obs.get(group)).map(|o| o.date).max();
            draws
                .iter()
                .filter_map(|d| d.obs.get(group))
                .filter(|o| Some(o.date) == on)
                .collect()
        };
        let ldl = newest("ldl_c");
        // any other draw counts, a second one on the pair's own day too
        let tc_newest = newest("total_cholesterol")
            .into_iter()
            .filter(|o| !std::ptr::eq(*o, tc_o));
        // a value printed as above a bound (">5.2") is above the bound, so the bound is judged, and one
        // printed as above the limit itself (">8") is over it; one printed as below a bound can never be
        // markedly raised
        let mut checks: Vec<(&Obs, Decimal, &str, String)> = Vec::new();
        for o in [tc_o].into_iter().chain(tc_newest).chain(ldl) {
            let Some(v) = o.value else { continue };
            let shown = match o.comparator.as_deref() {
                None => num(&v),
                Some(c @ (">" | ">=")) => format!("{c}{}", num(&v)),
                Some(_) => continue,
            };
            checks.push((o, v, o.unit.as_deref().unwrap_or_default(), shown));
        }
        let sbp_line = (sbp_v, sbp.date());
        for (group, unit, m, scale, inclusive) in MARKED {
            let limit = Decimal::new(m, scale);
            let hit = |v: Decimal, above: bool| if inclusive || above { v >= limit } else { v > limit };
            let found = if group == "bp_systolic" {
                hit(sbp_line.0, false).then_some((num(&sbp_line.0), sbp_line.1, None))
            } else {
                checks
                    .iter()
                    .find(|(o, v, u, _)| {
                        o.group == group && *u == unit && hit(*v, o.comparator.as_deref() == Some(">"))
                    })
                    .map(|(o, _, _, shown)| (shown.clone(), o.date, Some(*o)))
            };
            if let Some((v, on, obs)) = found {
                let why =
                    json!({"group": group, "value": v, "unit": shown_unit(unit), "limit": num(&limit), "on": iso(on)});
                let mut with = inputs.clone();
                if let Some(o) = obs.filter(|o| !std::ptr::eq(*o, tc_o)) {
                    with.push(input_json(o, None));
                }
                return refuse(calc, "refused", ("marked_risk_factor", why), as_of, with);
            }
        }
    } else {
        let mut checks: Vec<(&str, Decimal, String)> = vec![("bp_systolic", sbp_v, sbp_unit.clone())];
        for o in [tc_o, hdl_o] {
            checks.push((
                o.group.as_str(),
                o.value.unwrap_or_default(),
                o.unit.clone().unwrap_or_default(),
            ));
        }
        if let (Some(v), Some(e)) = (egfr_v, egfr) {
            checks.push(("egfr", v, e["unit"].as_str().unwrap_or_default().to_string()));
        }
        for (group, v, unit) in &checks {
            if let Some((_, _, lo, ls, hi, hs)) = PREVENT_RANGES.iter().find(|r| r.0 == *group && r.1 == unit) {
                let (lo, hi) = (Decimal::new(*lo, *ls), Decimal::new(*hi, *hs));
                if *v < lo || *v > hi {
                    let why = json!({"group": group, "value": num(v), "unit": shown_unit(unit), "low": num(&lo), "high": num(&hi)});
                    return refuse(calc, "refused", ("input_out_of_range", why), as_of, inputs);
                }
            }
        }
    }
    let sex = person.sex.unwrap_or(Sex::Female);
    let smoker = facts.smoking.as_ref().is_some_and(|s| s.value == "current");
    let f = Factors {
        region,
        smoker,
        sbp: sbp_v,
        diabetes: facts.conditions.contains_key("diabetes"),
        egfr: egfr_v.unwrap_or_default(),
        bp_treated: facts.drugs.contains_key("antihypertensive"),
        statin: facts.drugs.contains_key("statin"),
    };
    let age_u = u32::try_from(age).unwrap_or_default();
    let Some(raw) = compute(cv, id, labs[0].0, labs[1].0, age_u, sex, &f) else {
        return refuse(calc, "refused", ("out_of_range", json!({})), as_of, inputs);
    };
    if matches!(sbp, Sbp::Wearable { .. }) {
        limitations.push("sbp_from_wearable".into());
    }
    if !is_score2 && facts.country.as_deref().is_none_or(|c| !c.eq_ignore_ascii_case("US")) {
        limitations.push("prevent_us_cohorts".into());
    }
    let mut m = item(calc, "ok");
    m.insert("value".into(), json!(num(&quantize(raw, calc.precision))));
    m.insert("unit".into(), json!(calc.unit));
    m.insert("date".into(), json!(iso(as_of)));
    m.insert("person".into(), json!({"age": age, "sex": sex.code()}));
    m.insert("src".into(), Value::Array(src_of(&inputs)));
    m.insert("inputs".into(), Value::Array(inputs));
    m.insert("limitations".into(), json!(limitations));
    vec![Value::Object(m)]
}
