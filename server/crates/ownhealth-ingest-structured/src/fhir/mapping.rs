//! FHIR R4 resource -> internal rows (`ingest/fhir/mapping.py`, architecture.md 9). Pure functions,
//! no database.
//!
//! Codes arrive as received; the received JSON is kept in `fhir`. Each mapper returns a `Mapped`
//! (rows plus the structural checks that decide auto-accept). A resource type with no projection is
//! stored and listed in review as "stored, not projected" by the importer.

use std::collections::BTreeSet;

use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone, Utc};
use ownhealth_core::acceptance::Check;
use ownhealth_core::family::{self, FamilyCondition, MemberIn};
use ownhealth_core::fhir::loinc_of;
use ownhealth_core::observations::SENSITIVITY;
use ownhealth_core::pyjson::py_decimal_str;
use ownhealth_core::records::{
    AllergyIn, Common, ConditionIn, ImmunizationIn, MedCode, MedicationIn, ProcedureIn, RecordIn, VisitIn,
};
use rust_decimal::Decimal;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::py::{self, get_truthy, list, truthy};

pub const LOINC: &str = "http://loinc.org";
pub const UCUM: &str = "http://unitsofmeasure.org";
const SNOMED: &str = "http://snomed.info/sct";
const RXNORM: &str = "http://www.nlm.nih.gov/research/umls/rxnorm";

/// `core.fhir.CODE_SYSTEMS`, read backwards: URL -> our short name.
const SYSTEM_NAMES: [(&str, &str); 6] = [
    ("http://loinc.org", "loinc"),
    ("http://snomed.info/sct", "snomed"),
    ("http://hl7.org/fhir/sid/icd-10", "icd10"),
    ("http://www.nlm.nih.gov/research/umls/rxnorm", "rxnorm"),
    ("http://hl7.org/fhir/sid/cvx", "cvx"),
    ("http://www.whocc.no/atc", "atc"),
];

/// Resources that give context to others and are kept in the bundle blob only.
pub const SUPPORTING: [&str; 7] = [
    "Patient",
    "Composition",
    "Medication",
    "Organization",
    "Practitioner",
    "PractitionerRole",
    "Device",
];

/// v3-ActCode confidentiality / sensitivity labels -> our sensitivity tags.
fn security_tag(code: &str) -> Option<&'static str> {
    Some(match code {
        "PSY" | "PSYTHPN" | "MH" | "SUD" | "ETH" => "mental_health",
        // SDV is sexual assault, abuse or domestic violence: the closest bucket is sexual_health
        "SEX" | "STD" | "HIV" | "SDV" => "sexual_health",
        _ => return None,
    })
}

fn category_of(code: &str) -> Option<&'static str> {
    Some(match code {
        "laboratory" => "laboratory",
        "vital-signs" => "vital-signs",
        "activity" => "activity",
        "survey" | "social-history" => "survey",
        "exam" | "imaging" | "procedure" => "exam",
        _ => return None,
    })
}

const INTERP: [&str; 6] = ["L", "N", "H", "LL", "HH", "A"];

/// One observation row before normalization. `suffix` tells components apart in import_ref.
#[derive(Debug, Clone, PartialEq)]
pub struct ObsRow {
    pub suffix: String,
    pub category: String,
    pub printed_name: String,
    pub printed_value: String,
    pub printed_unit: Option<String>,
    pub printed_ref_range: Option<String>,
    pub value_num: Option<Decimal>,
    pub value_cmp: Option<String>,
    pub value_text: Option<String>,
    pub value_code: Option<String>,
    pub ref_low: Option<Decimal>,
    pub ref_high: Option<Decimal>,
    pub loinc: Option<String>,
    pub unit_ucum: Option<String>,
    pub interpretation: Option<String>,
    pub effective_at: Option<DateTime<FixedOffset>>,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mapped {
    Observations(Vec<ObsRow>),
    Record {
        record: Box<RecordIn>,
        checks: Vec<Check>,
    },
    /// A relative of the profile (FamilyMemberHistory, v0.3).
    Family {
        member: Box<MemberIn>,
        checks: Vec<Check>,
    },
    /// entered-in-error, not-done ...: counted, never projected.
    Skip(String),
    /// Project nothing; queue as stored-not-projected with this note.
    Unmapped(String),
}

fn skip(reason: &str) -> Mapped {
    Mapped::Skip(reason.into())
}

fn unmapped(note: impl Into<String>) -> Mapped {
    Mapped::Unmapped(note.into())
}

// --------------------------------------------------------------- primitives ----

/// Format plus the LOINC mod-10 check digit, so a typo or a shifted column is caught.
pub fn loinc_valid(code: Option<&str>) -> bool {
    let Some(code) = code else { return false };
    let Some((body, check)) = code.split_once('-') else {
        return false;
    };
    if body.is_empty() || body.len() > 7 || check.len() != 1 {
        return false;
    }
    if !body.bytes().chain(check.bytes()).all(|b| b.is_ascii_digit()) {
        return false;
    }
    let mut total = 0u32;
    for (i, ch) in body.bytes().rev().enumerate() {
        let mut d = u32::from(ch - b'0');
        if i % 2 == 0 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        total += d;
    }
    (10 - total % 10) % 10 == u32::from(check.as_bytes()[0] - b'0')
}

fn text_of(v: &Value) -> String {
    py::py_str(Some(v))
}

pub fn concept_text(cc: Option<&Value>) -> Option<String> {
    let cc = cc.filter(|c| truthy(Some(c)))?;
    if let Some(t) = get_truthy(Some(cc), "text") {
        return Some(text_of(t));
    }
    let codings = list(Some(cc), "coding");
    for key in ["display", "code"] {
        if let Some(v) = codings.iter().find_map(|c| get_truthy(Some(c), key)) {
            return Some(text_of(v));
        }
    }
    None
}

/// (short code system name or URL, code) of the preferred system, else the first coding.
pub fn first_coding(cc: Option<&Value>, prefer: Option<&str>) -> (Option<String>, Option<String>) {
    let mut codings: Vec<&Value> = list(cc, "coding").iter().filter(|c| truthy(c.get("code"))).collect();
    if let Some(p) = prefer {
        codings.sort_by_key(|c| c.get("system").and_then(Value::as_str) != Some(p));
    }
    let Some(c) = codings.first() else {
        return (None, None);
    };
    let system = c.get("system").filter(|s| !s.is_null()).map(text_of);
    let system = system.map(|s| {
        SYSTEM_NAMES
            .iter()
            .find(|(url, _)| *url == s)
            .map_or(s.clone(), |(_, name)| (*name).to_string())
    });
    (system, c.get("code").map(text_of))
}

/// FHIR date or dateTime. A date alone becomes 00:00 UTC; a time without zone is invalid FHIR.
pub fn parse_datetime(v: Option<&Value>) -> Option<DateTime<FixedOffset>> {
    let v = v?.as_str().filter(|s| !s.is_empty())?;
    let utc = |d: NaiveDate| -> Option<DateTime<FixedOffset>> {
        if chrono::Datelike::year(&d) < 1 {
            return None;
        }
        Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?).fixed_offset())
    };
    let n = v.chars().count();
    match n {
        4 => utc(NaiveDate::from_ymd_opt(v.trim().parse().ok()?, 1, 1)?),
        7 => utc(NaiveDate::from_ymd_opt(
            v.get(..4)?.trim().parse().ok()?,
            v.get(5..7)?.trim().parse().ok()?,
            1,
        )?),
        10 => utc(py::fromisoformat_date(v)?),
        _ => py::fromisoformat_aware(&v.replace('Z', "+00:00")),
    }
}

pub fn parse_date(v: Option<&Value>) -> Option<NaiveDate> {
    parse_datetime(v).map(|d| d.date_naive())
}

pub fn sensitivity(resource: &Value) -> Vec<String> {
    let tags: BTreeSet<&str> = list(resource.get("meta"), "security")
        .iter()
        .filter_map(|s| s.get("code").and_then(Value::as_str).and_then(security_tag))
        .collect();
    tags.into_iter().map(String::from).collect()
}

pub fn status_code(cc: Option<&Value>, default: &str) -> String {
    list(cc, "coding")
        .iter()
        .find_map(|c| get_truthy(Some(c), "code"))
        .map_or_else(|| default.to_string(), text_of)
}

/// NFKD, ASCII only, lower case, split on anything not a letter or digit, tokens longer than one.
pub fn name_tokens(s: &str) -> BTreeSet<String> {
    let ascii: String = s.nfkd().filter(char::is_ascii).collect::<String>().to_lowercase();
    ascii
        .split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit())
        .filter(|t| t.len() > 1)
        .map(String::from)
        .collect()
}

/// Identity of a bundle against the profile it is imported into: matched | hinted | mismatch | missing.
///
/// Matched needs both the name and the birth date. A different birth date is a mismatch even when
/// the name agrees; one of the two alone is only a hint.
pub fn match_patient(patient: Option<&Value>, display_name: &str, birth_date: Option<NaiveDate>) -> &'static str {
    let Some(patient) = patient else {
        return "missing";
    };
    let mut tokens: BTreeSet<String> = BTreeSet::new();
    for n in list(Some(patient), "name") {
        let mut parts: Vec<String> = list(Some(n), "given").iter().map(text_of).collect();
        for key in ["family", "text"] {
            parts.push(get_truthy(Some(n), key).map(text_of).unwrap_or_default());
        }
        tokens.extend(name_tokens(&parts.join(" ")));
    }
    let want = name_tokens(display_name);
    let name_ok = !tokens.is_empty() && !want.is_empty() && (want.is_subset(&tokens) || tokens.is_subset(&want));
    let pdob = parse_date(patient.get("birthDate"));
    if let (Some(p), Some(b)) = (pdob, birth_date)
        && p != b
    {
        return "mismatch";
    }
    let dob_ok = pdob.is_some() && pdob == birth_date;
    match (name_ok, dob_ok) {
        (true, true) => "matched",
        (true, false) | (false, true) => "hinted",
        _ if !tokens.is_empty() => "mismatch",
        _ => "missing",
    }
}

// ------------------------------------------------------------- observations ----

#[derive(Default)]
struct ValueFields {
    printed_value: String,
    printed_unit: Option<String>,
    value_num: Option<Decimal>,
    value_cmp: Option<String>,
    value_text: Option<String>,
    value_code: Option<String>,
    unit_ucum: Option<String>,
    checks: Vec<Check>,
}

/// Value fields of an Observation or one of its components; None when there is no value.
fn value(src: &Value) -> Option<ValueFields> {
    if let Some(q) = src.get("valueQuantity").filter(|q| !q.is_null())
        && q.get("value").is_some_and(|v| !v.is_null())
    {
        let ucum = (q.get("system").and_then(Value::as_str) == Some(UCUM))
            .then(|| q.get("code").filter(|c| !c.is_null()).map(text_of))
            .flatten();
        let mut checks = Vec::new();
        if ucum.as_deref().is_none_or(str::is_empty) {
            checks.push(Check {
                detail: Some("no UCUM code".into()),
                ..Check::fail("ucum_unit", "unit_unconvertible")
            });
        }
        let comparator = get_truthy(Some(q), "comparator").map(text_of);
        return Some(ValueFields {
            printed_value: format!(
                "{}{}",
                comparator.clone().unwrap_or_default(),
                py::num_text(q.get("value"))
            ),
            printed_unit: get_truthy(Some(q), "unit")
                .or_else(|| get_truthy(Some(q), "code"))
                .map(text_of),
            value_num: py::dec(q.get("value")),
            value_cmp: comparator.filter(|c| ["<", "<=", ">", ">="].contains(&c.as_str())),
            unit_ucum: ucum,
            checks,
            ..Default::default()
        });
    }
    if let Some(cc) = src.get("valueCodeableConcept").filter(|c| !c.is_null()) {
        let txt = concept_text(Some(cc)).unwrap_or_default();
        let code = first_coding(Some(cc), None).1.filter(|c| !c.is_empty());
        return Some(ValueFields {
            value_code: Some(code.unwrap_or_else(|| txt.clone())),
            printed_value: txt,
            ..Default::default()
        });
    }
    for key in [
        "valueString",
        "valueBoolean",
        "valueInteger",
        "valueDateTime",
        "valueTime",
    ] {
        let Some(raw) = src.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let text = text_of(raw);
        if key == "valueInteger" {
            return Some(ValueFields {
                value_num: py::dec(Some(raw)),
                printed_value: text,
                ..Default::default()
            });
        }
        return Some(ValueFields {
            printed_value: if key == "valueBoolean" {
                text.to_lowercase()
            } else {
                text.clone()
            },
            value_text: Some(text),
            ..Default::default()
        });
    }
    None
}

fn ref_range(src: &Value) -> (Option<String>, Option<Decimal>, Option<Decimal>) {
    let Some(rr) = list(Some(src), "referenceRange").first().filter(|r| truthy(Some(r))) else {
        return (None, None, None);
    };
    let low = py::dec(rr.get("low").and_then(|l| l.get("value")));
    let high = py::dec(rr.get("high").and_then(|h| h.get("value")));
    let mut printed = get_truthy(Some(rr), "text").map(text_of);
    if printed.is_none() {
        printed = match (&low, &high) {
            (Some(l), Some(h)) => Some(format!("{} - {}", py_decimal_str(l), py_decimal_str(h))),
            (None, Some(h)) => Some(format!("< {}", py_decimal_str(h))),
            (Some(l), None) => Some(format!("> {}", py_decimal_str(l))),
            (None, None) => None,
        };
    }
    (printed, low, high)
}

fn interp(src: &Value) -> Option<String> {
    list(Some(src), "interpretation").iter().find_map(|cc| {
        list(Some(cc), "coding")
            .iter()
            .filter_map(|c| c.get("code").and_then(Value::as_str))
            .find(|c| INTERP.contains(c))
            .map(String::from)
    })
}

fn obs_row(
    suffix: &str,
    category: &str,
    code: Option<&Value>,
    src: &Value,
    when: Option<DateTime<FixedOffset>>,
) -> Option<ObsRow> {
    let val = value(src)?;
    let name = concept_text(code)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "unnamed".into());
    let loinc = loinc_of(code);
    let mut checks = val.checks;
    if !loinc_valid(loinc.as_deref()) {
        checks.push(Check {
            detail: Some(loinc.clone().unwrap_or_else(|| "no LOINC coding".into())),
            ..Check::fail("loinc_code", "loinc_unknown")
        });
    }
    if when.is_none() {
        checks.push(Check::fail("effective_date", "date_invalid"));
    }
    let (printed_ref_range, ref_low, ref_high) = ref_range(src);
    Some(ObsRow {
        suffix: suffix.into(),
        category: category.into(),
        printed_name: name,
        printed_value: val.printed_value,
        printed_unit: val.printed_unit,
        printed_ref_range,
        value_num: val.value_num,
        value_cmp: val.value_cmp,
        value_text: val.value_text,
        value_code: val.value_code,
        ref_low,
        ref_high,
        loinc,
        unit_ucum: val.unit_ucum,
        interpretation: interp(src),
        effective_at: when,
        checks,
    })
}

fn status(r: &Value) -> Option<String> {
    r.get("status").filter(|s| !s.is_null()).map(text_of)
}

pub fn observation(r: &Value) -> Mapped {
    if let Some(s) = status(r).filter(|s| s == "entered-in-error" || s == "cancelled") {
        return skip(&format!("status_{s}"));
    }
    let cat = list(Some(r), "category")
        .iter()
        .find_map(|cc| category_of(&status_code(Some(cc), "")))
        .unwrap_or("laboratory");
    let effective = get_truthy(Some(r), "effectiveDateTime")
        .or_else(|| get_truthy(Some(r), "effectiveInstant"))
        .or_else(|| get_truthy(r.get("effectivePeriod"), "start"));
    let when = parse_datetime(effective).or_else(|| parse_datetime(r.get("issued")));
    let mut rows = Vec::new();
    if let Some(main) = obs_row("", cat, r.get("code"), r, when) {
        rows.push(main);
    }
    for (i, comp) in list(Some(r), "component").iter().enumerate() {
        if let Some(row) = obs_row(&format!("#component{i}"), cat, comp.get("code"), comp, when) {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        if truthy(r.get("hasMember")) {
            return skip("panel_grouping"); // its members arrive as their own resources
        }
        return unmapped("Observation without a value");
    }
    Mapped::Observations(rows)
}

// ------------------------------------------------------------------ records ----

fn effective(r: &Value, keys: &[&str]) -> Option<DateTime<FixedOffset>> {
    keys.iter()
        .find_map(|k| get_truthy(Some(r), k))
        .and_then(|v| parse_datetime(Some(v)))
}

fn code_check(code: &Option<String>) -> Vec<Check> {
    if code.as_deref().is_some_and(|c| !c.is_empty()) {
        Vec::new()
    } else {
        vec![Check {
            detail: Some("no coded concept".into()),
            ..Check::fail("code", "unmapped")
        }]
    }
}

fn common(profile_id: Uuid, effective_at: Option<DateTime<FixedOffset>>, r: &Value) -> Common {
    Common {
        profile_id,
        effective_at,
        sensitivity: sensitivity(r),
        fhir: Some(r.clone()),
    }
}

fn record(record: RecordIn, checks: Vec<Check>) -> Mapped {
    Mapped::Record {
        record: Box::new(record),
        checks,
    }
}

/// `x.get(a) or (x.get(b) or {}).get(c)`.
fn or_period<'a>(r: &'a Value, direct: &str, period: &str, key: &str) -> Option<&'a Value> {
    get_truthy(Some(r), direct).or_else(|| r.get(period).and_then(|p| p.get(key)))
}

pub fn condition(r: &Value, profile_id: Uuid) -> Mapped {
    let mut ver = status_code(r.get("verificationStatus"), "unconfirmed");
    if ver == "entered-in-error" {
        return skip("status_entered-in-error");
    }
    if !["provisional", "confirmed", "refuted", "unconfirmed", "differential"].contains(&ver.as_str()) {
        ver = "unconfirmed".into();
    }
    let (code_system, code) = first_coding(r.get("code"), Some(SNOMED));
    let Some(text) = concept_text(r.get("code")).filter(|t| !t.is_empty()) else {
        return unmapped("Condition without a code or text");
    };
    let checks = code_check(&code);
    record(
        RecordIn::Condition(ConditionIn {
            common: common(profile_id, effective(r, &["recordedDate", "onsetDateTime"]), r),
            text,
            code_system,
            code,
            clinical_status: status_code(r.get("clinicalStatus"), "active"),
            verification_status: ver,
            onset: parse_date(or_period(r, "onsetDateTime", "onsetPeriod", "start")),
            abatement: parse_date(or_period(r, "abatementDateTime", "abatementPeriod", "start")),
        }),
        checks,
    )
}

pub fn allergy(r: &Value, profile_id: Uuid) -> Mapped {
    if status_code(r.get("verificationStatus"), "") == "entered-in-error" {
        return skip("status_entered-in-error");
    }
    let Some(substance) = concept_text(r.get("code")).filter(|t| !t.is_empty()) else {
        return unmapped("AllergyIntolerance without a substance");
    };
    let (code_system, code) = first_coding(r.get("code"), None);
    let mut reaction = None;
    for rx in list(Some(r), "reaction") {
        let parts: Vec<String> = list(Some(rx), "manifestation")
            .iter()
            .filter_map(|m| concept_text(Some(m)))
            .filter(|p| !p.is_empty())
            .collect();
        reaction = get_truthy(Some(rx), "description")
            .map(text_of)
            .or_else(|| Some(parts.join(", ")).filter(|j| !j.is_empty()));
        if reaction.is_some() {
            break;
        }
    }
    let criticality = r
        .get("criticality")
        .and_then(Value::as_str)
        .filter(|c| ["low", "high", "unable-to-assess"].contains(c))
        .map(String::from);
    let checks = code_check(&code);
    record(
        RecordIn::Allergy(AllergyIn {
            common: common(profile_id, effective(r, &["recordedDate", "onsetDateTime"]), r),
            substance,
            code_system,
            code,
            criticality,
            reaction_text: reaction,
            clinical_status: status_code(r.get("clinicalStatus"), "active"),
            verification_status: status_code(r.get("verificationStatus"), "confirmed"),
        }),
        checks,
    )
}

pub fn immunization(r: &Value, profile_id: Uuid) -> Mapped {
    match status(r).as_deref() {
        Some("entered-in-error") => return skip("status_entered-in-error"),
        Some("not-done") => return unmapped("Immunization not given (status not-done)"),
        _ => {}
    }
    let Some(vaccine) = concept_text(r.get("vaccineCode")).filter(|t| !t.is_empty()) else {
        return unmapped("Immunization without a vaccine code");
    };
    let (code_system, code) = first_coding(r.get("vaccineCode"), None);
    let mut dose = None;
    for pa in list(Some(r), "protocolApplied") {
        if let Some(n) = pa.get("doseNumberPositiveInt").and_then(Value::as_i64) {
            dose = i32::try_from(n).ok();
        }
    }
    let occurred = parse_date(r.get("occurrenceDateTime"));
    let mut checks = code_check(&code);
    if occurred.is_none() {
        checks.push(Check::fail("occurrence", "date_invalid"));
    }
    record(
        RecordIn::Immunization(ImmunizationIn {
            common: common(profile_id, effective(r, &["occurrenceDateTime", "recorded"]), r),
            vaccine,
            code_system,
            code,
            occurred,
            dose_number: dose,
            lot: r.get("lotNumber").filter(|l| !l.is_null()).map(text_of),
        }),
        checks,
    )
}

pub fn procedure(r: &Value, profile_id: Uuid) -> Mapped {
    if let Some(s) = status(r).filter(|s| s == "entered-in-error" || s == "not-done") {
        return skip(&format!("status_{s}"));
    }
    let Some(text) = concept_text(r.get("code")).filter(|t| !t.is_empty()) else {
        return unmapped("Procedure without a code or text");
    };
    let (code_system, code) = first_coding(r.get("code"), Some(SNOMED));
    let performed = or_period(r, "performedDateTime", "performedPeriod", "start");
    let checks = code_check(&code);
    record(
        RecordIn::Procedure(ProcedureIn {
            common: common(profile_id, parse_datetime(performed), r),
            text,
            code_system,
            code,
            performed: parse_date(performed),
        }),
        checks,
    )
}

/// Our own export holds every version, tagged; only the current one is a fact. The export appends
/// its tag after any it stored from an earlier import, so the last one is the authoritative one.
fn not_current(r: &Value) -> bool {
    let lifecycle = list(r.get("meta"), "tag")
        .iter()
        .rfind(|t| t.get("system").and_then(Value::as_str) == Some("urn:ownhealth:lifecycle"))
        .and_then(|t| t.get("code").and_then(Value::as_str));
    matches!(lifecycle, Some("superseded" | "retracted"))
}

/// A visit. One that was cancelled or entered in error never happened; one still planned waits in
/// review as stored. The reason is the first reasonCode, else the visit type; the provider is the
/// service provider, else the first participant, as received.
pub fn encounter(r: &Value, profile_id: Uuid) -> Mapped {
    match status(r).as_deref() {
        Some(s @ ("entered-in-error" | "cancelled")) => return skip(&format!("status_{s}")),
        Some("planned") => return unmapped("Encounter not held yet (status planned)"),
        _ => {}
    }
    if not_current(r) {
        return skip("lifecycle_not_current");
    }
    let period = r.get("period");
    let Some(start) = parse_datetime(get_truthy(period, "start")) else {
        return unmapped("Encounter without a start date");
    };
    let mut checks = Vec::new();
    let raw_end = get_truthy(period, "end");
    let mut end = parse_datetime(raw_end);
    let problem = if raw_end.is_some() && end.is_none() {
        Some("the visit's end date cannot be read")
    } else if end.is_some_and(|e| e < start) {
        Some("the visit ends before it starts")
    } else {
        None
    };
    if let Some(p) = problem {
        end = None;
        checks.push(Check {
            detail: Some(p.into()),
            ..Check::fail("period", "date_invalid")
        });
    }
    let first_text = |key: &str| {
        list(Some(r), key)
            .iter()
            .find_map(|c| concept_text(Some(c)).filter(|t| !t.is_empty()))
    };
    let provider_name = get_truthy(r.get("serviceProvider"), "display")
        .or_else(|| {
            list(Some(r), "participant")
                .iter()
                .find_map(|p| get_truthy(p.get("individual"), "display"))
        })
        .map(text_of)
        .filter(|t| !t.is_empty());
    record(
        RecordIn::Visit(VisitIn {
            common: common(profile_id, Some(start), r),
            class: get_truthy(r.get("class"), "code").map(text_of),
            provider_name,
            period_start: Some(start),
            period_end: end,
            reason_text: first_text("reasonCode").or_else(|| first_text("type")),
        }),
        checks,
    )
}

fn med_status(status: &str) -> Option<&'static str> {
    Some(match status {
        "active" | "intended" => "active",
        "completed" | "stopped" | "cancelled" => "stopped",
        "on-hold" => "on_hold",
        _ => return None,
    })
}

/// Every coding of a medication concept that has a code, as received (`MedicationIn.codes`).
fn med_codes(concept: Option<&Value>) -> Option<Vec<MedCode>> {
    let field = |c: &Value, k: &str| get_truthy(Some(c), k).map(text_of);
    let codes: Vec<MedCode> = list(concept, "coding")
        .iter()
        .filter_map(|c| {
            Some(MedCode {
                system: field(c, "system"),
                // a blank code is junk from the source: drop the coding, keep the bundle
                code: field(c, "code").filter(|s| !s.trim().is_empty())?,
                display: field(c, "display"),
            })
        })
        .collect();
    (!codes.is_empty()).then_some(codes)
}

/// MedicationStatement and MedicationRequest; a request becomes a statement with the source status.
/// `resolve(reference)` returns a contained or bundled resource (for medicationReference).
pub fn medication<'a>(r: &'a Value, profile_id: Uuid, resolve: impl Fn(Option<&str>) -> Option<&'a Value>) -> Mapped {
    let rtype = r.get("resourceType").and_then(Value::as_str).unwrap_or_default();
    let raw_status = status(r);
    if raw_status.as_deref() == Some("entered-in-error") {
        return skip("status_entered-in-error");
    }
    let Some(status) = raw_status.as_deref().and_then(med_status) else {
        return unmapped(format!("{rtype} with status {}", py::py_str(r.get("status"))));
    };
    let mut concept = r.get("medicationCodeableConcept").filter(|c| !c.is_null());
    if concept.is_none() && truthy(r.get("medicationReference")) {
        let reference = r["medicationReference"].get("reference").and_then(Value::as_str);
        concept = resolve(reference).and_then(|m| m.get("code"));
    }
    let Some(name) = concept_text(concept).filter(|t| !t.is_empty()) else {
        return unmapped(format!("{rtype} without a medication code"));
    };
    let (code_system, code) = first_coding(concept, Some(RXNORM));
    let (started, mut stopped, dosage, effective_at) = if rtype == "MedicationStatement" {
        let period = r.get("effectivePeriod");
        let start = get_truthy(period, "start").or_else(|| r.get("effectiveDateTime"));
        let asserted = get_truthy(Some(r), "dateAsserted")
            .or_else(|| get_truthy(period, "start"))
            .or_else(|| r.get("effectiveDateTime"));
        (
            parse_date(start),
            parse_date(period.and_then(|p| p.get("end"))),
            list(Some(r), "dosage"),
            parse_datetime(asserted),
        )
    } else {
        let validity = r
            .get("dispenseRequest")
            .and_then(|d| d.get("validityPeriod"))
            .filter(|v| truthy(Some(v)));
        let start = get_truthy(validity, "start").or_else(|| r.get("authoredOn"));
        (
            parse_date(start),
            if status == "stopped" {
                parse_date(validity.and_then(|v| v.get("end")))
            } else {
                None
            },
            list(Some(r), "dosageInstruction"),
            parse_datetime(r.get("authoredOn")),
        )
    };
    let dose_text = dosage.iter().find_map(|d| get_truthy(Some(d), "text")).map(text_of);
    let schedule_text = dosage
        .iter()
        .find_map(|d| concept_text(d.get("timing").and_then(|t| t.get("code"))).filter(|t| !t.is_empty()));
    let who = if rtype == "MedicationStatement" {
        "informationSource"
    } else {
        "requester"
    };
    // informationSource is whoever reported the statement, often the patient; a requester can be
    // the patient too. A source that names a patient, relative or device is not a prescriber. A
    // display with no reference and no type is kept: in practice it names the clinician. The
    // referenced entry decides first, because IPS documents reference it by fullUrl (urn:uuid:...).
    let prescriber = r
        .get(who)
        .filter(|w| {
            let reference = w.get("reference").and_then(Value::as_str);
            let kind = resolve(reference)
                .and_then(|t| t.get("resourceType"))
                .and_then(Value::as_str)
                .or_else(|| reference.and_then(|r| r.rsplit('/').nth(1)))
                .or_else(|| w.get("type").and_then(Value::as_str));
            !matches!(kind, Some("Patient" | "RelatedPerson" | "Device"))
        })
        .and_then(|w| get_truthy(Some(w), "display"))
        .map(text_of)
        .filter(|t| !t.is_empty());
    let codes = med_codes(concept);
    if status == "stopped" && stopped.is_none() {
        stopped = if rtype == "MedicationStatement" {
            parse_date(r.get("dateAsserted"))
        } else {
            None
        };
    }
    let reason_text = list(Some(r), "reasonCode")
        .iter()
        .find_map(|c| concept_text(Some(c)).filter(|t| !t.is_empty()));
    let checks = code_check(&code);
    record(
        RecordIn::Medication(MedicationIn {
            common: common(profile_id, effective_at, r),
            kind: "medication".into(),
            name,
            ingredient: None,
            strength: None,
            dose_text,
            code_system,
            code,
            started,
            stopped,
            med_status: status.into(),
            reason_text,
            schedule_text,
            prescriber,
            codes,
        }),
        checks,
    )
}

// ------------------------------------------------------------- family (v0.3) ----

/// An age in years (`Age`: unit `a`), rounded to whole years.
fn years(v: Option<&Value>) -> Option<i32> {
    let v = v?;
    let unit = v
        .get("code")
        .or_else(|| v.get("unit"))
        .and_then(Value::as_str)
        .unwrap_or("a");
    if !matches!(unit, "a" | "yr" | "year" | "years") {
        return None;
    }
    let n = v.get("value")?.as_f64()?;
    (0.0..=130.0).contains(&n).then(|| n.round() as i32)
}

/// FamilyMemberHistory -> one relative. The relationship must be an HL7 v3 RoleCode family code we
/// draw; a condition's SNOMED code picks its kind (anything else is `other` with its text).
pub fn family_member_history(r: &Value, profile_id: Uuid) -> Mapped {
    if status(r).as_deref() == Some("entered-in-error") {
        return skip("status_entered-in-error");
    }
    if not_current(r) {
        return skip("lifecycle_not_current");
    }
    let Ok(cat) = family::catalog() else {
        return unmapped("family data not loaded");
    };
    let code = list(r.get("relationship"), "coding")
        .iter()
        .filter(|c| c.get("system").and_then(Value::as_str) == Some(family::ROLE_CODE))
        .find_map(|c| {
            c.get("code")
                .and_then(Value::as_str)
                .filter(|c| cat.relationship(c).is_some())
        });
    let Some(code) = code else {
        return unmapped("FamilyMemberHistory with a relationship we do not draw");
    };
    let mut m = MemberIn::new(profile_id, code);
    m.display_name = get_truthy(Some(r), "name").map(text_of);
    m.sex_at_birth = list(r.get("sex"), "coding")
        .iter()
        .find_map(|c| match c.get("code").and_then(Value::as_str)? {
            s @ ("female" | "male" | "unknown") => Some(s.to_string()),
            "other" => Some("unknown".into()),
            _ => None,
        });
    let born = r
        .get("bornDate")
        .or_else(|| r.get("bornPeriod").and_then(|p| p.get("start")));
    m.born_year = born
        .and_then(Value::as_str)
        .and_then(|s| s.get(..4))
        .and_then(|y| y.parse().ok());
    m.death_age = years(r.get("deceasedAge"));
    m.deceased = m.death_age.is_some()
        || r.get("deceasedBoolean").and_then(Value::as_bool).unwrap_or(false)
        || ["deceasedAge", "deceasedDate", "deceasedString", "deceasedRange"]
            .iter()
            .any(|k| truthy(r.get(*k)));
    for c in list(Some(r), "condition") {
        let (code_system, code) = first_coding(c.get("code"), Some(SNOMED));
        let known = match (code_system.as_deref(), code.as_deref()) {
            (Some("snomed"), Some(code)) => cat.kind_for_code(code),
            _ => None,
        };
        let mut text = concept_text(c.get("code")).filter(|t| !t.is_empty());
        if known.is_some_and(|k| text.as_deref() == Some(k.en.as_str())) {
            text = None; // the name we write ourselves, not a received one
        }
        let kind = known.map_or("other", |k| k.kind.as_str());
        if kind == "other" && text.is_none() {
            return unmapped("FamilyMemberHistory condition without a code or text");
        }
        let approx = c
            .get("onsetString")
            .and_then(Value::as_str)
            .and_then(|s| s.trim().strip_prefix(family::APPROX))
            .and_then(|n| n.trim().parse::<i32>().ok());
        let range = c.get("onsetRange").and_then(|r| years(r.get("low")));
        let (onset_age, onset_approx) = match (years(c.get("onsetAge")), approx.or(range)) {
            (Some(a), _) => (Some(a), false),
            (None, Some(a)) => (Some(a), true),
            _ => (None, false),
        };
        m.conditions.push(FamilyCondition {
            kind: kind.into(),
            text,
            code_system,
            code,
            onset_age,
            onset_approx,
        });
    }
    // our own export carries the exact tags beside the HL7 labels; keep the exact tags, and add any
    // HL7 label they do not cover (we export reproductive as SEX, so SEX is covered by it)
    let mut tags: BTreeSet<String> = list(r.get("meta"), "tag")
        .iter()
        .filter(|t| t.get("system").and_then(Value::as_str) == Some(family::SENSITIVITY_TAG))
        .filter_map(|t| t.get("code").and_then(Value::as_str))
        .filter(|c| SENSITIVITY.contains(c))
        .map(String::from)
        .collect();
    let has_reproductive = tags.contains("reproductive");
    for t in sensitivity(r) {
        if !(t == "sexual_health" && has_reproductive) {
            tags.insert(t);
        }
    }
    m.sensitivity = tags.into_iter().collect();
    m.fhir = Some(r.clone());
    match m.normalized() {
        Ok(m) => Mapped::Family {
            member: Box::new(m),
            checks: Vec::new(),
        },
        Err(e) => unmapped(format!("FamilyMemberHistory not readable: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_death_age_in_months_still_marks_the_relative_deceased() {
        let r = json!({
            "resourceType": "FamilyMemberHistory",
            "status": "completed",
            "relationship": {"coding": [{"system": family::ROLE_CODE, "code": "BRO"}]},
            "deceasedAge": {"value": 3, "unit": "mo", "code": "mo"}
        });
        let Mapped::Family { member, .. } = family_member_history(&r, Uuid::nil()) else {
            panic!("not mapped");
        };
        assert_eq!((member.deceased, member.death_age), (true, None));
    }

    #[test]
    fn an_hl7_label_our_exact_tags_do_not_cover_is_kept() {
        let r = json!({
            "resourceType": "FamilyMemberHistory",
            "status": "completed",
            "meta": {
                "security": [{"system": "http://terminology.hl7.org/CodeSystem/v3-ActCode", "code": "PSY"}],
                "tag": [{"system": family::SENSITIVITY_TAG, "code": "reproductive"}]
            },
            "relationship": {"coding": [{"system": family::ROLE_CODE, "code": "MTH"}]}
        });
        let Mapped::Family { member, .. } = family_member_history(&r, Uuid::nil()) else {
            panic!("not mapped");
        };
        assert_eq!(member.sensitivity, ["mental_health", "reproductive"]);
    }

    #[test]
    fn sdv_imports_as_sexual_health_not_reproductive() {
        let r = json!({"meta": {"security": [{"code": "SDV"}]}});
        assert_eq!(sensitivity(&r), ["sexual_health"]);
    }

    #[test]
    fn loinc_check_digit() {
        for (code, ok) in [
            (Some("718-7"), true),
            (Some("13457-7"), true),
            (Some("4548-4"), true),
            (Some("2345-8"), false),
            (Some("2345"), false),
            (None, false),
        ] {
            assert_eq!(loinc_valid(code), ok, "{code:?}");
        }
    }

    #[test]
    fn match_patient_rules() {
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd);
        let pat = json!({"name": [{"given": ["Lucia"], "family": "Rivera"}], "birthDate": "2017-06-21"});
        assert_eq!(match_patient(Some(&pat), "Lucía Rivera", d(2017, 6, 21)), "matched"); // accents ignored
        assert_eq!(match_patient(Some(&pat), "Lucía Rivera", d(2017, 6, 22)), "mismatch");
        let names = json!({"name": pat["name"]});
        assert_eq!(match_patient(Some(&names), "Lucía Rivera", d(2017, 6, 21)), "hinted");
        let dob = json!({"birthDate": "2017-06-21"});
        assert_eq!(match_patient(Some(&dob), "Lucía Rivera", d(2017, 6, 21)), "hinted");
        let other = json!({"name": [{"family": "Ortega"}]});
        assert_eq!(match_patient(Some(&other), "Lucía Rivera", None), "mismatch");
        assert_eq!(match_patient(None, "Lucía Rivera", None), "missing");
    }

    #[test]
    fn encounters_become_visits() {
        let pid = Uuid::nil();
        let enc = |extra: Value| {
            let mut r = json!({"resourceType": "Encounter", "status": "finished", "class": {"code": "EMER"},
                               "period": {"start": "2026-05-12"}});
            if let (Some(m), Value::Object(e)) = (r.as_object_mut(), extra) {
                m.extend(e);
            }
            encounter(&r, pid)
        };
        let Mapped::Record { record, checks } = enc(json!({
            "type": [{"text": "Emergency visit"}],
            "participant": [{"individual": {"display": "Dra. Ortega"}}]
        })) else {
            panic!("a visit")
        };
        let RecordIn::Visit(v) = *record else { panic!("a visit") };
        assert!(checks.is_empty());
        assert_eq!(
            (v.class.as_deref(), v.reason_text.as_deref(), v.provider_name.as_deref()),
            (Some("EMER"), Some("Emergency visit"), Some("Dra. Ortega"))
        );
        assert_eq!(
            v.period_start.map(|d| d.to_rfc3339()),
            Some("2026-05-12T00:00:00+00:00".into())
        );
        assert_eq!(enc(json!({"status": "cancelled"})), skip("status_cancelled"));
        assert_eq!(
            enc(json!({"status": "entered-in-error"})),
            skip("status_entered-in-error")
        );
        assert!(matches!(enc(json!({"status": "planned"})), Mapped::Unmapped(_)));
        assert!(matches!(
            enc(json!({"period": {"end": "2026-05-12"}})),
            Mapped::Unmapped(_)
        ));
        // an end before the start is dropped and the visit waits in review
        let Mapped::Record { record, checks } = enc(json!({"period": {"start": "2026-05-12", "end": "2026-05-11"}}))
        else {
            panic!("a visit")
        };
        assert!(matches!(*record, RecordIn::Visit(VisitIn { period_end: None, .. })));
        assert_eq!(checks.len(), 1);
        // an end we cannot read is not silently dropped: the visit waits in review
        let Mapped::Record { record, checks } = enc(json!({"period": {"start": "2026-05-12", "end": "not a date"}}))
        else {
            panic!("a visit")
        };
        assert!(matches!(*record, RecordIn::Visit(VisitIn { period_end: None, .. })));
        assert_eq!(checks.len(), 1);
        // our own export keeps every version, tagged: only the last tag counts
        let tag = |code: &str| json!({"system": "urn:ownhealth:lifecycle", "code": code});
        assert_eq!(
            enc(json!({"meta": {"tag": [tag("current"), tag("superseded")]}})),
            skip("lifecycle_not_current")
        );
        assert_eq!(
            enc(json!({"meta": {"tag": [tag("retracted")]}})),
            skip("lifecycle_not_current")
        );
        assert!(matches!(
            enc(json!({"meta": {"tag": [tag("superseded"), tag("current")]}})),
            Mapped::Record { .. }
        ));
    }

    #[test]
    fn dates_and_values() {
        let p = |s: &str| parse_datetime(Some(&json!(s))).map(|d| d.to_rfc3339());
        assert_eq!(p("2015-03"), Some("2015-03-01T00:00:00+00:00".into()));
        assert_eq!(p("2015"), Some("2015-01-01T00:00:00+00:00".into()));
        assert_eq!(p("2026-10-02T07:30:00-04:00"), Some("2026-10-02T07:30:00-04:00".into()));
        assert_eq!(p("2026-10-02T07:30:00Z"), Some("2026-10-02T07:30:00+00:00".into()));
        assert_eq!(p("2026-10-02T07:30:00"), None);
        let Mapped::Observations(rows) = observation(&json!({
            "resourceType": "Observation", "status": "final",
            "code": {"coding": [{"system": LOINC, "code": "4548-4", "display": "Hemoglobin A1c"}]},
            "effectiveDateTime": "2026-05-12T09:00:00+02:00",
            "valueQuantity": {"value": 5.9, "unit": "%", "system": UCUM, "code": "%"},
            "referenceRange": [{"low": {"value": 4.0}, "high": {"value": 5.6}}]
        })) else {
            panic!("an observation")
        };
        let r = &rows[0];
        assert_eq!((r.printed_value.as_str(), r.unit_ucum.as_deref()), ("5.9", Some("%")));
        assert_eq!(r.printed_ref_range.as_deref(), Some("4.0 - 5.6"));
        assert!(r.checks.is_empty());
        assert_eq!(
            name_tokens("Ñandú  O'Brien-x 7"),
            ["brien", "nandu"].iter().map(|s| s.to_string()).collect()
        );
    }
}
