//! Rule-based document classifier (`ingest/classify.py`, ingestion.md 5). No model.
//!
//! From the text lines of a document's first pages it guesses the document type, the language, the
//! patient block (name, birth date, sample id), the collection and report dates, and the report
//! status. Every guess is shown to the person, who confirms or corrects it; nothing here is a fact.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use regex::Regex;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use crate::py::{az, count_bounded, has_bounded, strip_chars};

pub const DOC_TYPES: [&str; 6] = [
    "lab_report",
    "imaging_report",
    "discharge",
    "prescription",
    "genomics",
    "other",
];

const TYPE_WORDS: [(&str, &[&str]); 5] = [
    (
        "lab_report",
        &[
            "hemograma",
            "bioquimica",
            "laboratorio",
            "valores de referencia",
            "intervalo de referencia",
            "laboratory",
            "lab results",
            "reference range",
            "reference interval",
            "haematology",
            "hematology",
            "laborbefund",
            "referenzbereich",
            "normbereich",
            "blutbild",
            "klinische chemie",
            "laboratoire",
            "valeurs de reference",
            "valeurs usuelles",
            "biochimie",
            "hematologie",
            "analyses",
        ],
    ),
    (
        "imaging_report",
        &[
            "radiologia",
            "ecografia",
            "resonancia",
            "tomografia",
            "radiografia",
            "informe radiologico",
            "radiology",
            "ultrasound",
            "mri",
            "ct scan",
            "x-ray",
            "rontgen",
            "sonographie",
            "mrt",
            "radiologie",
            "echographie",
            "irm",
            "scanner",
            "impression:",
            "hallazgos",
            "findings",
        ],
    ),
    (
        "discharge",
        &[
            "informe de alta",
            "alta hospitalaria",
            "discharge summary",
            "discharge letter",
            "entlassungsbericht",
            "entlassbrief",
            "compte rendu d'hospitalisation",
            "lettre de sortie",
            "fecha de ingreso",
            "admission",
        ],
    ),
    (
        "prescription",
        &[
            "receta",
            "prescripcion",
            "posologia",
            "prescription",
            "rx",
            "dosage",
            "rezept",
            "verordnung",
            "ordonnance",
            "posologie",
        ],
    ),
    (
        "genomics",
        &[
            "genotipo",
            "genomica",
            "farmacogenetica",
            "genotype",
            "pharmacogenomic",
            "pharmacogenetic",
            "rsid",
            "vcf",
            "genetik",
            "genotyp",
            "genetique",
            "sequencage",
            "haplotype",
            "diplotype",
        ],
    ),
];

const LANG_WORDS: [(&str, &[&str]); 4] = [
    (
        "es",
        &[
            "de",
            "la",
            "el",
            "los",
            "las",
            "del",
            "y",
            "resultado",
            "paciente",
            "fecha",
            "unidades",
            "prueba",
            "valores",
            "referencia",
            "informe",
            "muestra",
        ],
    ),
    (
        "en",
        &[
            "the",
            "and",
            "of",
            "result",
            "results",
            "patient",
            "date",
            "units",
            "test",
            "reference",
            "range",
            "report",
            "sample",
            "collected",
        ],
    ),
    (
        "de",
        &[
            "der",
            "die",
            "das",
            "und",
            "ergebnis",
            "einheit",
            "datum",
            "untersuchung",
            "referenzbereich",
            "befund",
            "seite",
            "geb",
        ],
    ),
    (
        "fr",
        &[
            "le",
            "les",
            "des",
            "et",
            "du",
            "resultat",
            "unite",
            "valeurs",
            "analyse",
            "patient",
            "date",
            "prelevement",
            "naissance",
            "page",
        ],
    ),
];

const LABELS: [(&str, &[&str]); 5] = [
    (
        "name",
        &[
            "paciente",
            "nombre",
            "patient(in)",
            "patientin",
            "patient(e)",
            "patient",
            "name",
            "nom",
        ],
    ),
    (
        "birth",
        &[
            "f. nacimiento",
            "fecha de nacimiento",
            "f. nac.",
            "fecha nac.",
            "date of birth",
            "dob",
            "born",
            "geb.-datum",
            "geburtsdatum",
            "geb.",
            "ne(e) le",
            "nee le",
            "ne le",
            "date de naissance",
        ],
    ),
    (
        "sample",
        &[
            "n.o muestra",
            "no muestra",
            "n.o de muestra",
            "muestra",
            "sample id",
            "specimen id",
            "sample",
            "accession",
            "probennummer",
            "probe-nr.",
            "auftragsnr.",
            "auftrag",
            "no echantillon",
            "echantillon",
            "dossier",
            "informe",
            "report id",
            "report",
            "befund-nr.",
        ],
    ),
    (
        "collected",
        &[
            "fecha de toma",
            "fecha de extraccion",
            "fecha extraccion",
            "f. extraccion",
            "fecha toma",
            "collected",
            "collection date",
            "date collected",
            "entnahmedatum",
            "entnahme",
            "abnahme",
            "date de prelevement",
            "preleve le",
            "prelevement",
        ],
    ),
    (
        "reported",
        &[
            "fecha de informe",
            "fecha de emision",
            "fecha validacion",
            "fecha de validacion",
            "reported",
            "report date",
            "befunddatum",
            "ausgestellt",
            "edite le",
            "date d'edition",
            "date du compte rendu",
        ],
    ),
];

const STATUS: [(&str, &[&str]); 2] = [
    (
        "amended",
        &[
            "rectificado",
            "informe corregido",
            "corregido",
            "amended",
            "corrected report",
            "korrigiert",
            "berichtigt",
            "rectifie",
            "compte rendu modifie",
        ],
    ),
    (
        "preliminary",
        &[
            "provisional",
            "preliminar",
            "preliminary",
            "vorlaufig",
            "provisoire",
            "partiel",
        ],
    ),
];

const MONTHS: &[(&str, u32)] = &[
    ("jan", 1),
    ("january", 1),
    ("enero", 1),
    ("ene", 1),
    ("januar", 1),
    ("janvier", 1),
    ("janv", 1),
    ("feb", 2),
    ("february", 2),
    ("febrero", 2),
    ("februar", 2),
    ("fevrier", 2),
    ("fevr", 2),
    ("mar", 3),
    ("march", 3),
    ("marzo", 3),
    ("marz", 3),
    ("mars", 3),
    ("apr", 4),
    ("april", 4),
    ("abril", 4),
    ("abr", 4),
    ("avril", 4),
    ("avr", 4),
    ("may", 5),
    ("mayo", 5),
    ("mai", 5),
    ("jun", 6),
    ("june", 6),
    ("junio", 6),
    ("juni", 6),
    ("juin", 6),
    ("jul", 7),
    ("july", 7),
    ("julio", 7),
    ("juli", 7),
    ("juillet", 7),
    ("juil", 7),
    ("aug", 8),
    ("august", 8),
    ("agosto", 8),
    ("ago", 8),
    ("aout", 8),
    ("sep", 9),
    ("sept", 9),
    ("september", 9),
    ("septiembre", 9),
    ("septembre", 9),
    ("oct", 10),
    ("october", 10),
    ("octubre", 10),
    ("oktober", 10),
    ("octobre", 10),
    ("okt", 10),
    ("nov", 11),
    ("november", 11),
    ("noviembre", 11),
    ("novembre", 11),
    ("dec", 12),
    ("december", 12),
    ("diciembre", 12),
    ("dic", 12),
    ("dezember", 12),
    ("decembre", 12),
    ("dez", 12),
];

fn month(name: &str) -> Option<u32> {
    MONTHS.iter().find(|(m, _)| *m == name).map(|(_, n)| *n)
}

fn rx(p: &str) -> Regex {
    Regex::new(p).unwrap_or_else(|e| panic!("bad built-in pattern {p}: {e}"))
}

static DATE_NUM: LazyLock<Regex> =
    LazyLock::new(|| rx(r"\b(\d{1,2})[./-](\d{1,2})[./-](\d{4})\b|\b(\d{4})-(\d{2})-(\d{2})\b"));
static DATE_WORD: LazyLock<Regex> =
    LazyLock::new(|| rx(r"\b(\d{1,2})\.?\s+([a-z]{3,10})\.?\s+(\d{4})\b|\b([a-z]{3,10})\.?\s+(\d{1,2}),\s*(\d{4})\b"));
static TIME: LazyLock<Regex> = LazyLock::new(|| rx(r"\b([01]?\d|2[0-3])[:h]([0-5]\d)\b"));
static AZ_RUN: LazyLock<Regex> = LazyLock::new(|| rx(r"[a-z]+"));
static TWO_LETTERS: LazyLock<Regex> = LazyLock::new(|| rx(r"[A-Za-zÀ-ÿ]{2}"));
static DIGIT: LazyLock<Regex> = LazyLock::new(|| rx(r"\d"));

/// Lowercase, strip accents, normalise the ordinal sign: for matching only.
pub fn fold(s: &str) -> String {
    let s = s.replace(['º', '°'], "o").replace('ª', "a");
    let stripped: String = s.nfkd().filter(|c| canonical_combining_class(*c) == 0).collect();
    stripped.to_lowercase()
}

fn num(m: &regex::Captures<'_>, i: usize) -> Option<i64> {
    m.get(i).and_then(|g| g.as_str().parse().ok())
}

fn ymd(y: Option<i64>, m: Option<i64>, d: Option<i64>) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(
        i32::try_from(y?).ok()?,
        u32::try_from(m?).ok()?,
        u32::try_from(d?).ok()?,
    )
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let f = fold(s);
    if let Some(m) = DATE_NUM.captures(&f) {
        if m.get(4).is_some() {
            return ymd(num(&m, 4), num(&m, 5), num(&m, 6));
        }
        // day first: es, de, fr and UK English; US layouts print ISO or month names
        return ymd(num(&m, 3), num(&m, 2), num(&m, 1));
    }
    if let Some(m) = DATE_WORD.captures(&f) {
        if m.get(1).is_some() {
            let mon = month(m.get(2)?.as_str())?;
            return ymd(num(&m, 3), Some(mon as i64), num(&m, 1));
        }
        let mon = month(m.get(4)?.as_str())?;
        return ymd(num(&m, 6), Some(mon as i64), num(&m, 5));
    }
    None
}

pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let d = parse_date(s)?;
    let rest = DATE_NUM.replace_all(&fold(s), " ").into_owned();
    let t = TIME
        .captures(&rest)
        .and_then(|m| NaiveTime::from_hms_opt(num(&m, 1)? as u32, num(&m, 2)? as u32, 0))
        .unwrap_or(NaiveTime::MIN);
    Some(d.and_time(t))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub doc_type: String,
    pub language: Option<String>,
    pub scores: BTreeMap<String, usize>,
    pub patient_name: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub sample_id: Option<String>,
    pub collected_at: Option<NaiveDateTime>,
    pub reported_at: Option<NaiveDateTime>,
    pub report_status: String,
}

impl Default for Classification {
    fn default() -> Self {
        Classification {
            doc_type: "other".into(),
            language: None,
            scores: BTreeMap::new(),
            patient_name: None,
            birth_date: None,
            sample_id: None,
            collected_at: None,
            reported_at: None,
            report_status: "final".into(),
        }
    }
}

/// Every label, longest first (Python's stable sort keeps the table order among equal lengths).
static ALL_LABELS: LazyLock<Vec<(&'static str, &'static str)>> = LazyLock::new(|| {
    let mut out: Vec<(&str, &str)> = LABELS
        .iter()
        .flat_map(|(k, labs)| labs.iter().map(move |l| (*k, *l)))
        .collect();
    out.sort_by_key(|(_, l)| std::cmp::Reverse(l.chars().count()));
    out
});

/// Character offset of a byte offset.
fn char_at(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

/// `Paciente: RIVERA, ALEX  F. nacimiento: 14/05/1979` -> {name: 'RIVERA, ALEX', birth: '14/05/1979'}.
pub fn label_values(line: &str) -> Vec<(&'static str, String)> {
    let f = fold(line);
    // (start, end) in characters of the folded line, as Python indexed it
    let mut hits: Vec<(usize, usize, &str)> = Vec::new();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    for (kind, lab) in ALL_LABELS.iter() {
        let mut from = 0usize;
        while let Some(rel) = f[from..].find(lab) {
            let i = from + rel;
            let j = i + lab.len();
            let (before, after) = crate::py::around(&f, i, j);
            if before.is_some_and(az) || after.is_some_and(az) {
                from = i + f[i..].chars().next().map_or(1, char::len_utf8);
                continue;
            }
            // \s*:? after the label
            let rest = &f[j..];
            let ws = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
            let mut end = j + ws;
            if f[end..].starts_with(':') {
                end += 1;
            }
            from = end.max(i + 1);
            let (a, b) = (char_at(&f, i), char_at(&f, end));
            if taken.iter().any(|(x, y)| a < *y && *x < b) {
                continue;
            }
            // a label needs a colon after it, or must start the line, to count (avoids "Patient" in prose)
            if !f[i..end].contains(':') && a != 0 {
                continue;
            }
            taken.push((a, b));
            hits.push((a, b, kind));
        }
    }
    hits.sort();
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for (i, (_a, b, kind)) in hits.iter().enumerate() {
        let end = if i + 1 < hits.len() { hits[i + 1].0 } else { chars.len() };
        let (b, end) = ((*b).min(chars.len()), end.min(chars.len()));
        let value: String = if b < end {
            chars[b..end].iter().collect()
        } else {
            String::new()
        };
        let value = strip_chars(&value, " :|;,\t").to_string();
        if !value.is_empty() && !out.iter().any(|(k, _)| k == kind) {
            let kind: &'static str = LABELS.iter().find(|(k, _)| k == kind).map_or("name", |(k, _)| k);
            out.push((kind, value));
        }
    }
    out
}

/// Guess type, language, patient block, dates and status from the first pages' text lines.
pub fn classify(lines: &[String]) -> Classification {
    let mut c = Classification::default();
    let folded: Vec<String> = lines.iter().map(|x| fold(x)).collect();
    let blob = folded.join("\n");
    let tokens: Vec<&str> = AZ_RUN.find_iter(&blob).map(|m| m.as_str()).collect();
    let lang_scores: Vec<(&str, usize)> = LANG_WORDS
        .iter()
        .map(|(lang, words)| (*lang, tokens.iter().filter(|t| words.contains(t)).count()))
        .collect();
    if lang_scores.iter().any(|(_, n)| *n > 0) {
        // Python's max keeps the first of equal scores
        let mut best = lang_scores[0];
        for s in &lang_scores[1..] {
            if s.1 > best.1 {
                best = *s;
            }
        }
        c.language = Some(best.0.to_string());
    }
    let mut best: Option<(&str, usize)> = None;
    for (t, words) in TYPE_WORDS.iter() {
        let n: usize = words.iter().map(|w| count_bounded(&blob, w, az)).sum();
        c.scores.insert(t.to_string(), n);
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((t, n));
        }
    }
    if let Some((t, n)) = best
        && n > 0
    {
        c.doc_type = t.to_string();
    }
    for line in lines {
        for (kind, value) in label_values(line) {
            match kind {
                "name" if c.patient_name.is_none() && TWO_LETTERS.is_match(&value) => c.patient_name = Some(value),
                "birth" if c.birth_date.is_none() => c.birth_date = parse_date(&value),
                "sample" if c.sample_id.is_none() => {
                    let tok = value.split_whitespace().next().unwrap_or("");
                    if DIGIT.is_match(tok) {
                        c.sample_id = Some(tok.to_string());
                    }
                }
                "collected" if c.collected_at.is_none() => c.collected_at = parse_datetime(&value),
                "reported" if c.reported_at.is_none() => c.reported_at = parse_datetime(&value),
                _ => {}
            }
        }
    }
    for (status, words) in STATUS.iter() {
        if words
            .iter()
            .any(|w| crate::py::occurrences(&blob, w).any(|i| !crate::py::around(&blob, i, i).0.is_some_and(az)))
        {
            c.report_status = status.to_string();
            break;
        }
    }
    c
}

// ------------------------------------------------------------------ identity ----

pub fn name_tokens(name: &str) -> BTreeSet<String> {
    AZ_RUN
        .find_iter(&fold(name))
        .map(|m| m.as_str())
        .filter(|t| t.len() > 1)
        .map(String::from)
        .collect()
}

/// matched | mismatch | missing | hinted (name or birth date alone matched, the other absent).
pub fn identity_match(
    page_name: Option<&str>,
    page_birth: Option<NaiveDate>,
    profile_name: &str,
    profile_birth: Option<NaiveDate>,
) -> &'static str {
    let page_name = page_name.filter(|n| !n.is_empty());
    if page_name.is_none() && page_birth.is_none() {
        return "missing";
    }
    let name_ok = page_name.map(|n| {
        let (got, want) = (name_tokens(n), name_tokens(profile_name));
        !want.is_empty() && want.is_subset(&got)
    });
    let birth_ok = match (page_birth, profile_birth) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };
    if name_ok == Some(false) || birth_ok == Some(false) {
        return "mismatch";
    }
    if name_ok == Some(true) && birth_ok == Some(true) {
        return "matched";
    }
    "hinted"
}

/// `has_bounded` with the `[a-z0-9]` neighbour rule the templates use.
pub fn has_word(folded: &str, word: &str) -> bool {
    has_bounded(folded, &fold(word), crate::py::az09)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("date")
    }

    #[test]
    fn test_parse_date() {
        for (s, want) in [
            ("14/05/1979", Some(d(1979, 5, 14))),
            ("14.05.1979", Some(d(1979, 5, 14))),
            ("1979-05-14", Some(d(1979, 5, 14))),
            ("14 May 1979", Some(d(1979, 5, 14))),
            ("May 14, 1979", Some(d(1979, 5, 14))),
            ("3 de marzo", None),
            ("31/02/2026", None),
        ] {
            assert_eq!(parse_date(s), want, "{s}");
        }
    }

    #[test]
    fn test_parse_datetime_keeps_the_time() {
        assert_eq!(
            parse_datetime("14/09/2026 08:15"),
            Some(d(2026, 9, 14).and_hms_opt(8, 15, 0).expect("time"))
        );
    }

    #[test]
    fn test_patient_block_dates_and_language_es() {
        let lines: Vec<String> = [
            "LabNorte",
            "Informe de resultados de laboratorio",
            "Paciente: RIVERA, ALEX      F. nacimiento: 14/05/1979",
            "Fecha de toma: 14/09/2026 08:15      Nº muestra: LN-1",
            "Fecha de informe: 14/09/2026 13:40      Página 1/3",
            "Prueba Resultado Unidades Valores de referencia",
            "HEMOGRAMA",
        ]
        .map(String::from)
        .to_vec();
        let c = classify(&lines);
        assert_eq!(
            (c.doc_type.as_str(), c.language.as_deref(), c.report_status.as_str()),
            ("lab_report", Some("es"), "final")
        );
        assert_eq!(c.patient_name.as_deref(), Some("RIVERA, ALEX"));
        assert_eq!(c.birth_date, Some(d(1979, 5, 14)));
        assert_eq!(c.collected_at, d(2026, 9, 14).and_hms_opt(8, 15, 0));
        assert_eq!(c.reported_at, d(2026, 9, 14).and_hms_opt(13, 40, 0));
        assert_eq!(c.sample_id.as_deref(), Some("LN-1"));
    }

    #[test]
    fn test_report_status() {
        for (line, status) in [
            ("Laborbefund - Vorläufiger Befund", "preliminary"),
            ("Laborbefund - KORRIGIERTER BEFUND", "amended"),
            ("Informe de resultados - INFORME RECTIFICADO", "amended"),
            ("Compte rendu provisoire", "preliminary"),
            ("Laboratory report", "final"),
        ] {
            assert_eq!(
                classify(&[line.into(), "Patient: X".into()]).report_status,
                status,
                "{line}"
            );
        }
    }

    #[test]
    fn test_non_lab_types() {
        let c = |l: &[&str]| classify(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>()).doc_type;
        assert_eq!(
            c(&["Informe de alta hospitalaria", "Fecha de ingreso: 01/02/2026"]),
            "discharge"
        );
        assert_eq!(
            c(&[
                "Radiology report",
                "Findings: no acute change",
                "Impression: normal MRI"
            ]),
            "imaging_report"
        );
        assert_eq!(c(&["Hola", "nada que ver"]), "other");
    }

    #[test]
    fn test_identity_match() {
        let born = Some(d(2017, 6, 21));
        assert_eq!(
            identity_match(Some("RIVERA, LUCÍA"), born, "Lucía Rivera", born),
            "matched"
        );
        assert_eq!(
            identity_match(Some("Lucia Rivera"), born, "Lucía Rivera", born),
            "matched"
        );
        assert_eq!(
            identity_match(Some("RIVERA, ALEX"), Some(d(1979, 5, 14)), "Lucía Rivera", born),
            "mismatch"
        );
        assert_eq!(
            identity_match(Some("RIVERA, LUCIA"), Some(d(2017, 6, 22)), "Lucía Rivera", born),
            "mismatch"
        );
        assert_eq!(
            identity_match(Some("RIVERA, LUCIA"), None, "Lucía Rivera", born),
            "hinted"
        );
        assert_eq!(identity_match(None, None, "Lucía Rivera", born), "missing");
    }

    #[test]
    fn fold_strips_accents_and_ordinals() {
        assert_eq!(fold("Nº Análisis"), "no analisis");
    }
}
