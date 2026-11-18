//! Calculators (`data/calculators/`): published formulas run on a person's recorded lab values. The
//! result is the ownhealth.calculators/v1 document, computed on read and never stored:
//!
//! ```text
//! {schema, profile, as_of, rules: "calculators.yaml <version>", units: "units.yaml <version>",
//!  items, calculators_hash}
//! item = {id, version, status: ok | refused | not_assessed, value, unit, date, person{age, sex},
//!         inputs[{group, value, unit, date, src, converted{value, unit}}], cite, src[..],
//!         reason_code, reason{..}, limitations[..], category{code, low, high, cite}}
//! ```
//!
//! `converted` is an input in the formula's unit, shown to 3 decimals; the formula reads it unrounded.
//!
//! Every input of one result comes from one draw: the observation's `report_key`, else its document
//! and day, else its day. A document alone is no draw: a FHIR bundle or an Apple Health export with
//! no DiagnosticReport holds years of values under one document. A draw whose inputs still carry
//! different days is refused as `inputs_not_same_draw`. A formula runs once per draw that holds all its
//! inputs; with no such draw the calculator gives one item saying why. A failed validity rule is
//! `refused`, a missing input `not_assessed`; neither ever carries a value.
//!
//! The arithmetic is `rust_decimal` (exp and ln through its maths feature), never f64, and the value
//! is quantised to the row's precision. The document holds codes, numbers, dates and ids only:
//! `render` adds the words in one language, so the hash is the same for every caller.

pub mod cardio;
pub mod ckd_epi_2021;
pub mod fib4;
pub mod homa_ir;
pub mod prevent;
pub mod score2;
pub mod units;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::access::{Profile, Scopes, all_scopes, get_profile};
use ownhealth_core::canonical::canonical_json;
use ownhealth_core::lifecycle::visible_sql;
use ownhealth_core::{Error, Result};
use rust_decimal::{Decimal, MathematicalOps};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection, Row};
use uuid::Uuid;

use crate::cited::CitedError;
use crate::engine::{day_end, iso};
use crate::num::{localized, num, quantize};
use crate::wording::imperative_phrases;
use units::UnitRow;

pub const SCHEMA: &str = "ownhealth.calculators/v1";
pub const STATUSES: [&str; 3] = ["ok", "refused", "not_assessed"];
/// The calculators the code computes; a data row with another id is refused at load.
pub const IDS: [&str; 8] = [
    "ckd_epi_2021_cr",
    "ckd_epi_2021_cr_cys",
    "fib4",
    "homa_ir",
    "score2",
    "score2_op",
    "prevent_cvd",
    "prevent_ascvd",
];
/// CKD-EPI is validated in adults.
const ADULT_AGE: i32 = 18;
/// FIB-4 is validated from this age.
const FIB4_MIN_AGE: i32 = 35;

/// The shipped files, compiled in. `tests/calculators.rs` loads the same files from `data/` through
/// `cited::load`.
pub const FILES: [(&str, &str); 2] = [
    (
        "calculators.yaml",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../data/calculators/calculators.yaml"
        )),
    ),
    (
        "units.yaml",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../data/calculators/units.yaml"
        )),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sex {
    Female,
    Male,
}

impl Sex {
    pub fn from_record(s: Option<&str>) -> Option<Sex> {
        match s {
            Some("female") => Some(Sex::Female),
            Some("male") => Some(Sex::Male),
            _ => None,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Sex::Female => "female",
            Sex::Male => "male",
        }
    }
}

/// `x^y` for a positive `x`, through exp and ln.
pub(crate) fn pow(x: Decimal, y: Decimal) -> Option<Decimal> {
    if x == Decimal::ONE {
        return Some(Decimal::ONE);
    }
    if x <= Decimal::ZERO {
        return None;
    }
    y.checked_mul(x.checked_ln()?)?.checked_exp()
}

/// `x^n` for a whole `n`.
pub(crate) fn powi(x: Decimal, n: u32) -> Option<Decimal> {
    x.checked_powu(u64::from(n))
}

/// One calculator row of calculators.yaml.
#[derive(Debug, Clone, Deserialize)]
pub struct Calc {
    pub id: String,
    pub version: String,
    pub inputs: Vec<String>,
    pub shows_on: Vec<String>,
    pub system: String,
    pub unit: String,
    pub precision: u32,
    pub cite: String,
}

/// A published FIB-4 band, from `from_age` on (0 when the row has none).
#[derive(Debug, Clone, Deserialize)]
pub struct Cutoff {
    pub id: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub low: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub high: Decimal,
    #[serde(default)]
    pub from_age: i32,
    pub cite: String,
}

/// The loaded data files.
#[derive(Debug, Clone)]
pub struct Data {
    pub version: String,
    pub units_version: String,
    pub calcs: Vec<Calc>,
    pub cutoffs: Vec<Cutoff>,
    pub units: Vec<UnitRow>,
    pub templates: Value,
}

fn is_https(s: &str) -> bool {
    s.strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/') && !rest.contains(char::is_whitespace))
}

fn header(name: &str, text: &str) -> std::result::Result<(String, Vec<Value>, Value), CitedError> {
    let err = |m: String| CitedError(format!("{name}: {m}"));
    let doc: Value = serde_yaml::from_str(text).map_err(|e| err(e.to_string()))?;
    let version = doc
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| err("no `version`".into()))?
        .to_string();
    let rows = doc.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    for (i, r) in rows.iter().enumerate() {
        if !r.get("cite").and_then(Value::as_str).is_some_and(is_https) {
            return Err(err(format!("rows[{i}] has no https `cite`")));
        }
    }
    Ok((version, rows, doc))
}

/// Read and check both files: every row cited over https, every calculator known to the code, every
/// template with its `en` and `es` text and none of it advice.
pub fn parse(calculators: &str, units_text: &str) -> std::result::Result<Data, CitedError> {
    let (version, rows, doc) = header("calculators.yaml", calculators)?;
    let (units_version, unit_rows, _) = header("units.yaml", units_text)?;
    let err = |m: String| CitedError(format!("calculators.yaml: {m}"));
    let mut calcs = Vec::new();
    let mut cutoffs = Vec::new();
    for r in rows {
        let id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        if id.starts_with("fib4.cutoffs") {
            cutoffs.push(serde_json::from_value::<Cutoff>(r).map_err(|e| err(format!("{id}: {e}")))?);
        } else if IDS.contains(&id.as_str()) {
            calcs.push(serde_json::from_value::<Calc>(r).map_err(|e| err(format!("{id}: {e}")))?);
        } else {
            return Err(err(format!("row `{id}` is no calculator this code computes")));
        }
    }
    for id in IDS {
        if !calcs.iter().any(|c| c.id == id) {
            return Err(err(format!("no row for `{id}`")));
        }
    }
    if !cutoffs.iter().any(|c| c.from_age == 0) {
        return Err(err("no FIB-4 cutoffs without an age".into()));
    }
    let units = unit_rows
        .into_iter()
        .map(|r| serde_json::from_value::<UnitRow>(r).map_err(|e| CitedError(format!("units.yaml: {e}"))))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if let Some(u) = units.iter().find(|u| u.divide <= Decimal::ZERO) {
        return Err(CitedError(format!("units.yaml: {} divides by {}", u.id, u.divide)));
    }
    let templates = doc.get("templates").cloned().unwrap_or(Value::Null);
    for section in templates.as_object().into_iter().flat_map(|m| m.values()) {
        for (key, t) in section.as_object().into_iter().flatten() {
            for lang in ["en", "es"] {
                let text = t
                    .get(lang)
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| err(format!("template {key} has no `{lang}` text")))?;
                if let Some(hit) = imperative_phrases(text).first() {
                    return Err(err(format!("template {key}.{lang} reads as advice ({hit:?})")));
                }
            }
        }
    }
    Ok(Data {
        version,
        units_version,
        calcs,
        cutoffs,
        units,
        templates,
    })
}

/// The shipped data, read once.
pub fn data() -> Result<&'static Data> {
    static DATA: OnceLock<std::result::Result<Data, String>> = OnceLock::new();
    DATA.get_or_init(|| parse(FILES[0].1, FILES[1].1).map_err(|e| e.0))
        .as_ref()
        .map_err(|e| Error::from(CitedError(e.clone())))
}

// ------------------------------------------------------------------ reading ----

/// One recorded value a formula may read.
#[derive(Debug, Clone)]
pub struct Obs {
    pub group: String,
    pub value: Option<Decimal>,
    pub comparator: Option<String>,
    pub printed: String,
    /// the printed unit in UCUM, else as printed
    pub unit: Option<String>,
    pub date: NaiveDate,
    pub src: String,
    /// the method the lab printed or the terminology resolved (`fasting` for a fasting glucose)
    pub method: Option<String>,
}

/// One draw: the values recorded in it, the newest per group.
#[derive(Debug, Clone, Default)]
pub struct Draw {
    pub date: Option<NaiveDate>,
    pub obs: BTreeMap<String, Obs>,
}

/// The draws holding any calculator input, oldest first. Retracted, superseded and sensitive values
/// are left out, as the state leaves them out.
async fn draws(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    known_at: Option<DateTime<Utc>>,
    groups: &[String],
) -> Result<Vec<Draw>> {
    let sql = format!(
        "select o.alias_group, o.value_num, o.value_cmp, o.printed_value, coalesce(o.unit_ucum, o.printed_unit) as unit, \
         o.effective_at, o.src_id, o.method, coalesce(o.report_key, o.document_id::text || ':' || o.effective_at::date::text, o.effective_at::date::text) as draw \
         from observation o where o.profile_id = $1 and {} and o.effective_at < $3 and o.alias_group = any($4) \
         and cardinality(o.sensitivity) = 0 order by o.effective_at, o.recorded_at, o.id",
        visible_sql("o", "$2")
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(known_at)
        .bind(day_end(as_of))
        .bind(groups)
        .fetch_all(&mut *conn)
        .await?;
    let mut by_key: BTreeMap<String, Draw> = BTreeMap::new();
    for r in rows {
        let effective: DateTime<Utc> = r.try_get("effective_at")?;
        let o = Obs {
            group: r.try_get("alias_group")?,
            value: r.try_get("value_num")?,
            comparator: r.try_get("value_cmp")?,
            printed: r.try_get("printed_value")?,
            unit: r.try_get("unit")?,
            date: effective.date_naive(),
            src: r.try_get("src_id")?,
            method: r.try_get("method")?,
        };
        let d = by_key.entry(r.try_get("draw")?).or_default();
        d.date = Some(d.date.map_or(o.date, |x| x.max(o.date)));
        d.obs.insert(o.group.clone(), o);
    }
    let mut out: Vec<(String, Draw)> = by_key.into_iter().collect();
    out.sort_by(|a, b| (a.1.date, &a.0).cmp(&(b.1.date, &b.0)));
    Ok(out.into_iter().map(|(_, d)| d).collect())
}

// ------------------------------------------------------------------ evaluating ----

/// Who the formula is about: what the profile records.
#[derive(Debug, Clone)]
pub struct Person {
    pub birth_date: Option<NaiveDate>,
    pub sex: Option<Sex>,
}

impl Person {
    pub fn of(p: &Profile) -> Person {
        Person {
            birth_date: p.birth_date,
            sex: Sex::from_record(p.sex_at_birth.as_deref()),
        }
    }

    fn age_on(&self, on: NaiveDate) -> Option<i32> {
        use chrono::Datelike;
        let b = self.birth_date?;
        let before_birthday = (on.month(), on.day()) < (b.month(), b.day());
        Some(on.year() - b.year() - i32::from(before_birthday))
    }
}

type Refusal = (&'static str, Value);

fn uses_sex(id: &str) -> bool {
    id.starts_with("ckd_epi")
}

fn uses_age(id: &str) -> bool {
    id.starts_with("ckd_epi") || id == "fib4"
}

/// The validity rules that depend on the person, on one day.
fn person_check(id: &str, person: &Person, on: NaiveDate) -> Option<Refusal> {
    if uses_sex(id) && person.sex.is_none() {
        return Some(("sex_not_recorded", json!({})));
    }
    if !uses_age(id) {
        return None;
    }
    let Some(age) = person.age_on(on) else {
        return Some(("birth_date_not_recorded", json!({})));
    };
    if id.starts_with("ckd_epi") && age < ADULT_AGE {
        return Some(("not_adult", json!({"age": age, "on": iso(on)})));
    }
    if id == "fib4" && age < FIB4_MIN_AGE {
        return Some(("age_below_35", json!({"age": age, "on": iso(on)})));
    }
    None
}

fn input_json(o: &Obs, converted: Option<(Decimal, &str)>) -> Value {
    let mut m = Map::new();
    m.insert("group".into(), json!(o.group));
    // a censored value keeps its comparator: '<5' is never shown as a plain 5
    let value = match (&o.value, &o.comparator) {
        (Some(v), Some(c)) => format!("{c}{}", num(v)),
        (Some(v), None) => num(v),
        (None, _) => o.printed.clone(),
    };
    m.insert("value".into(), json!(value));
    m.insert("unit".into(), json!(o.unit));
    m.insert("date".into(), json!(iso(o.date)));
    m.insert("src".into(), json!(o.src));
    if let Some((v, unit)) = converted {
        m.insert(
            "converted".into(),
            json!({"value": num(&quantize(v, 3).normalize()), "unit": unit}),
        );
    }
    Value::Object(m)
}

fn item(calc: &Calc, status: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), json!(calc.id));
    m.insert("version".into(), json!(calc.version));
    m.insert("status".into(), json!(status));
    m.insert("cite".into(), json!(calc.cite));
    m
}

fn refused(calc: &Calc, status: &str, (code, reason): Refusal, date: Option<NaiveDate>, inputs: Vec<Value>) -> Value {
    let mut m = item(calc, status);
    if let Some(d) = date {
        m.insert("date".into(), json!(iso(d)));
    }
    let src: Vec<Value> = inputs.iter().map(|i| i["src"].clone()).collect();
    m.insert("inputs".into(), Value::Array(inputs));
    m.insert("src".into(), Value::Array(src));
    m.insert("reason_code".into(), json!(code));
    if reason.as_object().is_some_and(|r| !r.is_empty()) {
        m.insert("reason".into(), reason);
    }
    Value::Object(m)
}

/// The formula on values already in its units.
fn compute(id: &str, x: &BTreeMap<&str, Decimal>, age: Option<i32>, sex: Option<Sex>) -> Option<Decimal> {
    let age = age.and_then(|a| u32::try_from(a).ok());
    let v = |g: &str| x.get(g).copied();
    match id {
        "ckd_epi_2021_cr" => ckd_epi_2021::cr(v("creatinine")?, age?, sex?),
        "ckd_epi_2021_cr_cys" => ckd_epi_2021::cr_cys(v("creatinine")?, v("cystatin_c")?, age?, sex?),
        "fib4" => fib4::fib4(age?, v("ast")?, v("alt")?, v("platelets")?),
        "homa_ir" => homa_ir::homa_ir(v("glucose")?, v("insulin")?),
        _ => None,
    }
}

/// The unrounded result for printed values `(group, value, unit)`, through the units file: what the
/// vectors in `tests/fixtures/calc/` pin. Err names the group whose unit has no row.
pub fn raw(
    data: &Data,
    id: &str,
    printed: &[(&str, Decimal, &str)],
    age: Option<i32>,
    sex: Option<Sex>,
) -> std::result::Result<Option<Decimal>, String> {
    let mut values = BTreeMap::new();
    for (group, value, unit) in printed {
        let row = units::find(&data.units, group, unit).ok_or_else(|| format!("{group} in {unit}"))?;
        values.insert(
            *group,
            units::convert(row, *value).ok_or_else(|| format!("{group} divide"))?,
        );
    }
    Ok(compute(id, &values, age, sex))
}

/// One calculator on one draw that holds all its inputs.
pub fn on_draw(data: &Data, calc: &Calc, draw: &Draw, person: &Person) -> Value {
    let found: Vec<&Obs> = calc.inputs.iter().filter_map(|g| draw.obs.get(g)).collect();
    // the date of this calculator's own inputs: another calculator's values in the same report
    // neither move nor refuse it
    let date = found.iter().map(|o| o.date).max().or(draw.date).unwrap_or_default();
    let plain = |found: &[&Obs]| found.iter().map(|o| input_json(o, None)).collect::<Vec<_>>();
    if found.iter().any(|o| o.date != date) {
        let why = json!({"groups": calc.inputs});
        return refused(
            calc,
            "refused",
            ("inputs_not_same_draw", why),
            Some(date),
            plain(&found),
        );
    }
    let mut values = BTreeMap::new();
    let mut inputs = Vec::new();
    let mut limitations: Vec<String> = Vec::new();
    for o in &found {
        let value = match (o.value, &o.comparator) {
            (Some(v), None) => v,
            _ => {
                let why = json!({"group": o.group, "printed": o.printed});
                return refused(calc, "refused", ("not_a_number", why), Some(date), plain(&found));
            }
        };
        let unit = o.unit.as_deref().unwrap_or_default();
        let Some(row) = units::find(&data.units, &o.group, unit) else {
            let why = json!({"group": o.group, "unit": unit});
            return refused(calc, "refused", ("unit_unsupported", why), Some(date), plain(&found));
        };
        let Some(v) = units::convert(row, value) else {
            return refused(calc, "refused", ("out_of_range", json!({})), Some(date), plain(&found));
        };
        let converted = (row.divide != Decimal::ONE || row.unit != row.to).then_some((v, row.to.as_str()));
        inputs.push(input_json(o, converted));
        values.insert(o.group.as_str(), v);
        if let Some(l) = &row.limitation
            && !limitations.contains(l)
        {
            limitations.push(l.clone());
        }
    }
    if let Some(r) = person_check(&calc.id, person, date) {
        return refused(calc, "refused", r, Some(date), inputs);
    }
    let age = person.age_on(date);
    let Some(raw) = compute(&calc.id, &values, age, person.sex) else {
        return refused(calc, "refused", ("out_of_range", json!({})), Some(date), inputs);
    };
    let value = quantize(raw, calc.precision);
    let mut m = item(calc, "ok");
    m.insert("value".into(), json!(num(&value)));
    m.insert(
        "unit".into(),
        if calc.unit.is_empty() {
            Value::Null
        } else {
            json!(calc.unit)
        },
    );
    m.insert("date".into(), json!(iso(date)));
    let mut person_used = Map::new();
    if uses_age(&calc.id) {
        person_used.insert("age".into(), json!(age));
    }
    if uses_sex(&calc.id) {
        person_used.insert("sex".into(), json!(person.sex.map(Sex::code)));
    }
    if !person_used.is_empty() {
        m.insert("person".into(), Value::Object(person_used));
    }
    let src: Vec<Value> = inputs.iter().map(|i| i["src"].clone()).collect();
    m.insert("inputs".into(), Value::Array(inputs));
    m.insert("src".into(), Value::Array(src));
    if calc.id == "fib4" {
        let age = age.unwrap_or_default();
        if let Some(c) = data
            .cutoffs
            .iter()
            .filter(|c| c.from_age <= age)
            .max_by_key(|c| c.from_age)
        {
            let band = if value < c.low {
                "fib4.low"
            } else if value > c.high {
                "fib4.high"
            } else {
                "fib4.mid"
            };
            m.insert(
                "category".into(),
                json!({"code": band, "low": num(&c.low), "high": num(&c.high), "cite": c.cite}),
            );
            if c.from_age > 0 {
                limitations.push("fib4_age_65".into());
            }
        }
    }
    if calc.id == "homa_ir" && draw.obs.get("glucose").and_then(|g| g.method.as_deref()) != Some("fasting") {
        limitations.insert(0, "fasting_not_recorded".into());
    }
    m.insert("limitations".into(), json!(limitations));
    Value::Object(m)
}

/// Every item of one calculator: one per complete draw, or one that says why there is none.
pub fn evaluate(data: &Data, calc: &Calc, draws: &[Draw], person: &Person, as_of: NaiveDate) -> Vec<Value> {
    let complete: Vec<&Draw> = draws
        .iter()
        .filter(|d| calc.inputs.iter().all(|g| d.obs.contains_key(g)))
        .collect();
    if !complete.is_empty() {
        // oldest first by each result's own date (a draw sorts by its newest value of any group);
        // the sort is stable, so draw order breaks ties
        let mut items: Vec<Value> = complete.into_iter().map(|d| on_draw(data, calc, d, person)).collect();
        items.sort_by(|a, b| a["date"].as_str().cmp(&b["date"].as_str()));
        return items;
    }
    // the newest value of each input the record has, in input order
    let newest: Vec<&Obs> = calc
        .inputs
        .iter()
        .filter_map(|g| draws.iter().rev().find_map(|d| d.obs.get(g)))
        .collect();
    let inputs: Vec<Value> = newest.iter().map(|o| input_json(o, None)).collect();
    // the age that counts is the one at the newest input's draw; with no input, on `as_of`
    let on = newest.iter().map(|o| o.date).max().unwrap_or(as_of);
    if let Some(r) = person_check(&calc.id, person, on) {
        return vec![refused(calc, "refused", r, None, inputs)];
    }
    if newest.len() == calc.inputs.len() {
        let why = json!({"groups": calc.inputs});
        return vec![refused(calc, "refused", ("inputs_not_same_draw", why), None, inputs)];
    }
    let missing: Vec<&String> = calc
        .inputs
        .iter()
        .filter(|g| !newest.iter().any(|o| &o.group == *g))
        .collect();
    let why = json!({"groups": missing});
    vec![refused(calc, "not_assessed", ("missing_input", why), None, inputs)]
}

/// The hash of a calculators document, as `state_hash` hashes the state body.
pub fn calculators_hash(doc: &Value) -> Result<String> {
    let mut clean = doc.clone();
    if let Some(m) = clean.as_object_mut() {
        m.shift_remove("calculators_hash");
    }
    let bytes = canonical_json(&clean).map_err(|e| Error::Other(anyhow::Error::new(e)))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// The document from values already read (the pure core of `document`). The lab calculators run
/// first; the cardiovascular ones (`cardio`) then read their eGFR.
pub fn build(
    data: &Data,
    slug: &str,
    as_of: NaiveDate,
    person: &Person,
    draws: &[Draw],
    facts: &cardio::Facts,
) -> Result<Value> {
    let cv = cardio::data()?;
    let lab: Vec<Value> = data
        .calcs
        .iter()
        .filter(|c| !cardio::IDS.contains(&c.id.as_str()))
        .flat_map(|c| evaluate(data, c, draws, person, as_of))
        .collect();
    let mut items = Vec::new();
    for c in &data.calcs {
        if cardio::IDS.contains(&c.id.as_str()) {
            items.extend(cardio::evaluate(data, cv, c, draws, person, facts, &lab, as_of));
        } else {
            items.extend(lab.iter().filter(|i| i["id"] == c.id.as_str()).cloned());
        }
    }
    let mut doc = json!({
        "schema": SCHEMA,
        "profile": slug,
        "as_of": iso(as_of),
        "rules": format!("calculators.yaml {}", data.version),
        "units": format!("units.yaml {}", data.units_version),
        "cardiovascular": cv.versions,
        "items": items,
    });
    let hash = calculators_hash(&doc)?;
    if let Some(m) = doc.as_object_mut() {
        m.insert("calculators_hash".into(), json!(hash));
    }
    Ok(doc)
}

/// The ownhealth.calculators/v1 document of one profile on `as_of`, read with every scope.
pub async fn document(conn: &mut PgConnection, profile_id: Uuid, as_of: NaiveDate) -> Result<Value> {
    document_for(conn, profile_id, as_of, &all_scopes()).await
}

/// The document as a caller with `scopes` may read it: a cardiovascular result reads conditions,
/// medicines, vitals and wearables only under their own scopes (`cardio::facts`). The hash is the same
/// for every caller with the same scopes.
pub async fn document_for(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    scopes: &Scopes,
) -> Result<Value> {
    let data = data()?;
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("profile {profile_id} not found")))?;
    let mut groups: Vec<String> = data.calcs.iter().flat_map(|c| c.inputs.clone()).collect();
    // SCORE2 reads LDL-C for its markedly-raised rule
    groups.push("ldl_c".into());
    groups.sort();
    groups.dedup();
    let draws = draws(&mut *conn, profile_id, as_of, None, &groups).await?;
    let facts = cardio::facts(
        &mut *conn,
        cardio::data()?,
        profile_id,
        profile.country.as_deref(),
        as_of,
        scopes,
    )
    .await?;
    build(data, &profile.slug, as_of, &Person::of(&profile), &draws, &facts)
}

// ------------------------------------------------------------------ words ----

fn template<'a>(data: &'a Data, section: &str, key: &str, lang: &str) -> Option<&'a str> {
    data.templates.get(section)?.get(key)?.get(lang)?.as_str()
}

fn group_name(data: &Data, group: &str, lang: &str) -> String {
    template(data, "group", group, lang).unwrap_or(group).to_string()
}

fn fill(text: &str, params: &Map<String, Value>, data: &Data, lang: &str) -> String {
    let and = if lang == "es" { " y " } else { " and " };
    let mut out = text.to_string();
    for (k, v) in params {
        let shown = match (k.as_str(), v) {
            // "a, b and c": the last two joined by the word, the rest by commas
            ("groups" | "scopes", Value::Array(gs)) => {
                let names: Vec<String> = gs
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|g| {
                        if k == "groups" {
                            group_name(data, g, lang)
                        } else {
                            g.to_string()
                        }
                    })
                    .collect();
                match names.split_last() {
                    Some((last, rest)) if !rest.is_empty() => format!("{}{and}{last}", rest.join(", ")),
                    _ => names.join(and),
                }
            }
            ("group", Value::String(g)) => group_name(data, g, lang),
            ("condition", Value::String(c)) => template(data, "condition", c, lang).unwrap_or(c).to_string(),
            (_, Value::String(s)) => localized(s, lang),
            (_, other) => other.to_string(),
        };
        out = out.replace(&format!("{{{k}}}"), &shown);
    }
    out
}

/// The document with the words in `lang` (es or en): each item gains `name` and `short`, its refusal as `text`,
/// `notes` (the band, then the limitations), the `system` and `shows_on` groups where it is
/// listed, and a `name` on each input. The hash stays the document's.
pub fn render(doc: &Value, lang: &str) -> Result<Value> {
    let data = data()?;
    let lang = if lang == "en" { "en" } else { "es" };
    let mut out = doc.clone();
    if let Some(m) = out.as_object_mut() {
        m.insert("lang".into(), json!(lang));
    }
    for it in out.get_mut("items").and_then(Value::as_array_mut).into_iter().flatten() {
        let Some(m) = it.as_object_mut() else { continue };
        let id = m.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut name = template(data, "name", &id, lang).unwrap_or(&id).to_string();
        // SCORE2's region, as the ESC names it
        if let Some(r) = m
            .get("region")
            .and_then(Value::as_str)
            .and_then(|r| template(data, "region", r, lang))
        {
            name = format!("{name}, {r}");
        }
        m.insert("name".into(), json!(name));
        let short = template(data, "short", &id, lang).unwrap_or(&name).to_string();
        m.insert("short".into(), json!(short));
        // where the UI lists it: the dashboard card and the biomarker pages
        if let Some(c) = data.calcs.iter().find(|c| c.id == id) {
            m.insert("system".into(), json!(c.system));
            m.insert("shows_on".into(), json!(c.shows_on));
        }
        if let Some(code) = m.get("reason_code").and_then(Value::as_str).map(str::to_string) {
            let params = m.get("reason").and_then(Value::as_object).cloned().unwrap_or_default();
            let text = template(data, "reason", &code, lang).unwrap_or(&code);
            m.insert("text".into(), json!(fill(text, &params, data, lang)));
        }
        for i in m.get_mut("inputs").and_then(Value::as_array_mut).into_iter().flatten() {
            let g = i["group"].as_str().unwrap_or_default().to_string();
            let fact = format!("{g}.{}", i["value"].as_str().unwrap_or_default());
            // a line that is no lab value reads as a phrase: "no diabetes recorded", "smokes now", or a
            // wearable mean with its window
            let text = match i.get("window").and_then(Value::as_object) {
                Some(w) => {
                    let mut p = w.clone();
                    p.insert("value".into(), i["value"].clone());
                    template(data, "fact", "bp_systolic.window", lang).map(|t| fill(t, &p, data, lang))
                }
                // a smoking status with no known code shows its words: "{printed}"
                None => template(data, "fact", &fact, lang).map(|t| match i.get("printed") {
                    Some(Value::String(p)) => t.replace("{printed}", p),
                    _ => t.to_string(),
                }),
            };
            if let Some(o) = i.as_object_mut() {
                o.insert("name".into(), json!(group_name(data, &g, lang)));
                if let Some(t) = text {
                    o.insert("text".into(), json!(t));
                }
            }
        }
        let mut notes = Vec::new();
        if let Some(c) = m.get("category").and_then(Value::as_object) {
            let code = c.get("code").and_then(Value::as_str).unwrap_or_default();
            if let Some(t) = template(data, "category", code, lang) {
                notes.push(json!(fill(t, c, data, lang)));
            }
        }
        for l in m.get("limitations").and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = l.as_str().and_then(|l| template(data, "limitation", l, lang)) {
                notes.push(json!(t));
            }
        }
        m.insert("notes".into(), Value::Array(notes));
    }
    Ok(out)
}
