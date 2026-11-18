//! The printed next-draw request: the rendered `ownhealth.next_draw/v1` plan
//! (`ownhealth_rules::next_draw::render`, plus `person`) on A4 through the one PDF writer
//! (`ownhealth_core::pdfwrite`), so the same plan always gives the same bytes.
//!
//! One section per set of preparations, each opening with what its cited rows say; one table row per
//! analyte with its LOINC code, every reason it is listed, and the rule and src ids of each reason in
//! the same row. The cited sources list maps every rule and preparation to its URL, never cut. The footer carries
//! the rules versions, the start of the state hash and the page number.

use std::cell::Cell;
use std::rc::Rc;

use ownhealth_core::pdfwrite::{Column, Font, Writer};
use serde_json::{Map, Value};

use crate::summary_pdf::plain;

const LEFT: f64 = 40.0;
const WIDTH: f64 = 515.0;
const SIZE: f64 = 7.5;
const LEADING: f64 = 9.4;

/// The finished sheet: the file, its page count and the src ids it prints, in print order.
#[derive(Debug, Clone)]
pub struct NextDrawPdf {
    pub pdf: Vec<u8>,
    pub pages: usize,
    pub printed: Vec<String>,
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn fill(template: &str, params: &[(&str, String)]) -> String {
    params
        .iter()
        .fold(template.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), v))
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

struct Sheet<'a> {
    model: &'a Value,
    l: &'a Map<String, Value>,
    w: Writer,
    printed: Vec<String>,
    /// (rule or preparation id, cite), in the order the rows name them.
    cites: Vec<(String, String)>,
}

impl Sheet<'_> {
    fn t(&self, key: &str) -> String {
        self.l.get(key).and_then(Value::as_str).unwrap_or(key).to_string()
    }

    fn line(&mut self, s: &str) {
        self.w.paragraph(&plain(s), Font::Regular, SIZE, LEADING);
    }

    fn cite(&mut self, id: &str, cite: &str) {
        let pair = (id.to_string(), cite.to_string());
        if !cite.is_empty() && !self.cites.contains(&pair) {
            self.cites.push(pair);
        }
    }

    fn heading(&mut self, title: &str) {
        self.w.ensure(LEADING * 4.0);
        self.w.y -= 5.0;
        self.w.paragraph(&plain(title), Font::Bold, 9.0, 11.0);
        self.w.y -= 1.0;
        let (l, r) = (self.w.left, self.w.right);
        self.w.rule(l, r, 0.4);
        self.w.y -= 3.0;
    }

    fn table(&mut self, widths: &[f64], header: &[&str], rows: Vec<Vec<String>>) {
        let head: Vec<String> = header.iter().map(|k| self.t(k)).collect();
        let head: Vec<&str> = head.iter().map(String::as_str).collect();
        let rows: Vec<Vec<String>> = rows.into_iter().map(|r| r.iter().map(|c| plain(c)).collect()).collect();
        self.w.table(&cols(widths), Some(&head), &rows, SIZE, LEADING);
    }

    fn header(&mut self) {
        let title = self.t("title");
        self.w.paragraph(&title, Font::Bold, 13.0, 16.0);
        let p = &self.model["person"];
        let who: Vec<&str> = ["name", "sex", "age", "born"]
            .iter()
            .map(|k| st(p, k))
            .filter(|s| !s.is_empty())
            .collect();
        if !who.is_empty() {
            self.w.paragraph(&plain(&who.join(" · ")), Font::Bold, 9.0, 12.0);
        }
        let as_of = self.t("as_of");
        self.line(&as_of);
        let notice = self.t("notice");
        self.line(&notice);
    }

    fn section(&mut self, sec: &Value) {
        self.heading(st(sec, "title"));
        // the heading names a single preparation already; several each name their own
        let notes = arr(sec, "notes");
        for n in notes {
            self.cite(st(n, "id"), st(n, "cite"));
            let text = format!("{} ({})", st(n, "text"), st(n, "source"));
            if notes.len() > 1 {
                self.line(&format!("{}: {text}", st(n, "name")));
            } else {
                self.line(&text);
            }
        }
        if !notes.is_empty() {
            self.w.y -= 2.0;
        }
        let mut rows = Vec::new();
        for it in arr(sec, "items") {
            let mut why = Vec::new();
            let mut basis = Vec::new();
            for r in arr(it, "reasons") {
                let rule = st(r, "rule");
                self.cite(rule, st(r, "cite"));
                let ids: Vec<String> = arr(r, "src")
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                for id in &ids {
                    if !self.printed.contains(id) {
                        self.printed.push(id.clone());
                    }
                }
                why.push(st(r, "text").to_string());
                basis.push(
                    std::iter::once(rule.to_string())
                        .chain(ids)
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
            rows.push(vec![
                st(it, "name").to_string(),
                st(it, "loinc").to_string(),
                why.join("\n"),
                basis.join("\n"),
            ]);
        }
        self.table(
            &[95.0, 50.0, 245.0, 125.0],
            &["col.test", "col.loinc", "col.reason", "col.basis"],
            rows,
        );
    }

    fn sources(&mut self) {
        if self.cites.is_empty() {
            return;
        }
        let title = self.t("sources");
        self.heading(&title);
        // a URL is never cut: when it does not fit beside its id it takes the next line whole
        for (id, c) in self.cites.clone() {
            self.line(&format!("{id}: {c}"));
        }
    }
}

fn draw(model: &Value, total: usize) -> (Vec<u8>, usize, Vec<String>) {
    let empty = Map::new();
    let l = model.get("labels").and_then(Value::as_object).unwrap_or(&empty);
    let hash = st(model, "state_hash");
    let hash = hash.split_once(':').map_or(hash, |(_, h)| h);
    let rules: Vec<&str> = arr(model, "rules").iter().filter_map(Value::as_str).collect();
    let footer = fill(
        l.get("footer").and_then(Value::as_str).unwrap_or_default(),
        &[("rules", rules.join(" · ")), ("hash", hash.chars().take(12).collect())],
    );
    let pages = Rc::new(Cell::new(0));
    let count = pages.clone();
    let mut w = Writer::new().on_new_page(move |w, n| {
        count.set(n);
        let f = fill(&footer, &[("page", n.to_string()), ("pages", total.max(n).to_string())]);
        w.text(LEFT, 24.0, &f, Font::Regular, 6.5);
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
        cites: Vec::new(),
    };
    sheet.header();
    let sections = arr(model, "sections");
    if sections.iter().all(|s| arr(s, "items").is_empty()) {
        sheet.w.y -= 6.0;
        let text = sheet.t("empty");
        sheet.line(&text);
    }
    for sec in sections {
        sheet.section(sec);
    }
    sheet.sources();
    let printed = std::mem::take(&mut sheet.printed);
    let out = sheet.w.finish();
    (out.pdf, pages.get().max(1), printed)
}

/// The request as a PDF, byte-stable for the same model.
pub fn render(model: &Value) -> NextDrawPdf {
    // the footer's "page x/n" needs n: draw once to count, then again with it (the layout is the same)
    let (_, pages, _) = draw(model, 0);
    let (pdf, pages, printed) = draw(model, pages);
    NextDrawPdf { pdf, pages, printed }
}
