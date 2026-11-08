//! Per-token identity modes (`mcp/redact.py`, mcp.md 9.6), applied to the rendered response after
//! `state_hash`.
//!
//! | Mode         | Names and IDs                          | Providers       | Dates             | Free text |
//! |--------------|----------------------------------------|-----------------|-------------------|-----------|
//! | full         | as stored                              | as stored       | exact             | wrapped   |
//! | pseudonymous | slug; detectors scrub IDs, phones, ... | role ("lab")    | exact             | scrubbed  |
//! | minimal      | as pseudonymous                        | as pseudonymous | relative to as_of | dropped   |
//!
//! Structured values (numbers, units, codes, ranges) are never touched, so trends survive every mode.

use std::sync::LazyLock;

use chrono::NaiveDate;
use regex::Regex;
use serde_json::{Map, Value};

use crate::envelope::is_untrusted;

pub const MODES: [&str; 3] = ["full", "pseudonymous", "minimal"];

const PROVIDER_KEYS: [&str; 5] = ["provider", "provider_name", "lab", "clinician", "performer"];
const DROP_KEYS_PSEUDO: [&str; 7] = [
    "display_name",
    "patient_name",
    "address",
    "phone",
    "email",
    "mrn",
    "national_id",
];
const DATE_KEYS: [&str; 11] = [
    "date",
    "since",
    "until",
    "last",
    "on",
    "due",
    "started",
    "stopped",
    "effective",
    "accepted_at",
    "updated",
];

#[allow(clippy::unwrap_used)] // literal patterns
static DETECTORS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    vec![
        ("email", Regex::new(r"[\w.+-]+@[\w-]+\.[\w.-]+").unwrap()),
        ("iban", Regex::new(r"\b[A-Z]{2}\d{2}(?:\s?[A-Z0-9]{4}){3,7}\b").unwrap()),
        ("nie", Regex::new(r"\b[XYZ]-?\d{7}-?[A-Z]\b").unwrap()),
        ("dni", Regex::new(r"\b\d{8}-?[A-Z]\b").unwrap()),
        ("ssn", Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap()),
        ("nhs", Regex::new(r"\b\d{3}\s\d{3}\s\d{4}\b").unwrap()),
    ]
});
#[allow(clippy::unwrap_used)]
static ISO_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}(?:[T ][\d:.]+(?:Z|[+-]\d{2}:?\d{2})?)?$").unwrap());
#[allow(clippy::unwrap_used)]
static DIGIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d$").unwrap());
#[allow(clippy::unwrap_used)]
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

fn strictness(mode: &str) -> Option<usize> {
    MODES.iter().position(|m| *m == mode)
}

/// A call may only ask for a stricter mode than its token's.
pub fn stricter(token_mode: &str, asked: Option<&str>) -> String {
    if let Some(a) = asked
        && let Some(s) = strictness(a)
        && s > strictness(token_mode).unwrap_or(1)
    {
        return a.to_string();
    }
    if strictness(token_mode).is_some() {
        token_mode.to_string()
    } else {
        "pseudonymous".into()
    }
}

/// Python's `\d` on one character (Unicode decimal digits).
fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || (!c.is_ascii() && DIGIT.is_match(c.encode_utf8(&mut [0; 4])))
}

/// Python's `\w` on one character.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_phone_class(c: char) -> bool {
    is_digit(c) || c.is_whitespace() || matches!(c, '(' | ')' | '.' | '-')
}

/// `(?<![\w.])\+?\d[\d\s().-]{7,}\d(?![\w.])`, matched as Python's backtracking engine does
/// (the regex crate has no lookaround). Returns (start, end) byte ranges, left to right.
fn phone_matches(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = chars.len();
    let byte_at = |i: usize| if i < n { chars[i].0 } else { text.len() };
    let free = |i: usize| i >= n || !(is_word(chars[i].1) || chars[i].1 == '.');
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if i > 0 && !free(i - 1) {
            i += 1;
            continue;
        }
        let mut p = i;
        if chars[p].1 == '+' {
            p += 1;
        }
        if p >= n || !is_digit(chars[p].1) {
            i += 1;
            continue;
        }
        let run_start = p + 1;
        let mut run_end = run_start;
        while run_end < n && is_phone_class(chars[run_end].1) {
            run_end += 1;
        }
        // the class run takes k >= 7 characters, then one digit, then no word char or dot
        let mut found = None;
        let max_k = run_end.saturating_sub(run_start);
        let mut k = max_k;
        while k >= 7 {
            let last = run_start + k;
            if last < n && is_digit(chars[last].1) && free(last + 1) {
                found = Some(last + 1);
                break;
            }
            if k == 7 {
                break;
            }
            k -= 1;
        }
        match found {
            Some(end) => {
                out.push((byte_at(i), byte_at(end)));
                i = end;
            }
            None => i += 1,
        }
    }
    out
}

/// `(?<!\w)NAME(?!\w)` case-insensitively, left to right without overlaps.
fn name_matches(re: &Regex, text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= text.len() {
        let Some(m) = re.find_at(text, pos) else { break };
        let before_ok = text[..m.start()].chars().next_back().is_none_or(|c| !is_word(c));
        let after_ok = text[m.end()..].chars().next().is_none_or(|c| !is_word(c));
        if before_ok && after_ok && m.end() > m.start() {
            out.push((m.start(), m.end()));
            pos = m.end();
        } else {
            pos = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

fn replace_ranges(text: &str, ranges: &[(usize, usize)], with: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for &(s, e) in ranges {
        out.push_str(&text[last..s]);
        out.push_str(&with(&text[s..e]));
        last = e;
    }
    out.push_str(&text[last..]);
    out
}

/// Scrub identifiers (email, IBAN, NIE, DNI, SSN, NHS, phone) and household names from free text.
pub fn scrub(text: &str, names: &[String]) -> String {
    let mut text = text.to_string();
    for (label, re) in DETECTORS.iter() {
        text = re.replace_all(&text, format!("[{label}]").as_str()).into_owned();
        if *label == "nhs" {
            // phone runs after nhs in Python's detector order: 9+ digits, so dates and lab values stay
            let ranges = phone_matches(&text);
            text = replace_ranges(&text, &ranges, |m| {
                if m.chars().filter(|c| is_digit(*c)).count() >= 9 {
                    "[phone]".into()
                } else {
                    m.to_string()
                }
            });
        }
    }
    for name in names.iter().filter(|n| !n.is_empty()) {
        let Ok(re) = Regex::new(&format!("(?i){}", regex::escape(name))) else {
            continue;
        };
        let ranges = name_matches(&re, &text);
        text = replace_ranges(&text, &ranges, |_| "[name]".into());
    }
    text
}

/// Full names and each part of 3+ letters, longest first, so 'Alex Rivera' goes before 'Alex'.
pub fn name_dictionary(full_names: &[String]) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for n in full_names.iter().filter(|n| !n.is_empty()) {
        parts.push(n.clone());
        parts.extend(SPACES.split(n).filter(|p| p.chars().count() >= 3).map(str::to_string));
    }
    parts.sort();
    parts.dedup();
    parts.sort_by_key(|p| std::cmp::Reverse(p.chars().count()));
    parts
}

fn parse_day(value: &str) -> Option<NaiveDate> {
    if value.contains('T') {
        let v = value.replace('Z', "+00:00");
        let date_part = v.split('T').next()?;
        // datetime.fromisoformat(...).date(): the date in the value's own offset
        crate::tools::parse_iso_datetime(&v)?;
        NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()
    } else {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
    }
}

/// "12 days before as_of" for an ISO date or datetime; anything else stays as it is.
pub fn relative(value: &str, as_of: NaiveDate) -> String {
    let Some(d) = parse_day(value) else {
        return value.to_string();
    };
    let days = (as_of - d).num_days();
    match days {
        0 => "same day as as_of".into(),
        d if d > 0 => format!("{d} days before as_of"),
        d => format!("{} days after as_of", -d),
    }
}

pub fn age_band(age: Option<i32>) -> Option<String> {
    let age = age?;
    if age < 18 {
        return Some("0-17".into());
    }
    let lo = age.div_euclid(10) * 10;
    Some(format!("{lo}-{}", lo + 9))
}

fn go(node: &Value, key: Option<&str>, mode: &str, as_of: NaiveDate, names: &[String]) -> Value {
    if is_untrusted(node) {
        if mode == "minimal" {
            return Value::Null;
        }
        let mut m = node.as_object().cloned().unwrap_or_default();
        if let Some(Value::String(t)) = m.get("text") {
            let s = scrub(t, names);
            m.insert("text".into(), Value::String(s));
        }
        return Value::Object(m);
    }
    match node {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if DROP_KEYS_PSEUDO.contains(&k.as_str()) {
                    continue;
                }
                if PROVIDER_KEYS.contains(&k.as_str())
                    && !matches!(v, Value::Null | Value::Array(_) | Value::Number(_) | Value::Bool(_))
                {
                    let role = if ["lab", "provider", "provider_name"].contains(&k.as_str()) {
                        "lab"
                    } else {
                        "clinician"
                    };
                    out.insert(k.clone(), Value::String(role.into()));
                    continue;
                }
                out.insert(k.clone(), go(v, Some(k), mode, as_of, names));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| go(v, key, mode, as_of, names)).collect()),
        Value::String(s)
            if mode == "minimal" && key.is_some_and(|k| DATE_KEYS.contains(&k)) && ISO_DATE.is_match(s) =>
        {
            Value::String(relative(s, as_of))
        }
        other => other.clone(),
    }
}

/// A redacted copy of `data` (the envelope's data block or any part of a response).
pub fn apply(data: Value, mode: &str, as_of: NaiveDate, names: &[String]) -> Value {
    if mode == "full" {
        return data;
    }
    go(&data, None, mode, as_of, names)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn detectors_leave_dates_and_values_alone() {
        assert_eq!(scrub("LDL 97 mg/dL on 2026-09-14", &[]), "LDL 97 mg/dL on 2026-09-14");
        assert_eq!(scrub("IBAN ES91 2100 0418 4502 0005 1332", &[]), "IBAN [iban]");
        assert_eq!(stricter("full", Some("minimal")), "minimal");
        assert_eq!(stricter("minimal", Some("full")), "minimal");
        assert_eq!(stricter("weird", None), "pseudonymous");
        assert_eq!(age_band(Some(8)).as_deref(), Some("0-17"));
        assert_eq!(age_band(Some(47)).as_deref(), Some("40-49"));
    }

    #[test]
    fn scrub_matches_python() {
        // the expected strings are redact.scrub's output in Python 0.1.1
        let names = name_dictionary(&["Alex Rivera".into(), "Lucía Rivera".into()]);
        assert_eq!(
            scrub(
                "Alex Rivera asked: call me at +34 600 123 456 or alex.rivera@example.com, DNI 12345678Z.",
                &names
            ),
            "[name] asked: call me at [phone] or [email], DNI [dni]."
        );
        assert_eq!(
            scrub("tel 600-123-45 and 2026-09-14 12:30", &[]),
            "tel 600-123-45 and [phone]:30"
        );
        assert_eq!(
            scrub("x.600123456 y 600123456z (600) 123 4567", &[]),
            "x.600123456 y 600123456z ([phone]"
        );
        assert_eq!(
            scrub("ALEXANDRA met alex, Lucía.", &names),
            "ALEXANDRA met [name], [name]."
        );
    }

    #[test]
    fn relative_dates() {
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        assert_eq!(relative("2026-09-14", as_of), "54 days before as_of");
        assert_eq!(relative("2026-11-07T08:00:00Z", as_of), "same day as as_of");
        assert_eq!(relative("2026-11-09", as_of), "2 days after as_of");
        assert_eq!(relative("2026-11-07 08:00", as_of), "2026-11-07 08:00");
    }
}
