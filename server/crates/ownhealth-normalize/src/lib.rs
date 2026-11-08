//! Lane A: normalization (`normalize/`, docs/design/ingestion.md 8, architecture.md 10).
//!
//! Printed value, unit and range stay the truth layer; everything here is derived and
//! recomputable. Importers (lanes B and C) and `propose` (lane F) call these functions; nobody
//! else parses units.
//!
//! Tables (`data/terminology`, embedded unchanged at build time): a LOINC
//! 2.83 subset (NOTICE.md, LOINC License), the alias groups with canonical units and plausibility
//! limits, aliases in es/en/de/fr, refused panel names, specimen twins and a cited molar-mass table.
//! `derivation_version()` carries a hash of every table, so a table edit changes it and `rederive`
//! recomputes the derived layer.

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{Signed, Zero};
use ownhealth_core::Result;
use ownhealth_core::acceptance::Check;
use ownhealth_core::observations::ObservationIn;
use ownhealth_core::pyjson::py_decimal_str;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub mod resolve;
mod store;
pub mod tables;
pub mod text;
pub mod units;

pub use store::{AliasIn, add_alias, normalize_row, rederive, register};

pub const NORMALIZER_VERSION: &str = "0.1.0";

/// `norm-<NORMALIZER_VERSION>+t<hash of every table>`, the same string Python computes.
pub fn derivation_version() -> String {
    format!("norm-{NORMALIZER_VERSION}+t{}", tables::tables_hash())
}

/// es | en | de | fr
pub type Locale = str;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedValue {
    pub printed: String,
    /// None for text results.
    pub value: Option<Decimal>,
    /// <, >, <=, >=
    pub comparator: Option<String>,
    /// negative | positive | not_detected | trace
    pub text_code: Option<String>,
    /// A review code when the parse is not trusted (ambiguous_separator ...).
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefRange {
    pub low: Option<Decimal>,
    pub high: Option<Decimal>,
    pub text: String,
    /// {sex, age_min, age_max, pregnancy}
    pub applies_to: Value,
    /// "> 90" is exclusive, "90 - 120" inclusive
    pub low_inclusive: bool,
    /// "< 130" is exclusive
    pub high_inclusive: bool,
    /// Expected text result: "Negativo" -> negative
    pub text_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub alias_group: String,
    pub loinc: Option<String>,
    pub property: Option<String>,
    pub method: Option<String>,
    pub confidence: f64,
    /// The alias row that matched, or "loinc" for a given code.
    pub matched: String,
    pub unit_ok: bool,
    pub specimen_ok: bool,
    /// Only part of the printed name matched: never auto-accepted.
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    /// Always set: an uncoded row stays usable under its group.
    pub alias_group: String,
    pub loinc: Option<String>,
    /// MCnc, SCnc, ... of the resolved code
    pub property: Option<String>,
    /// calculated | direct | ...
    pub method: Option<String>,
    /// A panel or category name, never coded as an analyte.
    pub refused: bool,
    /// unmapped name_partial unit_dimension_mismatch method_uncertain
    pub reasons: Vec<String>,
    /// 0..1 for the chosen candidate
    pub confidence: f64,
    /// Every group the name could be, best first.
    pub candidates: Vec<Candidate>,
}

impl Resolution {
    fn unmapped(alias_group: String, refused: bool) -> Resolution {
        Resolution {
            alias_group,
            loinc: None,
            property: None,
            method: None,
            refused,
            reasons: vec!["unmapped".into()],
            confidence: 0.0,
            candidates: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    pub observation: ObservationIn,
    pub checks: Vec<Check>,
}

/// What `resolve_analyte` needs besides the printed name.
#[derive(Debug, Clone, Default)]
pub struct ResolveQuery<'a> {
    pub lang: &'a str,
    /// The unit as printed (or UCUM).
    pub unit: Option<&'a str>,
    pub specimen: Option<&'a str>,
    pub method: Option<&'a str>,
    pub provider: Option<&'a str>,
    /// Adds that household's own aliases.
    pub household_id: Option<Uuid>,
    /// Negativo, trazas: picks the group's ordinal code.
    pub text_result: bool,
}

/// One printed row to normalize (`normalize_row` keyword arguments).
#[derive(Debug, Clone)]
pub struct RowIn<'a> {
    pub profile_id: Uuid,
    pub printed_name: &'a str,
    pub printed_value: &'a str,
    pub printed_unit: Option<&'a str>,
    pub printed_range: Option<&'a str>,
    pub printed_flag: Option<&'a str>,
    pub effective_at: chrono::DateTime<chrono::FixedOffset>,
    pub lang: &'a str,
    pub specimen: Option<&'a str>,
    pub provider: Option<&'a str>,
    /// A code a structured source sent.
    pub loinc: Option<&'a str>,
    pub method: Option<&'a str>,
    /// The source the row comes from: rows of one source are never duplicates.
    pub document_id: Option<Uuid>,
}

// --------------------------------------------------------------------- values ----

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}

pub fn parse_value(printed: &str, locale: &Locale, unit: Option<&str>) -> Result<ParsedValue> {
    let ucum = non_empty(unit).and_then(to_ucum);
    let v = text::parse_value(printed, locale, ucum.as_deref());
    Ok(ParsedValue {
        printed: printed.to_string(),
        value: v.value,
        comparator: v.comparator.map(String::from),
        text_code: v.text_code.map(String::from),
        reason: v.reason.map(String::from),
    })
}

pub fn to_ucum(printed_unit: &str) -> Option<String> {
    units::to_ucum(Some(printed_unit))
}

pub fn canonical_unit(alias_group: &str) -> Option<String> {
    tables::groups().get(alias_group).map(|g| g.canonical.clone())
}

/// `{canonical, si, conventional}` for a group, for the `units` preference: the pairs in that order.
pub fn group_units(alias_group: &str) -> Option<Vec<(String, String)>> {
    let g = tables::groups().get(alias_group)?;
    Some(vec![
        ("canonical".into(), g.canonical.clone()),
        ("si".into(), g.si.clone()),
        ("conventional".into(), g.conventional.clone()),
    ])
}

pub fn display_name(alias_group: &str, lang: &str) -> Option<String> {
    let g = tables::groups().get(alias_group)?;
    Some(if lang == "es" {
        g.name_es.clone()
    } else {
        g.name_en.clone()
    })
}

fn pow10(n: u32) -> BigInt {
    BigInt::from(10u8).pow(n)
}

/// `_to_decimal`: the exact fraction rounded to 12 significant digits (half even), trailing zeros
/// dropped, never an exponent (Python formats it with "f"). Beyond rust_decimal's 28 decimal places
/// the value is rounded there; beyond its range there is no value.
pub fn fraction_to_decimal(x: &BigRational) -> Option<Decimal> {
    if x.is_zero() {
        return Some(Decimal::ZERO);
    }
    let negative = x.is_negative();
    let a = x.abs();
    let (n, d) = (a.numer().clone(), a.denom().clone());
    // adjusted exponent e: 10^e <= a < 10^(e+1)
    let mut e = n.to_string().len() as i64 - d.to_string().len() as i64;
    let at_least = |e: i64| {
        if e >= 0 {
            n >= &d * pow10(e as u32)
        } else {
            &n * pow10((-e) as u32) >= d
        }
    };
    if !at_least(e) {
        e -= 1;
    }
    let k = (11 - e).min(28);
    let (num, den) = if k >= 0 {
        (&n * pow10(k as u32), d.clone())
    } else {
        (n.clone(), &d * pow10((-k) as u32))
    };
    let mut q = &num / &den;
    let twice = (&num % &den) * 2u8;
    if twice > den || (twice == den && (&q % 2u8) == BigInt::from(1u8)) {
        q += 1u8;
    }
    let (mantissa, scale) = if k >= 0 {
        (q, k as u32)
    } else {
        (q * pow10((-k) as u32), 0)
    };
    let mantissa = if negative { -mantissa } else { mantissa };
    let m = i128::try_from(mantissa).ok()?;
    Decimal::try_from_i128_with_scale(m, scale).ok().map(|d| d.normalize())
}

/// None when the conversion is not possible (the row then gets `unit_unconvertible`).
pub fn convert(value: Decimal, from_ucum: &str, to_ucum: &str, alias_group: &str) -> Result<Option<Decimal>> {
    if from_ucum == to_ucum {
        return Ok(Some(value));
    }
    let m = tables::molar().get(alias_group);
    let out = units::convert_fraction(
        &tables::decimal_fraction(&value),
        from_ucum,
        to_ucum,
        alias_group,
        m.and_then(|m| m.g_per_mol.as_ref()),
        m.and_then(|m| m.valence),
    );
    Ok(out.and_then(|f| fraction_to_decimal(&f)))
}

pub fn parse_range(printed: &str, locale: &Locale, unit: Option<&str>) -> Result<Vec<RefRange>> {
    let ucum = non_empty(unit).and_then(to_ucum);
    Ok(text::parse_range(printed, locale, ucum.as_deref()))
}

pub fn pick_range<'r>(ranges: &'r [RefRange], sex: Option<&str>, age_years: Option<i32>) -> Option<&'r RefRange> {
    text::pick_range(ranges, sex, age_years)
}

/// HL7 ObservationInterpretation from the printed range only: L, N, H, A. None without a range.
pub fn interpret(
    value: Option<Decimal>,
    text_code: Option<&str>,
    r: Option<&RefRange>,
    comparator: Option<&str>,
) -> Option<&'static str> {
    text::interpret(value, text_code, r, comparator)
}

// ----------------------------------------------------------------- resolution ----

/// Name + unit + specimen to an alias group and a LOINC code, with every candidate scored.
///
/// A match on only part of the name (a parenthesis dropped) carries reason name_partial and is
/// never auto-accepted.
pub async fn resolve_analyte(
    conn: Option<&mut PgConnection>,
    printed_name: &str,
    q: &ResolveQuery<'_>,
) -> Result<Resolution> {
    let ucum = non_empty(q.unit).and_then(to_ucum);
    let query = resolve::Query {
        lang: q.lang,
        unit_ucum: ucum.as_deref(),
        specimen: q.specimen,
        method: q.method,
        household_id: q.household_id,
        text_result: q.text_result,
    };
    let (cands, refused, folded) = resolve::resolve(conn, printed_name, &query).await?;
    if refused {
        return Ok(Resolution::unmapped(resolve::unmapped_group(&folded, q.provider), true));
    }
    let Some(best) = cands.first() else {
        return Ok(Resolution::unmapped(
            resolve::unmapped_group(&folded, q.provider),
            false,
        ));
    };
    let mut reasons = Vec::new();
    if best.partial {
        reasons.push("name_partial".to_string());
    }
    if !best.unit_ok {
        reasons.push("unit_dimension_mismatch".to_string());
    }
    if best.method.as_deref() == Some("ckd-epi") {
        reasons.push("method_uncertain".to_string()); // eGFR with no formula year printed: never assume 2021
    }
    Ok(Resolution {
        alias_group: best.alias_group.clone(),
        loinc: best.loinc.clone(),
        property: best.property.clone(),
        method: best.method.clone(),
        refused: false,
        reasons,
        confidence: best.confidence,
        candidates: cands.clone(),
    })
}

// ---------------------------------------------------------------------- a row ----

fn ok(name: &str) -> Check {
    Check::pass(name)
}

fn fail(name: &str, reason: &str, detail: String) -> Check {
    Check {
        detail: Some(detail),
        ..Check::fail(name, reason)
    }
}

/// The derived layer of one row: what `rederive` recomputes.
#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    pub loinc: Option<String>,
    pub alias_group: String,
    pub method: Option<String>,
    pub unit_ucum: Option<String>,
    pub value_canonical: Option<Decimal>,
    pub unit_canonical: Option<String>,
    pub interpretation: Option<String>,
    pub specimen: Option<String>,
}

/// The truth and parsed layers `derive` reads.
pub struct DeriveIn<'a> {
    pub printed_name: &'a str,
    pub printed_unit: Option<&'a str>,
    pub value: Option<Decimal>,
    pub comparator: Option<&'a str>,
    pub text_code: Option<&'a str>,
    pub range: Option<&'a RefRange>,
    pub printed_flag: Option<&'a str>,
    pub lang: &'a str,
    pub specimen: Option<&'a str>,
    pub method: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub loinc: Option<&'a str>,
    pub household_id: Option<Uuid>,
}

fn py_opt_decimal(d: Option<Decimal>) -> String {
    d.map_or_else(|| "None".into(), |d| py_decimal_str(&d))
}

/// The derived layer of one row from its truth and parsed layers: what `rederive` recomputes.
pub async fn derive(conn: Option<&mut PgConnection>, x: &DeriveIn<'_>) -> Result<(Derived, Resolution, Vec<Check>)> {
    use text::py_repr;
    let mut checks: Vec<Check> = Vec::new();
    let text_result = x.value.is_none() && x.text_code.is_some();
    let mut res = resolve_analyte(
        conn,
        x.printed_name,
        &ResolveQuery {
            lang: x.lang,
            unit: x.printed_unit,
            specimen: x.specimen,
            method: x.method,
            provider: x.provider,
            household_id: x.household_id,
            text_result,
        },
    )
    .await?;
    let codes = tables::loinc_codes();
    let given = non_empty(x.loinc);
    let code = given.and_then(|l| codes.get(l));
    let printed_ucum = non_empty(x.printed_unit).and_then(to_ucum);
    if let (Some(l), None) = (given, code) {
        checks.push(fail(
            "loinc",
            "loinc_unknown",
            format!("LOINC {l} is not in the shipped cut"),
        ));
    } else if let Some(code) = code {
        // a structured import brought its own code: keep it, check it like any other
        let eff = resolve::effective_unit(&code.alias_group, printed_ucum.as_deref(), Some(code));
        let fits = resolve::unit_fits(&code.property, eff.as_deref());
        let quantitative = matches!(code.scale.as_str(), "Qn" | "SemiQn");
        res = Resolution {
            alias_group: code.alias_group.clone(),
            loinc: Some(code.code.clone()),
            property: Some(code.property.clone()),
            method: non_empty(Some(&code.method)).map(String::from),
            refused: false,
            reasons: if fits || !quantitative {
                Vec::new()
            } else {
                vec!["unit_dimension_mismatch".into()]
            },
            confidence: 1.0,
            candidates: std::mem::take(&mut res.candidates),
        };
    }
    let c = codes.get(res.loinc.as_deref().unwrap_or(""));
    if res.refused {
        checks.push(fail(
            "loinc",
            "unmapped",
            "a panel or category name, not an analyte".into(),
        ));
    } else if c.is_none() {
        if checks.is_empty() {
            // loinc_unknown already says it
            checks.push(fail("loinc", "unmapped", "no alias matches this name".into()));
        }
    } else if res.reasons.iter().any(|r| r == "name_partial") {
        checks.push(fail(
            "loinc",
            "unmapped",
            format!("only part of the printed name matched {}", res.alias_group),
        ));
    } else if let Some(c) = c.filter(|c| c.status == "DEPRECATED") {
        checks.push(fail(
            "loinc",
            "loinc_deprecated",
            format!("LOINC {} is deprecated", c.code),
        ));
    } else if checks.is_empty() {
        checks.push(ok("loinc"));
    }
    let ucum = resolve::effective_unit(&res.alias_group, printed_ucum.as_deref(), c);
    let quantitative = c.is_none_or(|c| matches!(c.scale.as_str(), "Qn" | "SemiQn"));
    if let Some(c) = c
        && quantitative
        && x.value.is_some()
    {
        if res.reasons.iter().any(|r| r == "unit_dimension_mismatch") || ucum.is_none() {
            let what = match non_empty(x.printed_unit) {
                Some(pu) => format!(
                    "unit {} ({})",
                    py_repr(pu),
                    units::dims_name(units::dims_of(ucum.as_deref()))
                ),
                None => "no unit".into(),
            };
            checks.push(fail(
                "unit_dimension",
                "unit_dimension_mismatch",
                format!("{what} does not fit LOINC {} property {}", c.code, c.property),
            ));
        } else {
            checks.push(ok("unit_dimension"));
        }
    }
    if let Some(c) = c
        && quantitative
        && text_result
    {
        checks.push(fail(
            "plausibility",
            "implausible",
            format!("a text result on quantitative LOINC {}", c.code),
        ));
    }
    let canonical_u = canonical_unit(&res.alias_group).filter(|u| !u.is_empty());
    let mut canonical_v = None;
    if let (Some(v), Some(from), Some(to)) = (x.value, ucum.as_deref().filter(|u| !u.is_empty()), &canonical_u) {
        canonical_v = convert(v, from, to, &res.alias_group)?;
    }
    if let (Some(_), Some(cu), true) = (x.value, &canonical_u, quantitative) {
        if canonical_v.is_none() {
            checks.push(fail(
                "converts",
                "unit_unconvertible",
                format!(
                    "{} does not convert to {cu} for {}",
                    non_empty(x.printed_unit).unwrap_or("no unit"),
                    res.alias_group
                ),
            ));
        } else {
            checks.push(ok("converts"));
        }
    }
    if let (Some(v), Some(g)) = (canonical_v, tables::groups().get(&res.alias_group))
        && (g.plaus_low.is_some() || g.plaus_high.is_some())
    {
        checks.extend(plausibility(v, g, canonical_u.as_deref().unwrap_or("")));
    }
    let mut interp = interpret(x.value, x.text_code, x.range, x.comparator);
    let flag = text::printed_flag_code(x.printed_flag);
    if let Some(f @ ("HH" | "LL")) = flag
        && interp == Some(&f[..1])
    {
        interp = Some(f); // critical only when the lab prints it
    }
    if text::flag_disagrees(flag, interp) {
        checks.push(fail(
            "flag",
            "flag_mismatch",
            format!(
                "printed flag {} but the printed range gives {}",
                x.printed_flag.map_or_else(|| "None".into(), py_repr),
                interp.unwrap_or("None")
            ),
        ));
    } else if flag.is_some() && interp.is_some() {
        checks.push(ok("flag"));
    }
    let derived = Derived {
        loinc: res.loinc.clone(),
        alias_group: res.alias_group.clone(),
        method: res.method.clone(),
        unit_ucum: ucum,
        value_canonical: canonical_v,
        unit_canonical: if canonical_v.is_some() {
            canonical_unit(&res.alias_group)
        } else {
            None
        },
        interpretation: interp.map(String::from),
        specimen: match c {
            Some(c) => Some(c.system.clone()),
            None => resolve::specimen_code(x.specimen),
        },
    };
    Ok((derived, res, checks))
}

fn plausibility(v: Decimal, g: &tables::Group, unit: &str) -> Vec<Check> {
    let inside = |x: Decimal| g.plaus_low.is_none_or(|lo| x >= lo) && g.plaus_high.is_none_or(|hi| x <= hi);
    if inside(v) {
        return vec![ok("plausibility")];
    }
    let (lo, hi) = (py_opt_decimal(g.plaus_low), py_opt_decimal(g.plaus_high));
    let mut out = vec![fail(
        "plausibility",
        "implausible",
        format!("{} {unit} is outside {lo}-{hi} {unit}", py_decimal_str(&v)),
    )];
    for factor in [Decimal::from(1000), Decimal::new(1, 3)] {
        if let Some(scaled) = v.checked_mul(factor)
            && inside(scaled)
        {
            out.push(fail(
                "factor_1000",
                "suspect_separator",
                format!(
                    "{} {unit} would be plausible: a thousands or decimal separator misread?",
                    py_decimal_str(&scaled)
                ),
            ));
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frac(n: i64, d: i64) -> BigRational {
        BigRational::new(BigInt::from(n), BigInt::from(d))
    }

    #[test]
    fn twelve_significant_digits_like_python() {
        let s = |x: BigRational| fraction_to_decimal(&x).map(|d| d.to_string());
        assert_eq!(s(frac(1, 60)), Some("0.0166666666667".into()));
        assert_eq!(s(frac(60, 1)), Some("60".into()));
        assert_eq!(s(frac(1000, 180156)).as_deref(), Some("0.00555074490997"));
        assert_eq!(s(frac(-5, 2)), Some("-2.5".into()));
        assert_eq!(s(frac(1_234_567_890_123_456, 1)), Some("1234567890120000".into()));
        // half even at the twelfth digit
        assert_eq!(s(frac(1_000_000_000_005, 10)), Some("100000000000".into()));
        assert_eq!(s(frac(1_000_000_000_015, 10)), Some("100000000002".into()));
    }
}
