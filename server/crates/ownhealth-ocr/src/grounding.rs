//! Grounding (lane C; ingestion.md 7.2, security-privacy.md 4.2 #5): an extracted value counts only
//! when it is on the same page's word layer (the text layer, or our OCR pass). The match tolerates the
//! decimal separator: `12,3` = `12.3`, and `250.000` = `250000` in a Spanish thousands context. OCR rows
//! and agent proposals (lane F) both use it. A page with no word layer gives `UngroundedNoWordLayer`:
//! the row goes to review as `ungrounded_no_word_layer`, never as grounded.
//!
//! The value decides. The unit, when given, only picks the best of several hits and widens the box:
//! a printed unit and a normalised one (`x10^3/µL` against `10*3/uL`) need not be spelled alike. When
//! `near` is given (the box the extractor or the agent claimed), only words inside it count.

use ownhealth_ingest_pdf::Word;
use ownhealth_ingest_pdf::classify::fold;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "grounding", rename_all = "snake_case")]
pub enum Grounding {
    /// Found; `bbox` covers the matched words.
    Grounded { bbox: [f64; 4] },
    /// The page has words but not this value: confidence 0, red marker in review.
    Ungrounded,
    /// The page has no word layer at all (a scan while the `ocr` profile is off).
    UngroundedNoWordLayer,
}

impl Grounding {
    /// The `candidate_row.grounding` value.
    pub fn code(&self) -> &'static str {
        match self {
            Grounding::Grounded { .. } => "grounded",
            Grounding::Ungrounded => "ungrounded",
            Grounding::UngroundedNoWordLayer => "ungrounded_no_word_layer",
        }
    }
}

/// How far outside a claimed box a word's centre may sit, in points (a box drawn by hand or by an
/// agent is rarely tight).
const NEAR_PAD: f64 = 4.0;

/// Is `value` (with `unit`, when given) on this page's words, inside `near` when given? Pure; the
/// caller loads the words (`ownhealth_ingest_pdf::page_words`). Hidden words never ground a value.
pub fn ground(words: &[Word], value: &str, unit: Option<&str>, near: Option<[f64; 4]>) -> Grounding {
    let visible: Vec<&Word> = words.iter().filter(|w| w.hidden.is_none()).collect();
    if visible.is_empty() {
        return Grounding::UngroundedNoWordLayer;
    }
    let want: Vec<String> = value.split_whitespace().map(String::from).collect();
    if want.is_empty() {
        return Grounding::Ungrounded;
    }
    let pool: Vec<&Word> = visible
        .into_iter()
        .filter(|w| near.is_none_or(|b| centre_inside(&w.bbox, &b)))
        .collect();
    let lines = lines(&pool);
    let unit_key = unit.map(fold_unit).filter(|u| !u.is_empty());
    let mut best: Option<([f64; 4], bool)> = None;
    for line in &lines {
        for start in 0..line.len() {
            let Some(end) = match_at(line, start, &want) else {
                continue;
            };
            let mut bbox = union(line[start..end].iter().map(|w| w.bbox));
            // the unit: glued to the value, or the next word on the line
            let glued = unit_key.as_ref().is_some_and(|u| {
                line.get(end - 1)
                    .is_some_and(|w| fold_unit(&w.text).ends_with(u.as_str()))
            });
            let next = unit_key
                .as_ref()
                .and_then(|u| line.get(end).filter(|w| fold_unit(&w.text) == *u));
            if let Some(w) = next {
                bbox = union([bbox, w.bbox]);
            }
            let with_unit = glued || next.is_some();
            if best.is_none_or(|(_, u)| with_unit && !u) {
                best = Some((bbox, with_unit));
            }
        }
    }
    match best {
        Some((bbox, _)) => Grounding::Grounded { bbox },
        None => Grounding::Ungrounded,
    }
}

/// The words `line[start..end]` spell `want`, token by token; the end index.
fn match_at(line: &[&Word], start: usize, want: &[String]) -> Option<usize> {
    let end = start + want.len();
    let words = line.get(start..end)?;
    let first_numeric = want.len() == 1 && numbers(&want[0]).is_some();
    if first_numeric {
        // one word: the value alone, or with a comparator, a flag or the unit glued to it
        let w = words.first()?;
        return same_number(&want[0], &w.text).then_some(end);
    }
    words
        .iter()
        .zip(want)
        .all(|(w, t)| same_token(t, &w.text))
        .then_some(end)
}

fn same_token(want: &str, got: &str) -> bool {
    if numbers(want).is_some() {
        return same_number(want, got);
    }
    let (a, b) = (fold(want), fold(got));
    let trim = |s: &str| s.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
    trim(&a) == trim(&b) && !trim(&a).is_empty()
}

/// Both read as the same number under some separator convention.
fn same_number(want: &str, got: &str) -> bool {
    let (Some(a), Some(b)) = (numbers(want), numbers(leading_number(got))) else {
        return false;
    };
    a.iter()
        .any(|x| b.iter().any(|y| (x - y).abs() <= 1e-9 * x.abs().max(1.0)))
}

/// The number at the start of a word, after a comparator: `<0,5` -> `0,5`, `14,2H` -> `14,2`,
/// `6.1mg/dL` -> `6.1`.
fn leading_number(s: &str) -> &str {
    let s = s.trim_start_matches(['<', '>', '≤', '≥', '=', '~', '+']);
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i > 0 && (c == '.' || c == ',')) || (i == 0 && c == '-')))
        .map_or(s.len(), |(i, _)| i);
    s[..end].trim_end_matches(['.', ','])
}

/// Every value a printed number can mean: `12,3` -> 12.3; `1.234` -> 1.234 or 1234; `250.000` ->
/// 250 or 250000; `1,234.5` -> 1234.5. None when it is not a number.
fn numbers(s: &str) -> Option<Vec<f64>> {
    let s = s.trim().trim_start_matches(['<', '>', '≤', '≥', '=', '~', '+']).trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if body.is_empty()
        || !body.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        || !body.starts_with(|c: char| c.is_ascii_digit())
        || !body.ends_with(|c: char| c.is_ascii_digit())
    {
        return None;
    }
    let sign = if neg { -1.0 } else { 1.0 };
    let mut out = Vec::new();
    let mut push = |t: String| {
        if let Ok(v) = t.parse::<f64>() {
            out.push(sign * v);
        }
    };
    let (dots, commas) = (body.matches('.').count(), body.matches(',').count());
    match (dots, commas) {
        (0, 0) => push(body.to_string()),
        // one separator: decimal, or thousands when three digits follow
        (1, 0) | (0, 1) => {
            let sep = if dots == 1 { '.' } else { ',' };
            push(body.replace(sep, "."));
            if body.rsplit(sep).next().is_some_and(|tail| tail.len() == 3) {
                push(body.replace(sep, ""));
            }
        }
        // repeated separator: thousands groups
        (_, 0) => push(body.replace('.', "")),
        (0, _) => push(body.replace(',', "")),
        // both: the last one is the decimal separator
        _ => {
            let last = body.rfind(['.', ',']).unwrap_or(0);
            let (int, frac) = body.split_at(last);
            push(format!("{}.{}", int.replace(['.', ','], ""), &frac[1..]));
        }
    }
    (!out.is_empty()).then_some(out)
}

fn fold_unit(s: &str) -> String {
    fold(s)
        .replace(['µ', 'μ'], "u")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn centre_inside(b: &[f64; 4], outer: &[f64; 4]) -> bool {
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    outer[0] - NEAR_PAD <= cx && cx <= outer[2] + NEAR_PAD && outer[1] - NEAR_PAD <= cy && cy <= outer[3] + NEAR_PAD
}

/// Words grouped into lines (vertical overlap of at least half the smaller height), each left to right.
fn lines<'a>(words: &[&'a Word]) -> Vec<Vec<&'a Word>> {
    let mut sorted: Vec<&Word> = words.to_vec();
    sorted.sort_by(|a, b| a.bbox[1].total_cmp(&b.bbox[1]));
    let mut out: Vec<Vec<&Word>> = Vec::new();
    for w in sorted {
        let line = out.iter_mut().find(|l| l.iter().any(|o| same_line(o, w)));
        match line {
            Some(l) => l.push(w),
            None => out.push(vec![w]),
        }
    }
    for l in &mut out {
        l.sort_by(|a, b| a.bbox[0].total_cmp(&b.bbox[0]));
    }
    out
}

fn same_line(a: &Word, b: &Word) -> bool {
    let overlap = a.bbox[3].min(b.bbox[3]) - a.bbox[1].max(b.bbox[1]);
    overlap > 0.5 * (a.bbox[3] - a.bbox[1]).min(b.bbox[3] - b.bbox[1])
}

fn union(boxes: impl IntoIterator<Item = [f64; 4]>) -> [f64; 4] {
    boxes.into_iter().fold(
        [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY],
        |a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, x0: f64, top: f64) -> Word {
        Word {
            text: text.into(),
            bbox: [x0, top, x0 + 6.0 * text.chars().count() as f64, top + 9.0],
            font: String::new(),
            size: 9.0,
            bold: false,
            hidden: None,
        }
    }

    fn page() -> Vec<Word> {
        vec![
            w("Hemoglobina", 56.0, 200.0),
            w("14,2", 300.0, 200.0),
            w("g/dL", 352.0, 200.0),
            w("Plaquetas", 56.0, 215.0),
            w("250.000", 300.0, 215.0),
            w("/µL", 352.0, 215.0),
            w("Glucosa", 56.0, 230.0),
            w("<0,5H", 300.0, 230.0),
            w("Anticuerpos", 56.0, 245.0),
            w("No", 300.0, 245.0),
            w("detectado", 318.0, 245.0),
        ]
    }

    fn bbox(g: &Grounding) -> [f64; 4] {
        match g {
            Grounding::Grounded { bbox } => *bbox,
            other => panic!("not grounded: {other:?}"),
        }
    }

    #[test]
    fn decimal_separators_are_tolerated() {
        let p = page();
        assert_eq!(bbox(&ground(&p, "14.2", None, None)), [300.0, 200.0, 324.0, 209.0]);
        assert_eq!(ground(&p, "14,2", None, None).code(), "grounded");
        assert_eq!(ground(&p, "250000", None, None).code(), "grounded");
        assert_eq!(ground(&p, "250", None, None).code(), "grounded");
        assert_eq!(ground(&p, "0.5", None, None).code(), "grounded");
    }

    #[test]
    fn a_value_not_on_the_page_is_ungrounded() {
        let p = page();
        assert_eq!(ground(&p, "14.3", None, None), Grounding::Ungrounded);
        assert_eq!(ground(&p, "142", None, None), Grounding::Ungrounded);
        assert_eq!(ground(&p, "1.42", None, None), Grounding::Ungrounded);
        assert_eq!(ground(&p, "Positivo", None, None), Grounding::Ungrounded);
        assert_eq!(ground(&p, "", None, None), Grounding::Ungrounded);
    }

    #[test]
    fn no_word_layer_is_its_own_verdict() {
        assert_eq!(ground(&[], "14.2", None, None), Grounding::UngroundedNoWordLayer);
        let hidden = vec![Word {
            hidden: Some("white_on_white".into()),
            ..w("14,2", 300.0, 200.0)
        }];
        assert_eq!(ground(&hidden, "14.2", None, None), Grounding::UngroundedNoWordLayer);
    }

    #[test]
    fn hidden_words_never_ground() {
        let mut p = page();
        p.push(Word {
            hidden: Some("tiny".into()),
            ..w("99,9", 400.0, 260.0)
        });
        assert_eq!(ground(&p, "99.9", None, None), Grounding::Ungrounded);
    }

    #[test]
    fn near_limits_the_search_to_the_claimed_box() {
        let p = page();
        let row = [50.0, 198.0, 380.0, 211.0];
        assert_eq!(ground(&p, "14,2", None, Some(row)).code(), "grounded");
        // 250.000 is on the page, but not in the Hemoglobina row the agent pointed at
        assert_eq!(ground(&p, "250000", None, Some(row)), Grounding::Ungrounded);
    }

    #[test]
    fn the_unit_widens_the_box_and_text_values_match_word_by_word() {
        let p = page();
        assert_eq!(
            bbox(&ground(&p, "14,2", Some("g/dL"), None)),
            [300.0, 200.0, 376.0, 209.0]
        );
        assert_eq!(
            bbox(&ground(&p, "250000", Some("/uL"), None)),
            [300.0, 215.0, 370.0, 224.0]
        );
        // a unit spelled otherwise does not unground the value
        assert_eq!(ground(&p, "14.2", Some("g/100 mL"), None).code(), "grounded");
        assert_eq!(
            bbox(&ground(&p, "no detectado", None, None)),
            [300.0, 245.0, 372.0, 254.0]
        );
    }

    #[test]
    fn numbers_reads_every_convention() {
        assert_eq!(numbers("12,3"), Some(vec![12.3]));
        assert_eq!(numbers("1.234"), Some(vec![1.234, 1234.0]));
        assert_eq!(numbers("1.234.567"), Some(vec![1_234_567.0]));
        assert_eq!(numbers("1,234.5"), Some(vec![1234.5]));
        assert_eq!(numbers("-0,4"), Some(vec![-0.4]));
        assert_eq!(numbers("abc"), None);
        assert_eq!(numbers("4.0-11.0"), None);
        assert_eq!(leading_number("<0,5H"), "0,5");
        assert_eq!(leading_number("6.1mg/dL"), "6.1");
    }
}
