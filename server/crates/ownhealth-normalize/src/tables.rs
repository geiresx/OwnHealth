//! The shipped terminology tables (`normalize/_tables.py`), read once from the files embedded at
//! build time. The files live unchanged in `data/terminology/`:
//!
//! ```text
//! loinc/loinc_subset.tsv      LOINC 2.83 subset (loinc/build_subset.py from loinc/codes.tsv), LOINC License
//! groups.tsv                  one row per alias_group: canonical, SI and conventional units, limits, names
//! aliases/{es,en,de,fr}.tsv   names as labs print them -> alias_group (+ method)
//! aliases/panels.tsv          panel and category names, refused
//! aliases/specimen_twins.tsv  the same analyte in another specimen (creatinine in urine ...)
//! molar/molar_mass.tsv        molar mass and valence per alias_group, cited
//! ```

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::LazyLock;

use indexmap::IndexMap;
use num_bigint::BigInt;
use num_rational::BigRational;
use regex::Regex;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

pub const LANGS: [&str; 4] = ["es", "en", "de", "fr"];

macro_rules! table {
    ($path:literal) => {
        ($path, include_str!(concat!("../../../../data/terminology/", $path)))
    };
}

/// Every `*.tsv` under `terminology/`, in the order Python's `sorted(DATA.rglob("*.tsv"))` gives
/// (path components compared one by one). `tests/tables.rs` checks the list against the directory.
pub const FILES: [(&str, &str); 10] = [
    table!("aliases/de.tsv"),
    table!("aliases/en.tsv"),
    table!("aliases/es.tsv"),
    table!("aliases/fr.tsv"),
    table!("aliases/panels.tsv"),
    table!("aliases/specimen_twins.tsv"),
    table!("groups.tsv"),
    table!("loinc/codes.tsv"),
    table!("loinc/loinc_subset.tsv"),
    table!("molar/molar_mass.tsv"),
];

fn file(path: &str) -> &'static str {
    FILES.iter().find(|(p, _)| *p == path).map_or("", |(_, c)| c)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoincCode {
    pub code: String,
    pub alias_group: String,
    /// "" = method-agnostic
    pub method: String,
    pub long_name: String,
    pub component: String,
    pub property: String,
    pub time: String,
    pub system: String,
    pub scale: String,
    pub example_ucum: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub alias_group: String,
    pub canonical: String,
    pub si: String,
    pub conventional: String,
    pub plaus_low: Option<Decimal>,
    pub plaus_high: Option<Decimal>,
    pub name_en: String,
    pub name_es: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    pub alias: String,
    pub alias_group: String,
    pub method: String,
    pub lang: String,
    pub source: String,
    /// A household alias may name its code; its method then steers the pick.
    pub loinc: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Molar {
    pub alias_group: String,
    pub species: String,
    pub g_per_mol: Option<BigRational>,
    /// The molar mass as the table prints it ("386.654").
    pub g_per_mol_text: String,
    pub valence: Option<i64>,
    pub citation: String,
}

/// Python's `str.splitlines()` separators.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if is_line_break(c) {
            out.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r'
                && let Some((j, '\n')) = chars.peek().copied()
            {
                chars.next();
                end = j + 1;
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `_rows`: header plus rows, every cell stripped, short rows padded with "".
pub fn rows(text: &str) -> Vec<HashMap<String, String>> {
    let lines: Vec<&str> = split_lines(text)
        .into_iter()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let Some((head, body)) = lines.split_first() else {
        return Vec::new();
    };
    let header: Vec<&str> = head.split('\t').collect();
    body.iter()
        .map(|ln| {
            let mut cells: Vec<String> = ln.split('\t').map(|c| c.trim().to_string()).collect();
            while cells.len() < header.len() {
                cells.push(String::new());
            }
            header.iter().map(|h| h.to_string()).zip(cells).collect()
        })
        .collect()
}

fn cell<'a>(r: &'a HashMap<String, String>, key: &str) -> &'a str {
    r.get(key).map_or("", String::as_str)
}

static SPACE_AROUND_PUNCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*([/(),:])\s*").expect("static regex"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s_]+").expect("static regex"));

/// Python's `str.strip(chars)`.
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}

/// Matching key for names: lower case, no accents, µ as u, dashes and spaces folded.
pub fn fold(text: &str) -> String {
    let s = text.replace(['µ', 'μ'], "u").replace('ß', "ss");
    let s: String = s.nfkd().filter(|c| canonical_combining_class(*c) == 0).collect();
    let s = s.to_lowercase();
    let s = s.replace('œ', "oe").replace(['–', '−'], "-").replace('\u{a0}', " ");
    let s = SPACE_AROUND_PUNCT.replace_all(&s, "$1");
    let s = SPACES.replace_all(&s, " ");
    strip_chars(&s, " .:*").replace("( ", "(").replace(" )", ")")
}

/// A decimal string as an exact fraction (`Fraction("180.156")`).
pub fn fraction(s: &str) -> Option<BigRational> {
    let d = Decimal::from_str(s).ok()?;
    Some(decimal_fraction(&d))
}

pub fn decimal_fraction(d: &Decimal) -> BigRational {
    let den = BigInt::from(10u8).pow(d.scale());
    BigRational::new(BigInt::from(d.mantissa()), den)
}

pub struct Tables {
    pub loinc_codes: IndexMap<String, LoincCode>,
    pub codes_by_group: IndexMap<String, Vec<LoincCode>>,
    pub groups: IndexMap<String, Group>,
    /// Folded alias -> every shipped alias row with that key, in file order (es, en, de, fr).
    pub aliases: IndexMap<String, Vec<Alias>>,
    pub panels: HashSet<String>,
    pub specimen_twins: HashMap<(String, String), String>,
    pub molar: IndexMap<String, Molar>,
    /// Short hash over every shipped table: part of the derivation version.
    pub hash: String,
}

fn opt_decimal(s: &str) -> Option<Decimal> {
    if s.is_empty() {
        return None;
    }
    // The tables are fixed at build time and tests/tables.rs reads every row.
    Some(Decimal::from_str(s).expect("a decimal in the shipped tables"))
}

fn load() -> Tables {
    let mut loinc_codes = IndexMap::new();
    for r in rows(file("loinc/loinc_subset.tsv")) {
        let c = LoincCode {
            code: cell(&r, "loinc").into(),
            alias_group: cell(&r, "alias_group").into(),
            method: cell(&r, "method").into(),
            long_name: cell(&r, "long_name").into(),
            component: cell(&r, "component").into(),
            property: cell(&r, "property").into(),
            time: cell(&r, "time").into(),
            system: cell(&r, "system").into(),
            scale: cell(&r, "scale").into(),
            example_ucum: cell(&r, "example_ucum").into(),
            status: cell(&r, "status").into(),
        };
        loinc_codes.insert(c.code.clone(), c);
    }
    let mut codes_by_group: IndexMap<String, Vec<LoincCode>> = IndexMap::new();
    for c in loinc_codes.values() {
        codes_by_group.entry(c.alias_group.clone()).or_default().push(c.clone());
    }
    let mut groups = IndexMap::new();
    for r in rows(file("groups.tsv")) {
        let g = Group {
            alias_group: cell(&r, "alias_group").into(),
            canonical: cell(&r, "canonical").into(),
            si: cell(&r, "si").into(),
            conventional: cell(&r, "conventional").into(),
            plaus_low: opt_decimal(cell(&r, "plaus_low")),
            plaus_high: opt_decimal(cell(&r, "plaus_high")),
            name_en: cell(&r, "name_en").into(),
            name_es: cell(&r, "name_es").into(),
        };
        groups.insert(g.alias_group.clone(), g);
    }
    let mut aliases: IndexMap<String, Vec<Alias>> = IndexMap::new();
    for lang in LANGS {
        for r in rows(file(&format!("aliases/{lang}.tsv"))) {
            let source = match cell(&r, "source") {
                "" => "curated",
                s => s,
            };
            aliases.entry(fold(cell(&r, "alias"))).or_default().push(Alias {
                alias: cell(&r, "alias").into(),
                alias_group: cell(&r, "alias_group").into(),
                method: cell(&r, "method").into(),
                lang: lang.into(),
                source: source.into(),
                loinc: String::new(),
            });
        }
    }
    let panels = rows(file("aliases/panels.tsv"))
        .iter()
        .map(|r| fold(cell(r, "name")))
        .collect();
    let specimen_twins = rows(file("aliases/specimen_twins.tsv"))
        .iter()
        .map(|r| {
            (
                (cell(r, "alias_group").to_string(), cell(r, "specimen").to_string()),
                cell(r, "twin").to_string(),
            )
        })
        .collect();
    let mut molar = IndexMap::new();
    for r in rows(file("molar/molar_mass.tsv")) {
        let g = cell(&r, "g_per_mol");
        let v = cell(&r, "valence");
        let m = Molar {
            alias_group: cell(&r, "alias_group").into(),
            species: cell(&r, "species").into(),
            g_per_mol: (!g.is_empty()).then(|| fraction(g).expect("a molar mass in the shipped table")),
            g_per_mol_text: g.to_string(),
            valence: (!v.is_empty()).then(|| v.parse().expect("a valence in the shipped table")),
            citation: cell(&r, "citation").into(),
        };
        molar.insert(m.alias_group.clone(), m);
    }
    Tables {
        loinc_codes,
        codes_by_group,
        groups,
        aliases,
        panels,
        specimen_twins,
        molar,
        hash: hash_files(FILES.iter().map(|(p, c)| (*p, c.as_bytes()))),
    }
}

/// `tables_hash`: sha256 over (relative path, bytes) of every table in path order, first 8 hex chars.
pub fn hash_files<'a>(files: impl Iterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut h = Sha256::new();
    for (path, bytes) in files {
        h.update(path.as_bytes());
        h.update(bytes);
    }
    hex::encode(h.finalize())[..8].to_string()
}

static TABLES: LazyLock<Tables> = LazyLock::new(load);

pub fn tables() -> &'static Tables {
    &TABLES
}

pub fn loinc_codes() -> &'static IndexMap<String, LoincCode> {
    &TABLES.loinc_codes
}

pub fn codes_by_group() -> &'static IndexMap<String, Vec<LoincCode>> {
    &TABLES.codes_by_group
}

pub fn groups() -> &'static IndexMap<String, Group> {
    &TABLES.groups
}

pub fn aliases() -> &'static IndexMap<String, Vec<Alias>> {
    &TABLES.aliases
}

pub fn panels() -> &'static HashSet<String> {
    &TABLES.panels
}

pub fn specimen_twins() -> &'static HashMap<(String, String), String> {
    &TABLES.specimen_twins
}

pub fn molar() -> &'static IndexMap<String, Molar> {
    &TABLES.molar
}

pub fn tables_hash() -> &'static str {
    &TABLES.hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_matches_python() {
        assert_eq!(fold("Hemoglobina glicada (HbA1c)"), "hemoglobina glicada(hba1c)");
        assert_eq!(fold("  Cholestérol   LDL : "), "cholesterol ldl");
        assert_eq!(fold("Größe µg"), "grosse ug");
        assert_eq!(fold("Œstradiol\u{a0}–"), "oestradiol -");
        assert_eq!(fold("ALT ( GPT )"), "alt(gpt)");
    }

    #[test]
    fn splitlines_like_python() {
        assert_eq!(split_lines("a\r\nb\rc\n\nd"), vec!["a", "b", "c", "", "d"]);
        assert_eq!(split_lines("a\n"), vec!["a"]);
    }
}
