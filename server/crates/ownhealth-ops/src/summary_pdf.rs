//! The printed doctor summary: the `ownhealth.summary/v1` model (`ownhealth_rules::summary`) on at
//! most two A4 pages, through the one PDF writer (`ownhealth_core::pdfwrite`), so the same model
//! always gives the same bytes.
//!
//! Every table row that prints a value prints its `src` id in the same row; the sources appendix maps
//! each id to its document, page and date. When the content needs more than two pages, rows are cut
//! from the bottom of the ranking (unflagged lab rows first, then flags by rank, then vaccines, then
//! the tail of medicines, allergies, conditions and genome rows) and the cut section ends with
//! "n more in the record". The footer carries the rules versions, the
//! start of the state hash and the page number.

use std::cell::Cell;
use std::rc::Rc;

use ownhealth_core::pdfwrite::{Column, Font, PAGE_H, Rendered, Writer, encode};
use ownhealth_rules::summary::fill;
use serde_json::{Map, Value};

pub const MAX_PAGES: usize = 2;
const LEFT: f64 = 40.0;
const WIDTH: f64 = 515.0;
const SIZE: f64 = 7.5;
const LEADING: f64 = 9.4;

/// The finished sheet: the file, its page count, and how many rows were cut to fit.
#[derive(Debug, Clone)]
pub struct SummaryPdf {
    pub pdf: Vec<u8>,
    pub pages: usize,
    pub cut: usize,
    /// The src ids the printed rows cite, in print order (what the audit row names).
    pub printed: Vec<String>,
}

/// How many rows of each section print.
#[derive(Debug, Clone, Copy)]
struct Keep {
    markers: usize,
    flags: usize,
    vaccines: usize,
    medications: usize,
    allergies: usize,
    conditions: usize,
    genome: usize,
}

impl Keep {
    fn all(model: &Value) -> Keep {
        let n = |k: &str| arr(model, k).len();
        Keep {
            markers: n("markers"),
            flags: n("flags"),
            vaccines: n("vaccines"),
            medications: n("medications"),
            allergies: n("allergies"),
            conditions: n("conditions"),
            genome: n("genome"),
        }
    }

    fn rows(&self) -> usize {
        self.markers + self.flags + self.vaccines + self.medications + self.allergies + self.conditions + self.genome
    }
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// Superscript digits as `^n`: WinAnsi has no ⁹, and folding would print a different number.
fn plain(s: &str) -> String {
    const SUP: [(char, char); 11] = [
        ('⁰', '0'),
        ('¹', '1'),
        ('²', '2'),
        ('³', '3'),
        ('⁴', '4'),
        ('⁵', '5'),
        ('⁶', '6'),
        ('⁷', '7'),
        ('⁸', '8'),
        ('⁹', '9'),
        ('⁻', '-'),
    ];
    let mut out = String::with_capacity(s.len());
    let mut in_sup = false;
    for c in s.chars() {
        match SUP.iter().find(|(f, _)| *f == c) {
            // ¹ ² ³ are in WinAnsi, but a unit reads the same either way
            Some((_, d)) => {
                if !in_sup {
                    out.push('^');
                }
                out.push(*d);
                in_sup = true;
            }
            None => {
                out.push(c);
                in_sup = false;
            }
        }
    }
    out
}

/// Whether some text of the model prints a '?' in place of a character (a name in Chinese or Arabic
/// letters): the WinAnsi font has no glyph for it. The labels are the sheet's own words and print.
fn unprintable(model: &Value) -> bool {
    fn any(v: &Value) -> bool {
        match v {
            Value::String(s) => encode(&plain(s)).1.iter().any(|f| f.to.contains('?')),
            Value::Array(a) => a.iter().any(any),
            Value::Object(o) => o.values().any(any),
            _ => false,
        }
    }
    model
        .as_object()
        .is_some_and(|m| m.iter().any(|(k, v)| k != "labels" && any(v)))
}

fn join(parts: &[&str], sep: &str) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

fn srcs(v: &Value) -> Vec<String> {
    match v.get("src") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

fn cols(widths: &[f64]) -> Vec<Column> {
    let mut x = LEFT;
    widths
        .iter()
        .map(|w| {
            let c = Column::new(x, w - 6.0, Font::Regular);
            x += w;
            c
        })
        .collect()
}

/// The marker rows that print: every flagged row, then unflagged rows in table order, up to `keep`.
fn kept_markers(markers: &[Value], keep: usize) -> Vec<&Value> {
    let mut order: Vec<(usize, &Value)> = markers.iter().enumerate().collect();
    order.sort_by_key(|(i, m)| {
        (
            m["flag_rank"].as_i64().unwrap_or(i64::MAX),
            m.get("not_measured").is_some(),
            *i,
        )
    });
    let mut chosen: Vec<(usize, &Value)> = order.into_iter().take(keep).collect();
    chosen.sort_by_key(|(i, _)| *i);
    chosen.into_iter().map(|(_, m)| m).collect()
}

struct Sheet<'a> {
    model: &'a Value,
    l: &'a Map<String, Value>,
    w: Writer,
    /// src ids in the order the rows printed them, for the appendix.
    printed: Vec<String>,
}

impl Sheet<'_> {
    fn t(&self, key: &str) -> String {
        self.l.get(key).and_then(Value::as_str).unwrap_or(key).to_string()
    }

    fn cite(&mut self, ids: &[String]) -> String {
        for id in ids {
            if !self.printed.contains(id) {
                self.printed.push(id.clone());
            }
        }
        ids.join(" ")
    }

    fn heading(&mut self, key: &str) {
        let title = self.t(key);
        self.w.ensure(LEADING * 3.0);
        self.w.y -= 5.0;
        self.w.paragraph(&title, Font::Bold, 9.0, 11.0);
        self.w.y -= 1.0;
        let (l, r) = (self.w.left, self.w.right);
        self.w.rule(l, r, 0.4);
        self.w.y -= 3.0;
    }

    fn line(&mut self, s: &str) {
        self.w.paragraph(&plain(s), Font::Regular, SIZE, LEADING);
    }

    fn table(&mut self, widths: &[f64], header: &[&str], rows: Vec<Vec<String>>) {
        let head: Vec<String> = header.iter().map(|k| self.t(k)).collect();
        let head: Vec<&str> = head.iter().map(String::as_str).collect();
        let rows: Vec<Vec<String>> = rows.into_iter().map(|r| r.iter().map(|c| plain(c)).collect()).collect();
        self.w.table(&cols(widths), Some(&head), &rows, SIZE, LEADING);
    }

    fn none_or_more(&mut self, shown: usize, total: usize) {
        if total == 0 {
            let none = self.t("none");
            self.line(&none);
        } else if shown < total {
            let more = fill(&self.t("more"), &[("n", (total - shown).to_string())]);
            self.line(&more);
        }
    }

    /// A domain the caller cannot read: its sections are left out, and the header says so once.
    fn out(&self, domain: &str) -> bool {
        arr(&self.model["withheld"], "by_scope")
            .iter()
            .any(|d| d.as_str() == Some(domain))
    }

    fn header(&mut self) {
        let m = self.model;
        let title = self.t("title");
        let spec = st(&m["specialty"], "name");
        self.w.paragraph(&format!("{title} · {spec}"), Font::Bold, 13.0, 16.0);
        let p = &m["person"];
        let who = join(&[st(p, "name"), st(p, "sex"), st(p, "age"), st(p, "born")], " · ");
        self.w.paragraph(&who, Font::Bold, 9.0, 12.0);
        if unprintable(m) {
            let text = self.t("unprintable");
            self.line(&text);
        }
        let sub = fill(&self.t("subtitle"), &[("as_of", st(m, "as_of").into())]);
        self.line(&sub);
        let notice = self.t("notice");
        self.line(&notice);
        for domain in ["labs", "records", "genome"] {
            if self.out(domain) {
                let text = self.t(&format!("withheld.{domain}"));
                self.line(&text);
            }
        }
    }

    fn records(&mut self, keep: Keep) {
        let m = self.model;
        if self.out("records") {
            return;
        }
        self.heading("sec.conditions");
        {
            let all = arr(m, "conditions");
            let rows: Vec<Vec<String>> = all
                .iter()
                .take(keep.conditions)
                .map(|c| vec![st(c, "name").into(), st(c, "since").into(), self.cite(&srcs(c))])
                .collect();
            let n = rows.len();
            if n > 0 {
                self.table(&[330.0, 80.0, 105.0], &["col.condition", "col.since", "col.src"], rows);
            }
            self.none_or_more(n, all.len());
        }
        self.heading("sec.allergies");
        {
            let all = arr(m, "allergies");
            let rows: Vec<Vec<String>> = all
                .iter()
                .take(keep.allergies)
                .map(|a| {
                    vec![
                        st(a, "substance").into(),
                        st(a, "criticality").into(),
                        st(a, "reaction").into(),
                        self.cite(&srcs(a)),
                    ]
                })
                .collect();
            let n = rows.len();
            if n > 0 {
                self.table(
                    &[170.0, 70.0, 170.0, 105.0],
                    &["col.substance", "col.criticality", "col.reaction", "col.src"],
                    rows,
                );
            }
            self.none_or_more(n, all.len());
        }
        self.heading("sec.medications");
        {
            let all = arr(m, "medications");
            let rows: Vec<Vec<String>> = all
                .iter()
                .take(keep.medications)
                .map(|x| {
                    vec![
                        join(&[st(x, "name"), st(x, "strength"), st(x, "kind")], " · "),
                        st(x, "schedule").into(),
                        st(x, "prescriber").into(),
                        st(x, "since").into(),
                        self.cite(&srcs(x)),
                    ]
                })
                .collect();
            let n = rows.len();
            if n > 0 {
                self.table(
                    &[170.0, 110.0, 70.0, 60.0, 105.0],
                    &["col.medicine", "col.schedule", "col.prescriber", "col.since", "col.src"],
                    rows,
                );
            }
            self.none_or_more(n, all.len());
        }
    }

    fn genome(&mut self, keep: usize) {
        let m = self.model;
        let all = arr(m, "genome");
        let rows: Vec<Vec<String>> = all
            .iter()
            .take(keep)
            .map(|g| {
                let drugs: Vec<&str> = arr(g, "drugs").iter().filter_map(Value::as_str).collect();
                vec![
                    join(&[st(g, "title"), st(g, "note")], " · "),
                    drugs.join(", "),
                    st(g, "evidence").into(),
                    self.cite(&srcs(g)),
                ]
            })
            .collect();
        // the sheet goes to a third party: findings the sensitive gate or the minor rule hold back
        // are not counted here (the audit row keeps the counts), so the paper does not say they exist
        if self.out("genome") || all.is_empty() {
            return;
        }
        self.heading("sec.genome");
        let n = rows.len();
        if n > 0 {
            self.table(
                &[230.0, 130.0, 50.0, 105.0],
                &["col.finding", "col.drugs", "col.evidence", "col.src"],
                rows,
            );
        }
        self.none_or_more(n, all.len());
    }

    fn flags(&mut self, keep: usize) {
        let all = arr(self.model, "flags");
        // gene-drug flags print even without read:labs; with nothing to rank the section is left out
        if all.is_empty() && self.out("labs") {
            return;
        }
        self.heading("sec.flags");
        let rows: Vec<Vec<String>> = all
            .iter()
            .take(keep)
            .map(|f| vec![f["rank"].to_string(), st(f, "text").into(), self.cite(&srcs(f))])
            .collect();
        let n = rows.len();
        if n > 0 {
            self.table(&[18.0, 392.0, 105.0], &["col.rank", "col.flag", "col.src"], rows);
        }
        self.none_or_more(n, all.len());
    }

    fn markers(&mut self, keep: usize) {
        let all = arr(self.model, "markers");
        if self.out("labs") {
            return;
        }
        self.heading("sec.markers");
        let not_measured = self.t("not_measured");
        let rows: Vec<Vec<String>> = kept_markers(all, keep)
            .into_iter()
            .map(|r| {
                if r.get("not_measured").is_some() {
                    return vec![st(r, "name").into(), not_measured.clone()];
                }
                // the previous value names its unit only when the unit changed
                let prev = &r["previous"];
                let before = if prev.is_object() {
                    let ids = srcs(prev);
                    let id = self.cite(&ids);
                    let unit = Some(st(prev, "unit"))
                        .filter(|u| *u != st(r, "unit"))
                        .unwrap_or_default();
                    join(&[&join(&[st(prev, "value"), unit], " "), st(prev, "date"), &id], " · ")
                } else {
                    "-".into()
                };
                vec![
                    st(r, "name").into(),
                    join(&[st(r, "value"), st(r, "unit")], " "),
                    st(r, "range").into(),
                    st(r, "date").into(),
                    st(r, "status_text").into(),
                    self.cite(&srcs(r)),
                    before,
                ]
            })
            .collect();
        let n = rows.len();
        if n > 0 {
            self.table(
                &[80.0, 58.0, 56.0, 48.0, 62.0, 63.0, 148.0],
                &[
                    "col.test",
                    "col.value",
                    "col.range",
                    "col.date",
                    "col.status",
                    "col.src",
                    "col.previous",
                ],
                rows,
            );
        }
        self.none_or_more(n, all.len());
    }

    fn vaccines(&mut self, keep: usize) {
        let all = arr(self.model, "vaccines");
        if self.out("records") {
            return;
        }
        self.heading("sec.vaccines");
        let rows: Vec<Vec<String>> = all
            .iter()
            .take(keep)
            .map(|v| {
                let dose = v
                    .get("dose")
                    .filter(|d| !d.is_null())
                    .map(Value::to_string)
                    .unwrap_or_default();
                vec![st(v, "vaccine").into(), st(v, "date").into(), dose, self.cite(&srcs(v))]
            })
            .collect();
        let n = rows.len();
        if n > 0 {
            self.table(
                &[290.0, 80.0, 40.0, 105.0],
                &["col.vaccine", "col.date", "col.dose", "col.src"],
                rows,
            );
        }
        self.none_or_more(n, all.len());
    }

    /// The printed ids grouped by what they point at: `LabNorte · 2026-09-14 · p1: src_a src_b`,
    /// in the model's order (newest first, then provider and page).
    fn appendix(&mut self) {
        if self.printed.is_empty() {
            return;
        }
        let sources = arr(self.model, "sources");
        let known = |id: &String| sources.iter().any(|s| st(s, "src") == id);
        let listed = sources
            .iter()
            .map(|s| (st(s, "label").to_string(), st(s, "src").to_string()));
        // an id the model has no source row for still prints, unlabelled, at the end
        let unknown = self
            .printed
            .iter()
            .filter(|id| !known(id))
            .map(|id| (String::new(), id.clone()));
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for (what, id) in listed.chain(unknown) {
            if !self.printed.contains(&id) {
                continue;
            }
            match groups.iter_mut().find(|(w, _)| *w == what) {
                Some((_, ids)) => ids.push(id),
                None => groups.push((what, vec![id])),
            }
        }
        self.heading("sec.sources");
        let rows = groups
            .into_iter()
            .map(|(what, ids)| vec![what, ids.join(" ")])
            .collect();
        self.table(&[180.0, 335.0], &["col.document", "col.src"], rows);
    }
}

fn draw(model: &Value, keep: Keep, total: usize) -> (Rendered, usize, Vec<String>) {
    let empty = Map::new();
    let l = model.get("labels").and_then(Value::as_object).unwrap_or(&empty);
    let hash = st(model, "state_hash");
    let hash = hash.split_once(':').map_or(hash, |(_, h)| h);
    let footer = fill(
        l.get("footer").and_then(Value::as_str).unwrap_or_default(),
        &[
            ("rules", st(model, "rules").into()),
            ("summary", st(model, "summary_rules").into()),
            ("hash", hash.chars().take(12).collect()),
        ],
    );
    let running = format!(
        "{} · {}",
        st(&model["person"], "name"),
        l.get("title").and_then(Value::as_str).unwrap_or_default()
    );
    let pages = Rc::new(Cell::new(0));
    let count = pages.clone();
    let mut w = Writer::new().on_new_page(move |w, n| {
        count.set(n);
        let total = total.max(n);
        let f = fill(&footer, &[("page", n.to_string()), ("pages", total.to_string())]);
        w.text(LEFT, 24.0, &f, Font::Regular, 6.5);
        if n > 1 {
            w.text(LEFT, PAGE_H - 26.0, &running, Font::Regular, 6.5);
            w.y = PAGE_H - 40.0;
        }
    });
    w.left = LEFT;
    w.right = LEFT + WIDTH;
    w.top = 36.0;
    w.bottom = 40.0;
    let mut sheet = Sheet {
        model,
        l,
        w,
        printed: Vec::new(),
    };
    sheet.header();
    sheet.records(keep);
    sheet.genome(keep.genome);
    sheet.flags(keep.flags);
    sheet.markers(keep.markers);
    sheet.vaccines(keep.vaccines);
    sheet.appendix();
    let printed = std::mem::take(&mut sheet.printed);
    let out = sheet.w.finish();
    (out, pages.get().max(1), printed)
}

/// The summary on at most two pages (rows cut by rank when it would not fit), byte-stable.
pub fn render(model: &Value) -> SummaryPdf {
    let full = Keep::all(model);
    let mut keep = full;
    let mut pages = draw(model, keep, 0).1;
    while pages > MAX_PAGES {
        let unflagged = kept_markers(arr(model, "markers"), keep.markers)
            .iter()
            .any(|m| m["flag_rank"].is_null());
        if keep.markers > 0 && unflagged {
            keep.markers -= 1;
        } else if keep.flags > 0 {
            keep.flags -= 1;
            // a lab row stays only while its flag does
            let ranked = |m: &&Value| m["flag_rank"].as_i64().is_some_and(|r| r <= keep.flags as i64);
            keep.markers = arr(model, "markers").iter().filter(ranked).count();
        } else if keep.vaccines > 0 {
            keep.vaccines -= 1;
        } else if keep.markers > 0 {
            keep.markers -= 1;
        } else if keep.medications > 0 {
            keep.medications -= 1;
        } else if keep.allergies > 0 {
            keep.allergies -= 1;
        } else if keep.conditions > 0 {
            keep.conditions -= 1;
        } else if keep.genome > 0 {
            keep.genome -= 1;
        } else {
            break;
        }
        pages = draw(model, keep, 0).1;
    }
    // the footer's "page x/n" needs n: draw once more with it (the layout does not change)
    let (out, pages, printed) = draw(model, keep, pages);
    SummaryPdf {
        pdf: out.pdf,
        pages,
        cut: full.rows() - keep.rows(),
        printed,
    }
}
