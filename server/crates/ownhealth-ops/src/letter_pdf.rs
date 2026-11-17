//! The access-request letter for one document gap: one A4 page through the one PDF writer
//! (`ownhealth_core::pdfwrite`), so the same input always gives the same bytes.
//!
//! The letter names the patient, the provider and the visit day, and cites each law with its URL
//! (`data/timeline/gaps.yaml` `letters`). The identity document number and the postal address print
//! as labels over blank lines to fill in by hand: they are never an input here.

use ownhealth_core::pdfwrite::{Font, Writer, text_width};
use ownhealth_rules::gaps::{Gaps, Letter, fill, pick};

const SIZE: f64 = 10.5;
const LEADING: f64 = 15.0;

/// What one letter prints.
#[derive(Debug, Clone)]
pub struct LetterIn<'a> {
    pub lang: &'a str,
    /// The patient's name as the profile records it.
    pub name: &'a str,
    pub provider: &'a str,
    /// The visit day, YYYY-MM-DD.
    pub date: &'a str,
}

/// A label with a line to its right, to fill in by hand; with no label, a line across the page.
fn blank(w: &mut Writer, label: &str) {
    w.ensure(LEADING * 1.6);
    let y = w.y - SIZE;
    let x0 = if label.is_empty() {
        w.left
    } else {
        w.text(w.left, y, label, Font::Regular, SIZE);
        w.left + text_width(label, Font::Regular, SIZE) + 6.0
    };
    let right = w.right;
    w.y = y - 2.0;
    w.rule(x0, right, 0.5);
    w.y = y - LEADING * 0.9;
}

/// The letter as a PDF.
pub fn render(g: &Gaps, letter: &Letter, l: &LetterIn) -> Vec<u8> {
    let lang = if l.lang == "es" { "es" } else { "en" };
    let t = |k: &str| pick(&g.letter_text[k], lang);
    let p = [
        ("name", l.name.to_string()),
        ("provider", l.provider.to_string()),
        ("date", l.date.to_string()),
    ];
    let mut w = Writer::new();
    w.paragraph(&t("title"), Font::Bold, 15.0, 22.0);
    w.y -= 10.0;
    w.paragraph(&fill(&t("to"), &p), Font::Regular, SIZE, LEADING);
    w.paragraph(&fill(&t("from"), &p), Font::Regular, SIZE, LEADING);
    w.y -= 8.0;
    blank(&mut w, &t("id_number"));
    blank(&mut w, &t("address"));
    blank(&mut w, "");
    w.y -= 14.0;
    for k in ["basis", "deadline", "format"] {
        let text = if k == "basis" { pick(&letter.basis, lang) } else { t(k) };
        w.paragraph(&fill(&text, &p), Font::Regular, SIZE, LEADING);
        w.y -= 8.0;
    }
    w.y -= 18.0;
    blank(&mut w, &t("sign"));
    w.y -= 36.0;
    w.rule(w.left, w.right, 0.5);
    w.y -= 8.0;
    w.paragraph(&t("laws"), Font::Bold, 9.0, 13.0);
    for law in &letter.laws {
        w.paragraph(
            &format!("{}: {}", pick(&law.name, lang), law.cite),
            Font::Regular,
            8.5,
            12.0,
        );
    }
    w.y -= 10.0;
    w.paragraph(&fill(&t("footer"), &p), Font::Regular, 7.5, 10.0);
    w.finish().pdf
}
