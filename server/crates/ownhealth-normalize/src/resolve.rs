//! Analyte resolution from name + unit + specimen, never name alone (`normalize/_resolve.py`,
//! ingestion.md 8.2).
//!
//! 1. Fold the printed name; refuse panel and category names.
//! 2. Find alias groups for the name: the shipped tables, then the household's own aliases, with the
//!    name as printed, without specimen words, without a parenthesis, and the parenthesis alone.
//! 3. Move to the specimen twin (creatinine read in urine is urine_creatinine) and the unit twin
//!    (Neutrófilos in % is neutrophils_pct).
//! 4. Inside the group, the unit's dimension picks the LOINC property (LDL in mg/dL is MCnc, in
//!    mmol/L SCnc), the specimen picks the system, and the method picks between calculated and direct.

use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use ownhealth_core::Result;
use regex::Regex;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Candidate;
use crate::tables::{self as t, Alias, LoincCode, fold, strip_chars};
use crate::units::{self as u, Dims, ZERO};

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new(&$pat).expect("static regex"));
    };
}

/// LOINC property -> the unit dimensions it accepts (8.3). Ord / Nom scales carry no unit.
pub fn property_dims(prop: &str) -> Option<&'static [Dims]> {
    Some(match prop {
        "MCnc" | "EntMCnc" => &[u::D_MASS_CONC],
        "SCnc" => &[u::D_SUBST_CONC, u::D_EQ_CONC],
        "ACnc" => &[u::D_IU_CONC, u::D_ARB_CONC],
        "CCnc" => &[u::D_CAT_CONC],
        "NCnc" => &[u::D_NUM_CONC],
        "MFr" | "NFr" | "VFr" | "SFr" | "MRto" | "RelTime" | "SpGrav" | "DistWidth" => &[ZERO],
        "Ratio" => &[ZERO, u::D_MASS_PER_MOL],
        "EntMass" => &[u::D_MASS],
        "EntMeanVol" | "Vol" => &[u::D_VOL],
        "ArVRat" | "VRat" => &[u::D_VOL_RATE],
        "Osmol" => &[u::D_OSMOL],
        "PPres" => &[u::D_PRESSURE],
        "LsCnc" => &[u::D_PH],
        "Time" => &[u::D_TIME],
        "Sedimentation Rate" => &[u::D_SPEED],
        "Naric" => &[u::D_PER_HPF],
        "MRat" => &[u::D_MASS_RATE],
        _ => return None,
    })
}

pub const UNITLESS_CANONICAL: [&str; 4] = ["{INR}", "{ratio}", "{Spec_grav}", "[pH]"];

const SPECIMEN_HINTS: [(&str, &str); 3] = [
    (
        "Urine",
        r"\b(?:orina|urine|urin|urinaire|urinario|urinaria|harn|u-)\b|^u ",
    ),
    (
        "Bld",
        r"\b(?:sangre total|whole blood|vollblut|sang total|capilar|capillary|kapillar)\b",
    ),
    (
        "Ser/Plas",
        r"\b(?:suero|serum|plasma|serico|serica|serique|plasmatico|s-|p-)\b|^[sp] ",
    ),
];
static SPECIMEN_HINT_RES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    SPECIMEN_HINTS
        .iter()
        .map(|(code, p)| (*code, Regex::new(p).expect("static regex")))
        .collect()
});
re!(
    SPECIMEN_WORDS,
    r"\b(?:en|in|im|de|dans|del|la|le|les)?\s*\(?\b(?:orina|urine|urin|suero|serum|plasma|sangre|blood|blut|sang)\b\)?|^[spu]-\s*|^[spu] "
);

/// Free-text specimen hints from importers.
fn specimen_free(folded: &str) -> Option<&'static str> {
    Some(match folded {
        "urine" | "orina" | "urin" | "ur" => "Urine",
        "blood" | "bld" | "sangre" | "whole blood" | "blut" | "sang" => "Bld",
        "serum" | "suero" | "plasma" | "ser/plas" | "ser" | "plas" | "ppp" | "serum or plasma" => "Ser/Plas",
        _ => return None,
    })
}

fn systems(specimen: &str) -> Option<&'static [&'static str]> {
    Some(match specimen {
        "Urine" => &["Urine", "Urine sed", "Urine+Ser/Plas"],
        "Bld" => &["Bld", "BldA", "RBC", "Ser/Plas/Bld"],
        "Ser/Plas" => &["Ser/Plas", "Ser", "Plas", "PPP", "Ser/Plas/Bld", "Urine+Ser/Plas"],
        _ => return None,
    })
}

pub fn specimen_code(hint: Option<&str>) -> Option<String> {
    let hint = hint.filter(|h| !h.is_empty())?;
    if let Some(c) = specimen_free(&fold(hint)) {
        return Some(c.into());
    }
    systems(hint).map(|_| hint.to_string())
}

pub fn specimen_from_name(folded: &str) -> Option<&'static str> {
    SPECIMEN_HINT_RES
        .iter()
        .find(|(_, re)| re.is_match(folded))
        .map(|(code, _)| *code)
}

re!(SUBSTANCE_FRACTION, r"mol/(?:[a-z]?)mol$");

/// The unit's dimension matches the LOINC property. mmol/mol is a substance fraction (SFr) only.
pub fn unit_fits(prop: &str, ucum: Option<&str>) -> bool {
    let Some(allowed) = property_dims(prop) else {
        return true;
    };
    let Some(dims) = u::dims_of(ucum) else {
        return false;
    };
    if !allowed.contains(&dims) {
        return false;
    }
    if dims == ZERO && matches!(prop, "SFr" | "MFr" | "NFr" | "VFr") {
        return (prop == "SFr") == SUBSTANCE_FRACTION.is_match(ucum.unwrap_or(""));
    }
    true
}

pub fn system_fits(system: &str, specimen: Option<&str>) -> bool {
    match specimen {
        None => true,
        Some(s) => systems(s).is_none_or(|set| set.contains(&system)),
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `re.sub(r"(?<![\w\[])([kmu]?)U(?![\w\]])", r"\1" + atom, ucum)` without look-around.
fn replace_printed_u(ucum: &str, atom: &str) -> String {
    let chars: Vec<char> = ucum.chars().collect();
    let behind_ok = |i: usize| i == 0 || !(is_word(chars[i - 1]) || chars[i - 1] == '[');
    let ahead_ok = |i: usize| i >= chars.len() || !(is_word(chars[i]) || chars[i] == ']');
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if behind_ok(i) {
            if matches!(c, 'k' | 'm' | 'u') && chars.get(i + 1) == Some(&'U') && ahead_ok(i + 2) {
                out.push(c);
                out.push_str(atom);
                i += 2;
                continue;
            }
            if c == 'U' && ahead_ok(i + 1) {
                out.push_str(atom);
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// How the code reads a printed U or IU. Units never convert between [IU] and U (that stays in
/// `units`); this only decides what a label meant, from the code's property:
///   * ACnc with an [arb'U] example (CA 125): U/mL is [arb'U]/mL;
///   * ACnc with an [IU] example (TSH, FSH): mU/L is m[IU]/L, as the German "mU/l" means;
///   * CCnc (enzymes): UI/L is U/L, the enzyme unit that was named the international unit.
pub fn unit_reading(ucum: Option<&str>, code: Option<&LoincCode>) -> Option<String> {
    let ucum = ucum?;
    let Some(code) = code.filter(|_| !ucum.is_empty()) else {
        return Some(ucum.to_string());
    };
    if code.property == "ACnc" && code.example_ucum.contains("[arb'U]") {
        return Some(replace_printed_u(ucum, "[arb'U]"));
    }
    if code.property == "ACnc" && code.example_ucum.contains("[IU]") {
        return Some(replace_printed_u(ucum, "[IU]"));
    }
    if code.property == "CCnc" {
        return Some(ucum.replace("[IU]", "U"));
    }
    Some(ucum.to_string())
}

const METHOD_WORDS: [(&str, &str); 2] = [
    (
        "calculated",
        r"calculad[oa]|calculated|calc\.?|berechnet|calcule|friedewald|nih|martin",
    ),
    ("direct", r"direct[oa]?|direkt|directe|homogeneo|homogeneous"),
];
/// A key at or above this is the printed name up to specimen and method words.
pub const EXACT: f64 = 0.95;

static METHOD_WORD_RES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    METHOD_WORDS
        .iter()
        .map(|(m, p)| (*m, Regex::new(&format!(r"\b(?:{p})(?:\b|$)")).expect("static regex")))
        .collect()
});
re!(
    METHOD_RE,
    format!(
        r"\(?\b(?:{})(?:\b|$)\)?",
        METHOD_WORDS.iter().map(|(_, p)| *p).collect::<Vec<_>>().join("|")
    )
);
re!(SPACES, r"\s+");
re!(PARENS, r"[()]");
re!(PAREN_GROUP, r"\([^)]*\)");
re!(PAREN_INNER, r"\(([^)]*)\)");
re!(NOT_SLUG, r"[^a-z0-9]+");

pub fn method_from_name(folded: &str) -> Option<&'static str> {
    METHOD_WORD_RES
        .iter()
        .find(|(_, re)| re.is_match(folded))
        .map(|(m, _)| *m)
}

/// `_squash` strips with Python's `str.strip(" ,-()")`, which keeps other whitespace.
fn squash_exact(s: &str) -> String {
    let s = SPACES.replace_all(s, " ");
    strip_chars(&s, " ,-()").to_string()
}

/// The methods a group's LOINC codes tell apart (LDL: calculated, direct); empty for most groups.
pub fn group_methods(group: &str) -> HashSet<&'static str> {
    t::codes_by_group()
        .get(group)
        .map(|codes| {
            codes
                .iter()
                .filter(|c| !c.method.is_empty())
                .map(|c| c.method.as_str())
                .collect()
        })
        .unwrap_or_default()
}

/// (key, confidence, stripped method). The name as printed; then without specimen or method words
/// and with parentheses unwrapped (still exact); then, as partial matches that never auto-accept,
/// without a parenthesis or the parenthesis alone. A key with a method word stripped names that
/// method: it is exact only for a group whose codes have it (LDL direct), never for "Bilirubin,
/// direkt", where the word names another analyte.
pub fn name_keys(folded: &str) -> Vec<(String, f64, Option<&'static str>)> {
    let mut keys: Vec<(String, f64, Option<&'static str>)> = vec![(folded.to_string(), 1.0, None)];
    let unwrapped = squash_exact(&PARENS.replace_all(folded, " "));
    keys.push((unwrapped.clone(), 0.98, None));
    let no_spec = squash_exact(&SPECIMEN_WORDS.replace_all(folded, " "));
    keys.push((no_spec.clone(), EXACT, None));
    for base in [&unwrapped, &no_spec] {
        let stripped = squash_exact(&METHOD_RE.replace_all(base, " "));
        let method = if &stripped != base {
            method_from_name(base)
        } else {
            None
        };
        keys.push((stripped, EXACT, method));
    }
    for (base, conf) in [(folded, 0.9), (no_spec.as_str(), 0.88)] {
        // partial: below EXACT
        let outside = PAREN_GROUP.replace_all(base, " ");
        let outside = SPACES.replace_all(&outside, " ");
        keys.push((strip_chars(&outside, " ,-").to_string(), conf, None));
        for inner in PAREN_INNER.captures_iter(base) {
            keys.push((inner[1].trim().to_string(), conf - 0.05, None));
        }
    }
    let mut seen = HashSet::new();
    keys.into_iter()
        .filter(|(k, _, _)| !k.is_empty() && seen.insert(k.clone()))
        .collect()
}

pub async fn user_aliases(
    conn: Option<&mut PgConnection>,
    household_id: Option<Uuid>,
    key: &str,
) -> Result<Vec<Alias>> {
    let (Some(conn), Some(h)) = (conn, household_id) else {
        return Ok(Vec::new());
    };
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "select alias, alias_group, coalesce(lang, '') as lang, coalesce(loinc, '') as loinc from analyte_alias \
         where alias_norm = $1 and (household_id = $2 or household_id is null) order by created_at",
    )
    .bind(key)
    .bind(h)
    .fetch_all(conn)
    .await?;
    let codes = t::loinc_codes();
    Ok(rows
        .into_iter()
        .map(|(alias, alias_group, lang, loinc)| Alias {
            method: codes.get(&loinc).map(|c| c.method.clone()).unwrap_or_default(),
            alias,
            alias_group,
            lang,
            source: "user".into(),
            loinc,
        })
        .collect())
}

pub fn twin(group: &str, specimen: Option<&str>, dims: Option<Dims>, text_result: bool) -> String {
    let twins = t::specimen_twins();
    let mut group = group.to_string();
    if let Some(s) = specimen.filter(|s| !s.is_empty())
        && let Some(tw) = twins.get(&(group.clone(), s.to_string()))
    {
        group = tw.clone();
    }
    if text_result && let Some(tw) = twins.get(&(group.clone(), "text".to_string())) {
        group = tw.clone();
    }
    if let Some(dims) = dims {
        let twin = if let Some(stem) = group.strip_suffix("_abs")
            && dims == ZERO
        {
            format!("{stem}_pct")
        } else if let Some(stem) = group.strip_suffix("_pct")
            && dims == u::D_NUM_CONC
        {
            format!("{stem}_abs")
        } else {
            group.clone()
        };
        if t::groups().contains_key(&twin) {
            group = twin;
        }
    }
    group
}

/// The unit as the code reads it: unitless groups need none, CA 125 reads U as [arb'U].
pub fn effective_unit(group: &str, unit_ucum: Option<&str>, code: Option<&LoincCode>) -> Option<String> {
    match unit_ucum {
        None => t::groups()
            .get(group)
            .filter(|g| UNITLESS_CANONICAL.contains(&g.canonical.as_str()))
            .map(|g| g.canonical.clone()),
        Some(_) => unit_reading(unit_ucum, code),
    }
}

/// The best LOINC code of a group for this unit, specimen and method -> (code, unit_ok, specimen_ok).
pub fn pick_code(
    group: &str,
    unit_ucum: Option<&str>,
    specimen: Option<&str>,
    method: Option<&str>,
    text_result: bool,
) -> (Option<&'static LoincCode>, bool, bool) {
    let Some(codes) = t::codes_by_group().get(group).filter(|c| !c.is_empty()) else {
        return (None, false, false);
    };
    let score = |c: &LoincCode| {
        let ok_unit = if text_result {
            // Negativo, trazas: the presence code, never the quantitative one
            matches!(c.scale.as_str(), "Ord" | "Nom")
        } else if matches!(c.scale.as_str(), "Qn" | "SemiQn") {
            unit_fits(&c.property, effective_unit(group, unit_ucum, Some(c)).as_deref())
        } else {
            unit_ucum.is_none()
        };
        (
            ok_unit,
            system_fits(&c.system, specimen),
            matches!(c.status.as_str(), "ACTIVE" | "TRIAL"),
            match method.filter(|m| !m.is_empty()) {
                Some(m) => c.method == m,
                None => c.method.is_empty(),
            },
            c.method.is_empty(),
        )
    };
    // max keeps the first of equals: file order breaks ties
    let mut best = &codes[0];
    let mut best_score = score(best);
    for c in &codes[1..] {
        let s = score(c);
        if s > best_score {
            best = c;
            best_score = s;
        }
    }
    (Some(best), best_score.0, best_score.1)
}

/// Python's `round(x, 3)` for a float: the correctly rounded decimal, ties to even.
pub fn round3(x: f64) -> f64 {
    format!("{x:.3}").parse().unwrap_or(x)
}

pub struct Query<'a> {
    pub lang: &'a str,
    pub unit_ucum: Option<&'a str>,
    pub specimen: Option<&'a str>,
    pub method: Option<&'a str>,
    pub household_id: Option<Uuid>,
    pub text_result: bool,
}

/// Every candidate, best first; (candidates, refused, folded_name).
pub async fn resolve(
    mut conn: Option<&mut PgConnection>,
    printed_name: &str,
    q: &Query<'_>,
) -> Result<(Vec<Candidate>, bool, String)> {
    let folded = fold(printed_name);
    if t::panels().contains(&folded) {
        return Ok((Vec::new(), true, folded));
    }
    let spec = specimen_code(q.specimen).or_else(|| specimen_from_name(&folded).map(String::from));
    let spec = spec.as_deref();
    let dims = u::dims_of(q.unit_ucum);
    let named_method = method_from_name(&folded);
    let mut found: IndexMap<String, Candidate> = IndexMap::new();
    for (key, name_conf, stripped) in name_keys(&folded) {
        let mut rows = user_aliases(conn.as_deref_mut(), q.household_id, &key).await?;
        rows.extend(t::aliases().get(&key).cloned().unwrap_or_default());
        for a in &rows {
            let group = twin(&a.alias_group, spec, dims, q.text_result);
            let valid = group_methods(&group);
            let m = [q.method, Some(a.method.as_str()), named_method]
                .into_iter()
                .flatten()
                .find(|x| !x.is_empty() && valid.contains(x));
            let (code, unit_ok, spec_ok) = pick_code(&group, q.unit_ucum, spec, m, q.text_result);
            let mut conf = name_conf
                * (if a.lang == q.lang || a.lang.is_empty() {
                    1.0
                } else {
                    0.9
                })
                * (if a.source == "user" { 1.0 } else { 0.99 });
            conf *= if unit_ok { 1.0 } else { 0.5 };
            conf *= if spec_ok { 1.0 } else { 0.7 };
            let method = m
                .map(String::from)
                .or_else(|| code.map(|c| c.method.clone()))
                .filter(|s| !s.is_empty());
            let cand = Candidate {
                alias_group: group.clone(),
                loinc: code.map(|c| c.code.clone()),
                property: code.map(|c| c.property.clone()),
                method,
                confidence: round3(conf),
                matched: a.alias.clone(),
                unit_ok,
                specimen_ok: spec_ok,
                partial: name_conf < EXACT || stripped.is_some_and(|s| !valid.contains(s)),
            };
            match found.get(&group) {
                Some(old) if old.confidence >= cand.confidence => {}
                _ => {
                    found.insert(group, cand);
                }
            }
        }
        if !found.is_empty() && name_conf >= EXACT && found.values().any(|c| c.unit_ok && c.specimen_ok && !c.partial) {
            break;
        }
    }
    let groups: Vec<String> = found.keys().cloned().collect();
    for group in groups {
        let Some(cand) = found.get(&group).filter(|c| c.partial).cloned() else {
            continue;
        };
        if glossed(
            conn.as_deref_mut(),
            q.household_id,
            &folded,
            &group,
            spec,
            dims,
            q.text_result,
        )
        .await?
        {
            found.insert(
                group,
                Candidate {
                    partial: false,
                    confidence: round3(cand.confidence / 0.9 * EXACT),
                    ..cand
                },
            );
        }
    }
    let mut cands: Vec<Candidate> = found.into_values().collect();
    // stable, like Python's sorted(key=-confidence)
    cands.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok((cands, false, folded))
}

/// "GPT (ALT)": the name and its parenthesis are each a name of the same group, so the whole name
/// is that group, not a partial match.
async fn glossed(
    mut conn: Option<&mut PgConnection>,
    household_id: Option<Uuid>,
    folded: &str,
    group: &str,
    spec: Option<&str>,
    dims: Option<Dims>,
    text_result: bool,
) -> Result<bool> {
    let outside = squash_exact(&PAREN_GROUP.replace_all(folded, " "));
    let mut parts: Vec<String> = vec![outside];
    parts.extend(PAREN_INNER.captures_iter(folded).map(|c| c[1].to_string()));
    let parts: Vec<String> = parts
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return Ok(false);
    }
    for part in &parts {
        let mut rows = user_aliases(conn.as_deref_mut(), household_id, part).await?;
        rows.extend(t::aliases().get(part).cloned().unwrap_or_default());
        if !rows
            .iter()
            .any(|a| twin(&a.alias_group, spec, dims, text_result) == group)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn unmapped_group(folded: &str, provider: Option<&str>) -> String {
    let mut slug = strip_chars(&NOT_SLUG.replace_all(folded, "_"), "_").to_string();
    if slug.is_empty() {
        slug = "unnamed".into();
    }
    if let Some(p) = provider.filter(|p| !p.is_empty()) {
        let folded_p = fold(p);
        let ps = NOT_SLUG.replace_all(&folded_p, "_");
        let ps = strip_chars(&ps, "_");
        slug.push('@');
        slug.push_str(if ps.is_empty() { "provider" } else { ps });
    }
    slug.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printed_u_is_replaced_like_the_python_regex() {
        assert_eq!(replace_printed_u("mU/L", "[IU]"), "m[IU]/L");
        assert_eq!(replace_printed_u("U/mL", "[arb'U]"), "[arb'U]/mL");
        assert_eq!(replace_printed_u("m[IU]/L", "[IU]"), "m[IU]/L");
        assert_eq!(replace_printed_u("kU/L", "[IU]"), "k[IU]/L");
        assert_eq!(replace_printed_u("xmU/L", "[IU]"), "xmU/L");
    }

    #[test]
    fn round3_is_pythons_round() {
        assert_eq!(round3(0.8500000000000001), 0.85);
        assert_eq!(round3(0.9 * 0.99), 0.891);
        assert_eq!(round3(0.0005), 0.001);
        assert_eq!(round3(0.0625), 0.062); // an exact tie goes to even, as in Python
        assert_eq!(round3(0.1875), 0.188);
    }
}
