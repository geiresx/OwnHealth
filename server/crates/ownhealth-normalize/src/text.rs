//! Printed values, ranges and flags (`normalize/_text.py`, ingestion.md 8.1, 8.4, 8.5). The locale
//! decides separators.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use rust_decimal::Decimal;
use serde_json::{Map, Value};

use crate::RefRange;
use crate::tables::{fold, strip_chars};

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new(&$pat).expect("static regex"));
    };
}

// UCUM count-per-volume codes: 10*3/uL, /uL, /mm3 (8.1: only these read 1.200 as 1200)
re!(COUNT, r"^(?:10\*\d+)?/(?:u|m|n)?L$|^/mm3$|^10\*\d+/(?:u|m|n)?L$");
re!(NUMBER_CHARS, r"[\s' ]");
re!(NUMBER, r"^(?:[+-]?(?:\d[\d.,]*|[.,]\d+))$");
re!(FLAG_TAIL, r"\s*(?:\*+|[↑↓]|\([hl+-]\)|\b[hl]\b)\s*$");
re!(
    LOOKS_LIKE_RANGE,
    r"^(?:\s*[+-]?\d[\d.,]*\s*(?:-|–|—|a|to|bis)\s*\d[\d.,]*\s*)$"
);
re!(ENTRY_SPLIT, r"[;\n|]");

/// folded prefix -> comparator, longest first where they overlap
const COMPARATORS: [(&str, &str); 20] = [
    ("<=", "<="),
    (">=", ">="),
    ("=<", "<="),
    ("=>", ">="),
    ("≤", "<="),
    ("≥", ">="),
    ("<", "<"),
    (">", ">"),
    ("inferior a", "<"),
    ("menor de", "<"),
    ("menor que", "<"),
    ("superior a", ">"),
    ("mayor de", ">"),
    ("mayor que", ">"),
    ("kleiner als", "<"),
    ("grosser als", ">"),
    ("inferieur a", "<"),
    ("superieur a", ">"),
    ("less than", "<"),
    ("greater than", ">"),
];

pub fn text_code(s: &str) -> Option<&'static str> {
    Some(match s {
        "negativo" | "negativa" | "negative" | "negativ" | "negatif" | "neg" | "-" => "negative",
        "positivo" | "positiva" | "positive" | "positiv" | "positif" | "pos" | "+" | "++" | "+++" | "++++" => {
            "positive"
        }
        "indetectable" | "no detectado" | "no detectable" | "not detected" | "undetectable" | "nicht nachweisbar"
        | "non detecte" | "absence" | "absent" | "ausencia" | "ausente" => "not_detected",
        "trazas" | "indicios" | "trace" | "traces" | "spuren" => "trace",
        _ => return None,
    })
}

fn is_negative_code(c: &str) -> bool {
    c == "negative" || c == "not_detected"
}

pub fn is_count_unit(ucum: Option<&str>) -> bool {
    ucum.is_some_and(|u| !u.is_empty() && COUNT.is_match(u))
}

/// Python's `repr()` of a str.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !py_printable(c) => {
                let n = c as u32;
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `str.isprintable()` for one character: not a control, format, separator (other than space),
/// surrogate or private-use character.
fn py_printable(c: char) -> bool {
    let n = c as u32;
    if c == ' ' {
        return true;
    }
    !(n < 0x20
        || (0x7f..=0xa0).contains(&n)
        || n == 0xad
        || n == 0x1680
        || (0x2000..=0x200f).contains(&n)
        || (0x2028..=0x202f).contains(&n)
        || (0x205f..=0x2064).contains(&n)
        || (0x2066..=0x206f).contains(&n)
        || n == 0x3000
        || n == 0xfeff
        || (0xe000..=0xf8ff).contains(&n)
        || (0xfff9..=0xfffb).contains(&n)
        || n >= 0xf0000)
}

fn grouped(s: &str, sep: char) -> bool {
    let parts: Vec<&str> = s.split(sep).collect();
    let first = parts[0].chars().count();
    (1..=3).contains(&first)
        && parts[1..].iter().all(|p| p.chars().count() == 3)
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(char::is_numeric))
}

/// One printed number with the locale's separators -> (value, reason). Reason is a review code.
pub fn parse_number(token: &str, locale: &str, count_unit: bool) -> (Option<Decimal>, Option<&'static str>) {
    let t = NUMBER_CHARS.replace_all(token, "");
    if !NUMBER.is_match(&t) {
        return (None, None);
    }
    let mut t: &str = &t;
    let mut sign = "";
    if t.starts_with('+') || t.starts_with('-') {
        (sign, t) = t.split_at(1);
    }
    let mut reason = None;
    let body: String;
    if t.contains('.') && t.contains(',') {
        let dec = if t.rfind('.') > t.rfind(',') { '.' } else { ',' };
        let grp = if dec == '.' { ',' } else { '.' };
        let Some((head, tail)) = t.rsplit_once(dec) else {
            return (None, Some("suspect_separator"));
        };
        if !grouped(head, grp) {
            return (None, Some("suspect_separator"));
        }
        body = format!("{}.{}", head.replace(grp, ""), tail);
    } else if t.contains('.') || t.contains(',') {
        let sep = if t.contains(',') { ',' } else { '.' };
        let decimal_sep = if locale == "en" { '.' } else { ',' };
        if t.matches(sep).count() > 1 {
            if !grouped(t, sep) {
                return (None, Some("suspect_separator"));
            }
            body = t.replace(sep, ""); // 1.234.567: only a grouping reading exists
        } else if sep == decimal_sep || !grouped(t, sep) {
            body = t.replace(sep, ".");
        } else if count_unit || locale == "en" {
            body = t.replace(sep, ""); // 6.800 /µL in an es report is 6800; 1,200 in an en report is 1200
        } else {
            reason = Some("ambiguous_separator"); // 1.200 mg/dL in an es report: 1.2 or 1200?
            body = t.replace(sep, ".");
        }
    } else {
        body = t.to_string();
    }
    match py_decimal(&format!("{sign}{body}")) {
        Some(d) => (Some(d), reason),
        None => (None, Some("suspect_separator")),
    }
}

/// `Decimal(s)` for the plain numbers parse_number builds ("5.", ".5", "-0.70").
fn py_decimal(s: &str) -> Option<Decimal> {
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() || digits == "." || !digits.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let int = if int.is_empty() { "0" } else { int };
    let text = if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    };
    Decimal::from_str_exact(&text).ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub value: Option<Decimal>,
    pub comparator: Option<&'static str>,
    pub text_code: Option<&'static str>,
    pub reason: Option<&'static str>,
}

pub fn parse_value(printed: &str, locale: &str, ucum: Option<&str>) -> Parsed {
    let f = fold(printed);
    let f = FLAG_TAIL.replace_all(&f, "");
    let mut f: &str = f.trim();
    if let Some(code) = text_code(strip_chars(f, "() ")) {
        return Parsed {
            value: None,
            comparator: None,
            text_code: Some(code),
            reason: None,
        };
    }
    let mut cmp = None;
    for (word, c) in COMPARATORS {
        if let Some(rest) = f.strip_prefix(word) {
            cmp = Some(c);
            f = rest.trim();
            break;
        }
    }
    let (num, reason) = parse_number(f, locale, is_count_unit(ucum));
    Parsed {
        value: num,
        comparator: if num.is_some() { cmp } else { None },
        text_code: None,
        reason,
    }
}

pub fn looks_like_range(printed: &str) -> bool {
    LOOKS_LIKE_RANGE.is_match(printed)
}

// -------------------------------------------------------------------- ranges ----

const SEX_MALE: &str = r"hombres?|varon(?:es)?|masculino|male|men|maenner|manner|mann|hommes?";
const SEX_FEMALE: &str = r"mujer(?:es)?|femenino|female|women|frauen|frau|femmes?";
const PREG: &str = r"embarazo|embarazada|gestacion|pregnan\w*|schwanger\w*|grossesse|enceinte|\d\s*(?:er|o|º|ª|st|nd|rd)?\s*trimest\w*";
const ADULT: &str = r"adultos|adults?|erwachsene|adultes";
const CHILD: &str = r"ninos|children|kinder|enfants";
const YEARS: &str = r"(?:anos|years?|y|jahre|ans)\b";
const NUM: &str = r"[+-]?\d[\d.,]*";

re!(
    LABEL,
    format!(
        r"\b(?:(?P<male>{SEX_MALE})|(?P<female>{SEX_FEMALE})|(?P<preg>{PREG})|(?P<adult>{ADULT})|(?P<child>{CHILD}))\b\s*[:=]?"
    )
);
re!(AGE_SPAN, format!(r"(\d+)\s*(?:-|a|to|bis)\s*(\d+)\s*{YEARS}"));
re!(
    AGE_OPEN,
    format!(r"(>=|<=|>|<|mayores de|menores de|over|under|ab|plus de|moins de)\s*(\d+)\s*{YEARS}")
);
re!(
    SPAN,
    format!(r"(?P<lo>{NUM})\s*(?:-|–|—|\ba\b|\bto\b|\bbis\b)\s*(?P<hi>\d[\d.,]*)")
);
re!(
    UPPER,
    format!(
        r"(?P<op><=|=<|≤|<|hasta|up to|bis|jusqu'?a|inferior a|menor de|menor que|kleiner als|inferieur a)\s*(?P<hi>{NUM})"
    )
);
re!(
    LOWER,
    format!(r"(?P<op>>=|=>|≥|>|superior a|mayor de|mayor que|grosser als|superieur a|desde)\s*(?P<lo>{NUM})")
);

fn range(low: Option<Decimal>, high: Option<Decimal>, text: &str, applies: Map<String, Value>) -> RefRange {
    RefRange {
        low,
        high,
        text: text.to_string(),
        applies_to: Value::Object(applies),
        low_inclusive: true,
        high_inclusive: true,
        text_code: None,
    }
}

/// A printed reference range into one range per condition ("Mujeres: 12-16 Hombres: 13-17").
pub fn parse_range(printed: &str, locale: &str, ucum: Option<&str>) -> Vec<RefRange> {
    if printed.trim().is_empty() {
        return Vec::new();
    }
    let count = is_count_unit(ucum);
    let mut out: Vec<RefRange> = Vec::new();
    let single = ENTRY_SPLIT.split(printed.trim()).count() == 1;
    // A line with a label and no value of its own ("Hombres:") continues on the next line. Split
    // before folding: fold turns line breaks into spaces, and a line break separates entries.
    let mut carry = String::new();
    for line in ENTRY_SPLIT.split(printed) {
        let chunk = if carry.is_empty() {
            fold(line)
        } else {
            format!("{carry} {}", fold(line)).trim().to_string()
        };
        let found = chunk_ranges(&chunk, locale, count);
        carry = if found.is_empty() { chunk } else { String::new() };
        out.extend(found);
    }
    if single && out.len() == 1 && applies_empty(&out[0].applies_to) {
        out[0].text = printed.trim().to_string();
        out[0].applies_to = Value::Object(Map::new());
    }
    out
}

fn applies_empty(v: &Value) -> bool {
    v.as_object().is_none_or(Map::is_empty)
}

fn chunk_ranges(chunk: &str, locale: &str, count: bool) -> Vec<RefRange> {
    let labels: Vec<Captures> = LABEL.captures_iter(chunk).collect();
    let mut pieces: Vec<(Map<String, Value>, &str)> = Vec::new();
    match labels.first() {
        None => pieces.push((Map::new(), chunk)),
        Some(first) => {
            let start = first.get(0).map_or(0, |m| m.start());
            if !strip_chars(&chunk[..start], " ,").is_empty() {
                pieces.push((Map::new(), &chunk[..start]));
            }
            for (i, m) in labels.iter().enumerate() {
                let end = labels
                    .get(i + 1)
                    .and_then(|n| n.get(0))
                    .map_or(chunk.len(), |n| n.start());
                let me = m.get(0).map_or(0, |x| x.end());
                pieces.push((label_applies(m), &chunk[me..end]));
            }
        }
    }
    pieces
        .into_iter()
        .filter_map(|(applies, text)| one_range(text, applies, locale, count))
        .collect()
}

fn label_applies(m: &Captures) -> Map<String, Value> {
    let mut a = Map::new();
    if m.name("male").is_some() {
        a.insert("sex".into(), "male".into());
    } else if m.name("female").is_some() {
        a.insert("sex".into(), "female".into());
    } else if m.name("preg").is_some() {
        a.insert("pregnancy".into(), true.into());
    } else if m.name("adult").is_some() {
        a.insert("age_min".into(), 18.into());
    } else {
        a.insert("age_max".into(), 17.into());
    }
    a
}

fn cut(t: &str, start: usize, end: usize) -> String {
    let joined = format!("{}{}", &t[..start], &t[end..]);
    strip_chars(&joined, " ,:").to_string()
}

fn one_range(text: &str, mut applies: Map<String, Value>, locale: &str, count: bool) -> Option<RefRange> {
    let mut t = strip_chars(text, " ,:").to_string();
    if let Some(m) = AGE_SPAN.captures(&t) {
        let lo: i64 = m[1].parse().ok()?;
        let hi: i64 = m[2].parse().ok()?;
        applies.insert("age_min".into(), lo.into());
        applies.insert("age_max".into(), hi.into());
        let whole = m.get(0)?;
        t = cut(&t, whole.start(), whole.end());
    } else if let Some(m) = AGE_OPEN.captures(&t) {
        let n: i64 = m[2].parse().ok()?;
        let op = &m[1];
        if matches!(op, ">" | ">=" | "mayores de" | "over" | "ab" | "plus de") {
            let bump = i64::from(matches!(op, ">" | "mayores de" | "over" | "plus de"));
            applies.insert("age_min".into(), (n + bump).into());
        } else {
            let bump = i64::from(matches!(op, "<" | "menores de" | "under" | "moins de"));
            applies.insert("age_max".into(), (n - bump).into());
        }
        let whole = m.get(0)?;
        t = cut(&t, whole.start(), whole.end());
    }
    if t.is_empty() {
        return None;
    }
    let label = strip_chars(text, " ,:");
    if let Some(m) = SPAN.captures(&t) {
        let (lo, _) = parse_number(&m["lo"], locale, count);
        let (hi, _) = parse_number(&m["hi"], locale, count);
        if lo.is_some() && hi.is_some() {
            return Some(range(lo, hi, label, applies));
        }
    }
    if let Some(m) = UPPER.captures(&t) {
        let (hi, _) = parse_number(&m["hi"], locale, count);
        if hi.is_some() {
            let mut r = range(None, hi, label, applies);
            r.high_inclusive = &m["op"] != "<";
            return Some(r);
        }
    }
    if let Some(m) = LOWER.captures(&t) {
        let (lo, _) = parse_number(&m["lo"], locale, count);
        if lo.is_some() {
            let mut r = range(lo, None, label, applies);
            r.low_inclusive = &m["op"] != ">";
            return Some(r);
        }
    }
    let code = text_code(strip_chars(&t, "() "))?;
    let mut r = range(None, None, label, applies);
    r.text_code = Some(code.into());
    Some(r)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// The range that applies to this person; None when none does or two equally specific disagree.
pub fn pick_range<'r>(ranges: &'r [RefRange], sex: Option<&str>, age_years: Option<i32>) -> Option<&'r RefRange> {
    let empty = Map::new();
    let applies = |r: &'r RefRange| r.applies_to.as_object().unwrap_or(&empty).clone();
    let mut fits: Vec<(&RefRange, usize)> = Vec::new();
    for r in ranges {
        let a = applies(r);
        if truthy(a.get("pregnancy")) {
            continue; // we do not know a pregnancy stage in v0.1: never pick a trimester range
        }
        if let Some(s) = a.get("sex")
            && s.as_str() != sex
        {
            continue;
        }
        let min = a.get("age_min");
        let max = a.get("age_max");
        if min.is_some() || max.is_some() {
            let Some(age) = age_years else { continue };
            let age = f64::from(age);
            if min.is_some_and(|m| m.as_f64().is_none_or(|m| age < m)) {
                continue;
            }
            if max.is_some_and(|m| m.as_f64().is_none_or(|m| age > m)) {
                continue;
            }
        }
        fits.push((r, a.len()));
    }
    let best = fits.iter().map(|(_, n)| *n).max()?;
    let top: Vec<&RefRange> = fits.iter().filter(|(_, n)| *n == best).map(|(r, _)| *r).collect();
    let first = top[0];
    if top
        .iter()
        .any(|r| (r.low, r.high, &r.text_code) != (first.low, first.high, &first.text_code))
    {
        return None;
    }
    Some(first)
}

/// L, N, H or A from the printed range only (HL7 ObservationInterpretation). None without a range.
pub fn interpret(
    value: Option<Decimal>,
    text_code: Option<&str>,
    r: Option<&RefRange>,
    comparator: Option<&str>,
) -> Option<&'static str> {
    let r = r?;
    if let Some(expected) = r.text_code.as_deref() {
        let tc = text_code?;
        let same = tc == expected || (is_negative_code(tc) && is_negative_code(expected));
        return Some(if same { "N" } else { "A" });
    }
    let value = value?;
    if r.low.is_none() && r.high.is_none() {
        return None;
    }
    let (lo, hi) = (r.low, r.high);
    let below = lo.is_some_and(|lo| value < lo || (value == lo && !r.low_inclusive));
    let above = hi.is_some_and(|hi| value > hi || (value == hi && !r.high_inclusive));
    if matches!(comparator, Some("<" | "<=")) {
        // the true value is at most `value` (below it for "<")
        if below || (lo == Some(value) && comparator == Some("<")) {
            return Some("L");
        }
        // "<5" against a range "<5" is inside it: the endpoint is the comparator's, not the range's
        let under_hi = hi.is_none_or(|hi| value < hi || (value == hi && (comparator == Some("<") || r.high_inclusive)));
        if lo.is_none() && under_hi {
            return Some("N");
        }
        return None;
    }
    if matches!(comparator, Some(">" | ">=")) {
        // the true value is at least `value` (above it for ">")
        if above || (hi == Some(value) && comparator == Some(">")) {
            return Some("H");
        }
        let over_lo = lo.is_none_or(|lo| value > lo || (value == lo && (comparator == Some(">") || r.low_inclusive)));
        if hi.is_none() && over_lo {
            return Some("N");
        }
        return None;
    }
    Some(if below {
        "L"
    } else if above {
        "H"
    } else {
        "N"
    })
}

fn flag_word(s: &str) -> Option<&'static str> {
    Some(match s {
        "h" | "high" | "alto" | "alta" | "hoch" | "haut" | "eleve" | "elevado" | "elevada" | "↑" | "+" | ">" => "H",
        "l" | "low" | "bajo" | "baja" | "niedrig" | "bas" | "↓" | "<" | "-" | "disminuido" => "L",
        "hh" | ">>" | "↑↑" | "critico alto" | "critical high" => "HH",
        "ll" | "<<" | "↓↓" | "critico bajo" | "critical low" => "LL",
        "*" | "**" | "a" | "abnormal" | "anormal" | "anomalo" | "pathologisch" | "patologico" => "A",
        "n" | "normal" => "N",
        _ => return None,
    })
}

pub fn printed_flag_code(printed: Option<&str>) -> Option<&'static str> {
    let printed = printed?;
    if printed.trim().is_empty() {
        return None;
    }
    let folded = fold(printed);
    let key = strip_chars(&folded, "() ");
    flag_word(if key.is_empty() { printed.trim() } else { key })
}

/// A printed flag that contradicts the range: a strong sign of a mis-read range or column shift.
pub fn flag_disagrees(printed: Option<&str>, computed: Option<&str>) -> bool {
    let (Some(p), Some(c)) = (printed, computed) else {
        return false;
    };
    match p {
        "H" | "HH" => !matches!(c, "H" | "HH"),
        "L" | "LL" => !matches!(c, "L" | "LL"),
        "A" => c == "N",
        "N" => c != "N",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_like_python() {
        assert_eq!(py_repr("mg/dL"), "'mg/dL'");
        assert_eq!(py_repr("jusqu'a"), "\"jusqu'a\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("1\u{a0}2\n"), "'1\\xa02\\n'");
        assert_eq!(py_repr("µg"), "'µg'");
    }

    #[test]
    fn decimals_keep_their_printed_scale() {
        assert_eq!(py_decimal("1.200").map(|d| d.scale()), Some(3));
        assert_eq!(py_decimal("5.").map(|d| d.to_string()), Some("5".into()));
        assert_eq!(py_decimal(".5").map(|d| d.to_string()), Some("0.5".into()));
        assert_eq!(py_decimal("-0.70").map(|d| d.to_string()), Some("-0.70".into()));
    }
}
