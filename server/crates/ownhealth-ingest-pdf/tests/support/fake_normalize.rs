//! A thin stand-in for lane A's `normalize_row` (0.1.1's `fixtures/labslips/fake_normalize.py`).
//!
//! It knows the analytes of the catalog (names in es/en/de/fr, the units the slips print) and parses
//! values and ranges by locale. Anything it does not know fails a check, so a wrong row can only reach
//! the record through a missing check here, which the eval's false auto-accept count would show.

use std::sync::LazyLock;

use futures::future::BoxFuture;
use ownhealth_core::Result;
use ownhealth_core::acceptance::Check;
use ownhealth_core::observations::ObservationIn;
use ownhealth_normalize::{Normalized, RowIn};
use regex::Regex;
use rust_decimal::Decimal;
use sqlx::PgConnection;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::catalog::{ANALYTES, Analyte, TEXT_RESULT, UNIT_UCUM};

pub const VERSION: &str = "fake-normalize-1";

fn fold(s: &str) -> String {
    let s: String = s
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase();
    // after NFKD the micro sign is already U+03BC, so this replace is a no-op, as in the Python
    s.replace('\u{b5}', "u")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn by_name(name: &str, urine: bool) -> Option<&'static Analyte> {
    let f = fold(name);
    let extra = [
        ("Glucosa en orina", "urine_glucose"),
        ("Proteínas en orina", "urine_protein"),
    ];
    if urine && let Some((_, k)) = extra.iter().find(|(n, _)| fold(n) == f) {
        return ANALYTES.iter().find(|a| a.key == *k);
    }
    // Python dict: a later analyte with the same folded name wins
    ANALYTES
        .iter()
        .rev()
        .find(|a| (a.specimen == "urine") == urine && a.names.iter().any(|n| fold(n) == f))
}

static CMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([<>≤≥]=?)\s*(.*)$").expect("pattern"));
static SPACED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,3}( \d{3})+$").expect("pattern"));
static DOTTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,3}(\.\d{3})+$").expect("pattern"));
static COMMAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{1,3}(,\d{3})+$").expect("pattern"));
static RANGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*([\d.,]+)\s*-\s*([\d.,]+)\s*$").expect("pattern"));
static UPPER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*<\s*([\d.,]+)\s*$").expect("pattern"));
static DIGIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d").expect("pattern"));
static NON_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").expect("pattern"));

/// (value, comparator, reason)
pub fn parse_number(s: &str, lang: &str) -> (Option<Decimal>, Option<String>, Option<&'static str>) {
    let mut s = s.trim().to_string();
    let mut cmp = None;
    if let Some(m) = CMP.captures(&s.clone()) {
        let c = &m[1];
        cmp = Some(match c {
            "≤" => "<=".to_string(),
            "≥" => ">=".to_string(),
            _ => c.to_string(),
        });
        s = m[2].to_string();
    }
    if SPACED.is_match(&s) {
        return (None, cmp, Some("suspect_separator")); // "250 000": thousands with spaces
    }
    if matches!(lang, "es" | "de" | "fr") {
        if DOTTED.is_match(&s) {
            s = s.replace('.', "");
        }
        s = s.replace(',', ".");
    } else if COMMAD.is_match(&s) {
        s = s.replace(',', "");
    }
    match s.parse::<Decimal>() {
        Ok(d) => (Some(d), cmp, None),
        Err(_) => (None, cmp, Some("unparsed")),
    }
}

fn check(name: &str, ok: bool, reason: &str, detail: Option<String>) -> Check {
    Check {
        name: name.into(),
        ok,
        reason: (!ok).then(|| reason.into()),
        detail,
    }
}

pub fn normalize(row: &RowIn<'_>) -> Normalized {
    let mut checks = Vec::new();
    let a = by_name(row.printed_name, row.specimen == Some("urine"));
    let group = match a {
        Some(a) => a.group.to_string(),
        None => format!(
            "uncoded:{}",
            NON_ALNUM.replace_all(&fold(row.printed_name), "_").trim_matches('_')
        ),
    };
    let loinc = a.map(|a| a.loinc.to_string());
    checks.push(check("loinc", loinc.is_some(), "loinc_unknown", None));
    let (mut value_num, mut cmp, mut value_text, mut value_code) = (None, None, None, None);
    let folded = fold(row.printed_value);
    if TEXT_RESULT.iter().any(|t| fold(t) == folded) {
        value_text = Some(row.printed_value.to_string());
        value_code = Some("negative".to_string());
    } else {
        let (v, c, why) = parse_number(row.printed_value, row.lang);
        (value_num, cmp) = (v, c);
        if let Some(why) = why {
            let reason = if why == "suspect_separator" {
                "suspect_separator"
            } else {
                "unit_unconvertible"
            };
            checks.push(check("value", false, reason, Some(format!("value {why}"))));
        }
    }
    let unit = row.printed_unit.filter(|u| !u.is_empty());
    let mut ucum: Option<String> = None;
    match (a, unit) {
        (Some(a), Some(u)) => {
            if a.units.iter().any(|k| fold(k) == fold(u)) {
                ucum = a
                    .ucum
                    .or_else(|| UNIT_UCUM.iter().find(|(k, _)| *k == u).map(|(_, v)| *v))
                    .map(String::from);
            } else if u == "/µL" && a.key == "platelets" {
                ucum = Some("/uL".into());
            }
            checks.push(check("unit", ucum.is_some(), "unit_dimension_mismatch", None));
        }
        (Some(a), None) if a.decimals.is_some() => {
            checks.push(check(
                "unit",
                false,
                "unit_dimension_mismatch",
                Some("no unit printed".into()),
            ));
        }
        _ => {}
    }
    let (mut low, mut high) = (None, None);
    if let Some(r) = row.printed_range.filter(|r| !r.is_empty())
        && value_code.is_none()
    {
        if let Some(m) = RANGE.captures(r) {
            low = parse_number(&m[1], row.lang).0;
            high = parse_number(&m[2], row.lang).0;
        } else if let Some(m) = UPPER.captures(r) {
            high = parse_number(&m[1], row.lang).0;
        } else if !DIGIT.is_match(r) || r.contains(':') {
            // sex-conditioned or text ranges: kept as printed
        } else {
            checks.push(check("range", false, "range_parse", None));
        }
    }
    let observation = ObservationIn {
        profile_id: row.profile_id,
        category: "laboratory".into(),
        source_kind: "clinical".into(),
        printed_name: row.printed_name.into(),
        printed_value: row.printed_value.into(),
        printed_unit: row.printed_unit.map(String::from),
        printed_ref_range: row.printed_range.map(String::from),
        printed_flag: row.printed_flag.map(String::from),
        value_num,
        value_cmp: cmp,
        value_text,
        value_code,
        method: None,
        ref_low: low,
        ref_high: high,
        ref_applies: None,
        specimen: row.specimen.map(String::from),
        loinc,
        alias_group: group,
        unit_ucum: ucum.clone(),
        value_canonical: value_num,
        unit_canonical: ucum,
        interpretation: None,
        derivation_version: VERSION.into(),
        effective_at: row.effective_at,
        document_id: None,
        candidate_row_id: None,
        report_key: None,
        sensitivity: Vec::new(),
        fhir: None,
    };
    Normalized { observation, checks }
}

/// The `Normalizer` hook form.
pub fn normalize_row<'c>(_conn: &'c mut PgConnection, row: &'c RowIn<'c>) -> BoxFuture<'c, Result<Normalized>> {
    Box::pin(async move { Ok(normalize(row)) })
}
