//! Grounding for `propose` (ingestion.md 7.2, ROADMAP v0.2): a proposed value counts only where the
//! agent says it is. With a `bbox`, the value must sit inside that box (`ownhealth_ocr::grounding`).
//! Without one, it must sit on a line that prints one of the analyte's name words before it. The
//! same number printed under another analyte, or outside the claimed box, grounds nothing.

use ownhealth_ingest_pdf::Word;
use ownhealth_ingest_pdf::classify::fold;
use ownhealth_ocr::grounding::{Grounding, ground};

/// The box of `value` on this page's words, widened to the name words on its line; None when the
/// page does not show it where the agent claims.
pub fn ground_row(
    words: &[Word],
    name: &str,
    value: &str,
    unit: Option<&str>,
    near: Option<[f64; 4]>,
) -> Option<[f64; 4]> {
    if near.is_some() {
        return bbox_of(ground(words, value, unit, near));
    }
    let want = name_tokens(name);
    let named = |w: &&Word| w.hidden.is_none() && tokens(&w.text).iter().any(|t| want.contains(t));
    let mut best: Option<([f64; 4], usize)> = None;
    for anchor in words.iter().filter(named) {
        // the rest of the anchor's line, to its right
        let rest = [anchor.bbox[2], anchor.bbox[1], f64::INFINITY, anchor.bbox[3]];
        let Some(hit) = bbox_of(ground(words, value, unit, Some(rest))) else {
            continue;
        };
        let names: Vec<&Word> = words
            .iter()
            .filter(named)
            .filter(|w| same_line(&w.bbox, &hit) && w.bbox[2] <= hit[0])
            .collect();
        let mut seen: Vec<String> = names
            .iter()
            .flat_map(|w| tokens(&w.text))
            .filter(|t| want.contains(t))
            .collect();
        seen.sort();
        seen.dedup();
        if best.is_none_or(|(_, s)| seen.len() > s) {
            let b = names.iter().fold(hit, |b, w| {
                [
                    b[0].min(w.bbox[0]),
                    b[1].min(w.bbox[1]),
                    b[2].max(w.bbox[2]),
                    b[3].max(w.bbox[3]),
                ]
            });
            best = Some((b, seen.len()));
        }
    }
    best.map(|(b, _)| b)
}

fn bbox_of(g: Grounding) -> Option<[f64; 4]> {
    match g {
        Grounding::Grounded { bbox } => Some(bbox),
        Grounding::Ungrounded | Grounding::UngroundedNoWordLayer => None,
    }
}

fn tokens(s: &str) -> Vec<String> {
    fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// The name's words; one-letter ones only when that is all the name has (`K`, `Na`, `Hb A1c`).
fn name_tokens(name: &str) -> Vec<String> {
    let all = tokens(name);
    let long: Vec<String> = all.iter().filter(|t| t.chars().count() > 1).cloned().collect();
    if long.is_empty() { all } else { long }
}

fn same_line(a: &[f64; 4], b: &[f64; 4]) -> bool {
    a[3].min(b[3]) - a[1].max(b[1]) > 0.5 * (a[3] - a[1]).min(b[3] - b[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, x0: f64, y0: f64, x1: f64) -> Word {
        Word {
            text: text.into(),
            bbox: [x0, y0, x1, y0 + 10.0],
            font: String::new(),
            size: 9.0,
            bold: false,
            hidden: None,
        }
    }

    fn page() -> Vec<Word> {
        vec![
            w("Glucosa", 56.0, 150.0, 100.0),
            w("92", 300.0, 150.0, 312.0),
            w("mg/dL", 330.0, 150.0, 360.0),
            w("Leucocitos", 56.0, 166.0, 110.0),
            w("5,8", 300.0, 166.0, 315.0),
        ]
    }

    #[test]
    fn a_value_needs_its_name_on_the_line_before_it() {
        let p = page();
        assert_eq!(
            ground_row(&p, "Leucocitos", "5.8", None, None),
            Some([56.0, 166.0, 315.0, 176.0])
        );
        assert_eq!(ground_row(&p, "Glucosa", "5,8", None, None), None);
        assert_eq!(ground_row(&p, "Hemoglobina", "92", None, None), None);
    }

    #[test]
    fn a_claimed_box_must_hold_the_value() {
        let p = page();
        assert_eq!(
            ground_row(&p, "Leucocitos", "5,8", None, Some([50.0, 164.0, 380.0, 178.0])),
            Some([300.0, 166.0, 315.0, 176.0])
        );
        assert_eq!(
            ground_row(&p, "Leucocitos", "5,8", None, Some([50.0, 148.0, 380.0, 162.0])),
            None
        );
    }

    #[test]
    fn hidden_words_never_name_or_ground() {
        let mut p = page();
        p[3].hidden = Some("white".into());
        assert_eq!(ground_row(&p, "Leucocitos", "5,8", None, None), None);
    }
}
