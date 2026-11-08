//! Lab table extraction from a page's word layer (`ingest/pdf/tables.py`, ingestion.md 7).
//!
//! One engine, two configurations: a vendor template (`templates`, prior 1.0) pins the header words,
//! locale, flag tokens and stop lines of one provider; the generic rules (prior 0.95) use the header
//! vocabulary of es/en/de/fr. Both find a header line with name and value columns, cut the page into
//! column ranges from the header word positions, group words into lines by vertical overlap, and turn
//! each line below the header into a result row, a pending ("ordered but missing") line, a section
//! title, or a continuation of the row above (wrapped names, two-line ranges). A line inside the table
//! that does not read cleanly still becomes a low-confidence row for review: nothing is dropped.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::classify::fold;
use crate::py::{az, has_bounded, py_isupper, py_round, py_round3, strip, strip_chars};
use crate::textlayer::{PWord, PageModel, group_lines, union};

pub const REQUIRED: [&str; 3] = ["name", "value", "unit"];

pub const GENERIC_HEADERS: [(&str, &[&str]); 5] = [
    (
        "name",
        &[
            "prueba",
            "pruebas",
            "analisis",
            "determinacion",
            "parametro",
            "magnitud",
            "test",
            "tests",
            "analyte",
            "investigation",
            "parameter",
            "examination",
            "untersuchung",
            "analyt",
            "kenngrosse",
            "analyse",
            "analyses",
            "examen",
            "parametre",
        ],
    ),
    (
        "value",
        &[
            "resultado",
            "resultados",
            "result",
            "results",
            "value",
            "ergebnis",
            "wert",
            "messwert",
            "resultat",
            "valeur",
        ],
    ),
    (
        "unit",
        &[
            "unidades",
            "unidad",
            "uds",
            "units",
            "unit",
            "einheit",
            "einheiten",
            "unite",
            "unites",
        ],
    ),
    (
        "ref_range",
        &[
            "valores de referencia",
            "intervalo de referencia",
            "rango de referencia",
            "v. referencia",
            "referencia",
            "reference range",
            "reference interval",
            "ref. range",
            "reference",
            "range",
            "referenzbereich",
            "normbereich",
            "referenzwerte",
            "referenz",
            "valeurs de reference",
            "valeurs usuelles",
            "intervalle de reference",
            "valeurs normales",
            "normes",
        ],
    ),
    (
        "flag",
        &[
            "indicador",
            "marca",
            "flag",
            "flags",
            "kennz.",
            "kennzeichen",
            "bewertung",
            "indicateur",
            "alerte",
        ],
    ),
];

pub const FLAG_TOKENS: [&str; 21] = [
    "H", "L", "HH", "LL", "A", "*", "**", "+", "-", "++", "--", "↑", "↓", "(H)", "(L)", "▲", "▼", "h", "l", "(+)",
    "(-)",
];

const TEXT_RESULTS: [&str; 28] = [
    "negativo",
    "negativa",
    "positivo",
    "positiva",
    "no detectado",
    "no detectable",
    "indetectable",
    "trazas",
    "ausencia",
    "presencia",
    "normal",
    "negative",
    "positive",
    "not detected",
    "undetectable",
    "trace",
    "absent",
    "present",
    "negativ",
    "positiv",
    "nicht nachweisbar",
    "spuren",
    "negatif",
    "positif",
    "non detecte",
    "traces",
    "absence",
    "presence",
];
const PENDING: [&str; 12] = [
    "pendiente",
    "en curso",
    "en proceso",
    "pending",
    "to follow",
    "in progress",
    "folgt",
    "ausstehend",
    "in arbeit",
    "en cours",
    "a suivre",
    "resultat a suivre",
];
pub const SKIP_PREFIXES: [&str; 11] = [
    "comentario",
    "nota:",
    "observaciones",
    "comment",
    "note:",
    "remark",
    "hinweis",
    "bemerkung",
    "kommentar",
    "remarque",
    "commentaire",
];
pub const STOP_PREFIXES: [&str; 12] = [
    "documento sintetico",
    "validado por",
    "fin del informe",
    "synthetic document",
    "validated by",
    "end of report",
    "synthetisches dokument",
    "validiert",
    "ende des befundes",
    "document synthetique",
    "valide par",
    "fin du compte rendu",
];
const SPECIMENS: [(&str, &[&str]); 4] = [
    ("urine", &["orina", "urine", "urin", "urines", "harn"]),
    (
        "blood",
        &[
            "sangre",
            "blood",
            "blut",
            "sang",
            "hemograma",
            "haematology",
            "hematology",
            "blutbild",
            "hematologie",
        ],
    ),
    (
        "serum",
        &[
            "suero",
            "serum",
            "bioquimica",
            "biochemistry",
            "chemistry",
            "klinische chemie",
            "biochimie",
        ],
    ),
    ("stool", &["heces", "stool", "faeces", "stuhl", "selles"]),
];

fn rx(p: &str) -> Regex {
    Regex::new(p).unwrap_or_else(|e| panic!("bad built-in pattern {p}: {e}"))
}

static VALUE_RE: LazyLock<Regex> = LazyLock::new(|| rx(r"^[<>≤≥=]{0,2}\s*[-+]?\d"));
static NUMBER_RE: LazyLock<Regex> = LazyLock::new(|| rx(r"[-+]?\d+(?:[.,']\d+)*"));
static NUMBER_FULL: LazyLock<Regex> = LazyLock::new(|| rx(r"^(?:[-+]?\d+(?:[.,']\d+)*)$"));
static GLUED_FLAG: LazyLock<Regex> = LazyLock::new(|| rx(r"^([<>≤≥]?[-+]?\d[\d.,]*)(\*{1,2}|H|L)$"));
static THREE_DIGITS: LazyLock<Regex> = LazyLock::new(|| rx(r"^\d{3}$"));

const COL_MARGIN: f64 = 6.0;
const STRADDLE_PT: f64 = 2.0;
/// A flag token this far right of the range text sits in its own (unheaded) column.
const FLAG_GAP: f64 = 12.0;
/// A gap under this share of the line height joins a line to its neighbour.
const TIGHT: f64 = 0.45;

#[derive(Debug, Clone, PartialEq)]
pub struct TableSpec {
    /// "template:es/labnorte" or "rules:generic-table"
    pub extractor: String,
    pub version: String,
    pub prior: f64,
    /// column kind -> folded header phrases, in the order given
    pub headers: Vec<(String, Vec<String>)>,
    pub flag_tokens: BTreeSet<String>,
    pub stop: Vec<String>,
    pub skip: Vec<String>,
    pub locale: Option<String>,
}

impl TableSpec {
    pub fn header(&self, kind: &str) -> &[String] {
        self.headers
            .iter()
            .find(|(k, _)| k == kind)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[])
    }

    fn is_flag(&self, t: &str) -> bool {
        self.flag_tokens.contains(t)
    }
}

pub fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

pub static GENERIC: LazyLock<TableSpec> = LazyLock::new(|| TableSpec {
    extractor: "rules:generic-table".into(),
    version: "1".into(),
    prior: 0.95,
    headers: GENERIC_HEADERS
        .iter()
        .map(|(k, v)| (k.to_string(), strings(v)))
        .collect(),
    flag_tokens: FLAG_TOKENS.iter().map(|s| s.to_string()).collect(),
    stop: strings(&STOP_PREFIXES),
    skip: strings(&SKIP_PREFIXES),
    locale: None,
});

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub text: String,
    pub bbox: Option<[f64; 4]>,
    pub conf: f64,
    pub bold: bool,
}

impl Cell {
    fn new(text: String, bbox: Option<[f64; 4]>, conf: f64) -> Cell {
        Cell {
            text,
            bbox,
            conf,
            bold: false,
        }
    }

    pub fn as_dict(&self) -> Value {
        let mut d = json!({"text": self.text, "bbox": self.bbox, "conf": py_round3(self.conf)});
        if self.bold {
            d["bold"] = json!(true);
        }
        d
    }
}

/// The cells of a row, in Python's dict insertion order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cells(pub Vec<(String, Cell)>);

impl Cells {
    pub fn get(&self, kind: &str) -> Option<&Cell> {
        self.0.iter().find(|(k, _)| k == kind).map(|(_, c)| c)
    }

    pub fn set(&mut self, kind: &str, cell: Cell) {
        match self.0.iter_mut().find(|(k, _)| k == kind) {
            Some((_, c)) => *c = cell,
            None => self.0.push((kind.to_string(), cell)),
        }
    }

    pub fn as_dict(&self) -> Value {
        Value::Object(self.0.iter().map(|(k, c)| (k.clone(), c.as_dict())).collect())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub page_no: i32,
    /// result | pending
    pub kind: String,
    pub cells: Cells,
    pub words: Vec<PWord>,
    pub section: Option<String>,
    pub specimen: Option<String>,
    pub notes: Vec<String>,
}

impl Row {
    pub fn bbox(&self) -> [f64; 4] {
        let boxes: Vec<[f64; 4]> = self.words.iter().map(PWord::bbox).collect();
        union(&boxes)
    }

    pub fn text(&self, kind: &str) -> &str {
        self.cells.get(kind).map(|c| c.text.as_str()).unwrap_or("")
    }

    pub fn conf(&self, prior: f64) -> f64 {
        let m = REQUIRED
            .iter()
            .map(|k| self.cells.get(k).map_or(1.0, |c| c.conf))
            .fold(f64::INFINITY, f64::min);
        py_round3(m * prior)
    }
}

/// Column kind -> x0 for one table region, in insertion order.
pub type Region = Vec<(String, f64)>;

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub top: f64,
    pub bottom: f64,
    pub regions: Vec<Region>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageResult {
    pub rows: Vec<Row>,
    pub headers: usize,
    pub lines_in_tables: usize,
}

fn region_get(r: &Region, kind: &str) -> Option<f64> {
    r.iter().find(|(k, _)| k == kind).map(|(_, x)| *x)
}

fn boxes(words: &[PWord]) -> [f64; 4] {
    let b: Vec<[f64; 4]> = words.iter().map(PWord::bbox).collect();
    union(&b)
}

// --------------------------------------------------------------- headers ----

/// (start, end) word index spans where a header phrase appears, longest phrases first, no overlap.
fn phrase_hits(words: &[PWord], phrases: &[String]) -> Vec<(usize, usize)> {
    let folded: Vec<String> = words
        .iter()
        .map(|w| strip_chars(&fold(&w.text), ":").to_string())
        .collect();
    let mut sorted: Vec<&String> = phrases.iter().collect();
    sorted.sort_by_key(|p| std::cmp::Reverse(p.split_whitespace().count()));
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for phrase in sorted {
        let toks: Vec<&str> = phrase.split_whitespace().collect();
        let n = toks.len();
        if n == 0 || n > folded.len() {
            continue;
        }
        for i in 0..=(folded.len() - n) {
            if folded[i..i + n].iter().map(String::as_str).eq(toks.iter().copied())
                && !spans.iter().any(|(a, b)| i < *b && *a < i + n)
            {
                spans.push((i, i + n));
            }
        }
    }
    spans.sort();
    spans
}

pub fn find_headers(lines: &[Vec<PWord>], spec: &TableSpec) -> Vec<(usize, Header)> {
    let mut found = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let mut hits: Vec<(f64, &str)> = Vec::new();
        let mut used: BTreeSet<usize> = BTreeSet::new();
        for kind in ["ref_range", "name", "value", "unit", "flag"] {
            // multiword kinds first
            for (a, b) in phrase_hits(line, spec.header(kind)) {
                if (a..b).any(|i| used.contains(&i)) {
                    continue;
                }
                used.extend(a..b);
                hits.push((line[a].x0, kind));
            }
        }
        let kinds: Vec<&str> = hits.iter().map(|(_, k)| *k).collect();
        if !kinds.contains(&"name") || !kinds.contains(&"value") {
            continue;
        }
        // every word of a header line is a header word (a result row never is)
        if used.len() < line.len() {
            continue;
        }
        hits.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
        let mut regions: Vec<Region> = Vec::new();
        for (x, kind) in hits {
            let new = kind == "name" || regions.last().is_none_or(|r| region_get(r, kind).is_some());
            if new {
                regions.push(Vec::new());
            }
            if let Some(r) = regions.last_mut() {
                match r.iter_mut().find(|(k, _)| k == kind) {
                    Some((_, v)) => *v = x,
                    None => r.push((kind.to_string(), x)),
                }
            }
        }
        regions.retain(|r| region_get(r, "name").is_some() && region_get(r, "value").is_some());
        if !regions.is_empty() {
            let top = line.iter().map(|w| w.top).fold(f64::INFINITY, f64::min);
            let bottom = line.iter().map(|w| w.bottom).fold(f64::NEG_INFINITY, f64::max);
            found.push((li, Header { top, bottom, regions }));
        }
    }
    found
}

// --------------------------------------------------------------- columns ----

/// Words per column kind, kinds in the order a word first landed there.
type Parts = Vec<(String, Vec<PWord>)>;

fn part<'a>(parts: &'a Parts, kind: &str) -> &'a [PWord] {
    parts
        .iter()
        .find(|(k, _)| k == kind)
        .map(|(_, v)| v.as_slice())
        .unwrap_or(&[])
}

fn has_part(parts: &Parts, kind: &str) -> bool {
    parts.iter().any(|(k, _)| k == kind)
}

fn sorted_cols(cols: &Region) -> Region {
    let mut order = cols.clone();
    order.sort_by(|a, b| a.1.total_cmp(&b.1));
    order
}

fn assign(words: &[PWord], cols: &Region) -> Parts {
    let order = sorted_cols(cols);
    let mut out: Parts = Vec::new();
    for w in words {
        let mut kind = order[0].0.as_str();
        for (k, x) in &order {
            if w.x0 >= x - COL_MARGIN {
                kind = k;
            }
        }
        match out.iter_mut().find(|(k, _)| k == kind) {
            Some((_, v)) => v.push(w.clone()),
            None => out.push((kind.to_string(), vec![w.clone()])),
        }
    }
    out
}

fn straddles(words: &[PWord], kind: &str, cols: &Region) -> bool {
    let Some(me) = region_get(cols, kind) else { return false };
    let next = cols
        .iter()
        .map(|(_, x)| *x)
        .filter(|x| *x > me)
        .fold(f64::INFINITY, f64::min);
    next.is_finite() && words.iter().any(|w| w.x1 > next - COL_MARGIN + STRADDLE_PT)
}

pub fn text_of(words: &[PWord]) -> String {
    let mut ws: Vec<&PWord> = words.iter().collect();
    ws.sort_by(|a, b| {
        py_round(a.top, 0)
            .total_cmp(&py_round(b.top, 0))
            .then(a.x0.total_cmp(&b.x0))
    });
    ws.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

pub fn is_value(text: &str) -> bool {
    let f = fold(text);
    let f = strip(&f);
    VALUE_RE.is_match(f) || TEXT_RESULTS.iter().any(|t| f == *t || f.starts_with(&format!("{t} ")))
}

pub fn is_pending(text: &str) -> bool {
    let f = fold(text);
    let f = strip_chars(&f, " .()");
    PENDING.iter().any(|p| f == *p || f.starts_with(p))
}

const COMPARATORS: [&str; 6] = ["<", ">", "≤", "≥", "<=", ">="];

/// Split a value cell into (value words, unit words, flag words, spilled name words).
#[allow(clippy::type_complexity)]
fn split_value(words: &[PWord], spec: &TableSpec) -> (Vec<PWord>, Vec<PWord>, Vec<PWord>, Vec<PWord>) {
    let mut ws: Vec<PWord> = words.to_vec();
    ws.sort_by(|a, b| a.x0.total_cmp(&b.x0));
    let mut flags: Vec<PWord> = Vec::new();
    while ws.len() > 1 && spec.is_flag(&ws[0].text) {
        flags.push(ws.remove(0));
    }
    while ws.len() > 1 && ws.last().is_some_and(|w| spec.is_flag(&w.text)) {
        if let Some(w) = ws.pop() {
            flags.insert(0, w);
        }
    }
    // a word before the number that is not part of a value spilled over from the name column
    let mut lead: Vec<PWord> = Vec::new();
    while ws.len() > 1 && !is_value(&ws[0].text) && is_value(&text_of(&ws[1..])) {
        lead.push(ws.remove(0));
    }
    let mut units: Vec<PWord> = Vec::new();
    if !ws.is_empty() && VALUE_RE.is_match(&fold(&ws[0].text)) {
        // comparator split from its number: "<" "0,5"
        let mut head = 1;
        if COMPARATORS.contains(&ws[0].text.as_str()) && ws.len() > 1 {
            head = 2;
        }
        // a flag glued to the number: "6,2*" or "6,2H"
        let w = ws[head - 1].clone();
        if let Some(m) = GLUED_FLAG.captures(&w.text) {
            let mut num = w.clone();
            num.text = m[1].to_string();
            let mut flag = w.clone();
            flag.text = m[2].to_string();
            flag.x0 = w.x1;
            ws[head - 1] = num;
            flags.push(flag);
        }
        // digit groups split by a space: "250 000" (kept together, and ambiguous enough for review)
        while head < ws.len() && THREE_DIGITS.is_match(&ws[head].text) {
            head += 1;
        }
        let rest = &ws[head..];
        if !rest.is_empty() && !rest.iter().all(|w| NUMBER_FULL.is_match(&w.text)) {
            units = rest.to_vec();
            ws.truncate(head);
        }
    }
    (ws, units, flags, lead)
}

fn section_like(words: &[PWord]) -> bool {
    let letters: String = words
        .iter()
        .flat_map(|w| w.text.chars())
        .filter(|c| c.is_alphabetic())
        .collect();
    (letters.chars().count() >= 4 && py_isupper(&letters)) || words.iter().all(|w| w.bold)
}

fn specimen_of(section: Option<&str>) -> Option<String> {
    let f = fold(section.filter(|s| !s.is_empty())?);
    SPECIMENS
        .iter()
        .find(|(_, words)| words.iter().any(|w| has_bounded(&f, w, az)))
        .map(|(s, _)| s.to_string())
}

fn min_top(ws: &[PWord]) -> f64 {
    ws.iter().map(|w| w.top).fold(f64::INFINITY, f64::min)
}

fn max_bottom(ws: &[PWord]) -> f64 {
    ws.iter().map(|w| w.bottom).fold(f64::NEG_INFINITY, f64::max)
}

// ------------------------------------------------------------------ rows ----

pub fn extract_page(page: &PageModel, spec: &TableSpec) -> PageResult {
    let lines = group_lines(&page.visible(), 0.5);
    let headers = find_headers(&lines, spec);
    let mut rows = Vec::new();
    let mut in_tables = 0;
    for (hi, (li, header)) in headers.iter().enumerate() {
        let end = headers.get(hi + 1).map_or(lines.len(), |(l, _)| *l);
        let body = &lines[li + 1..end];
        let starts: Vec<f64> = header
            .regions
            .iter()
            .map(|r| r.iter().map(|(_, x)| *x).fold(f64::INFINITY, f64::min))
            .collect();
        for (ri, cols) in header.regions.iter().enumerate() {
            let x_lo = if ri > 0 { starts[ri] - COL_MARGIN } else { -1e9 };
            let x_hi = starts.get(ri + 1).map_or(1e9, |s| s - COL_MARGIN);
            let segs: Vec<Vec<PWord>> = body
                .iter()
                .map(|line| {
                    line.iter()
                        .filter(|w| x_lo <= w.x0 && w.x0 < x_hi)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .filter(|s| !s.is_empty())
                .collect();
            let (region_rows, n) = rows_in_region(&segs, cols, spec, page.page_no);
            rows.extend(region_rows);
            in_tables += n;
        }
    }
    PageResult {
        rows,
        headers: headers.len(),
        lines_in_tables: in_tables,
    }
}

fn rows_in_region(segs: &[Vec<PWord>], cols: &Region, spec: &TableSpec, page_no: i32) -> (Vec<Row>, usize) {
    let mut items: Vec<(&str, Vec<PWord>, Parts)> = Vec::new();
    let mut last_bottom: Option<f64> = None;
    let mut pitch: Option<f64> = None;
    let mut after_gap = false;
    for seg in segs {
        let folded = fold(&text_of(seg));
        let txt = strip(&folded);
        if spec.stop.iter().any(|s| txt.starts_with(s.as_str())) {
            break;
        }
        let top = min_top(seg);
        if let (Some(lb), Some(p)) = (last_bottom, pitch)
            && p != 0.0
            && top - lb > 4.0 * p
        {
            // a wide gap: maybe a footer, maybe more of the table. From here on only lines that read as
            // clean results count, so a footer adds no noise and a late row is still not lost.
            after_gap = true;
        }
        if spec.skip.iter().any(|s| txt.starts_with(s.as_str())) {
            items.push(("note", seg.clone(), Vec::new()));
            last_bottom = Some(max_bottom(seg));
            continue;
        }
        let parts = assign(seg, cols);
        let value_text = text_of(part(&parts, "value"));
        let has_name = !part(&parts, "name").is_empty();
        let others: Vec<&str> = parts.iter().map(|(k, _)| k.as_str()).filter(|k| *k != "name").collect();
        let kind = if has_name && !value_text.is_empty() && is_pending(&value_text) {
            "pending"
        } else if has_name && !value_text.is_empty() {
            "result"
        } else if has_name && others.is_empty() {
            "name_only"
        } else if !has_name && others.iter().all(|k| *k == "ref_range" || *k == "unit") {
            "range_only"
        } else if !has_name && !value_text.is_empty() {
            // the name sits on the line above (wrapped name, value on the second line)
            "value_only"
        } else {
            "orphan"
        };
        let clean = kind == "pending"
            || (kind == "result"
                && is_value(&value_text)
                && (has_part(&parts, "unit") || has_part(&parts, "ref_range")));
        if after_gap && !clean {
            continue;
        }
        let h = max_bottom(seg) - top;
        if let Some(lb) = last_bottom
            && kind == "result"
        {
            let gap = top - lb;
            pitch = Some(pitch.map_or(gap + h, |p| p.min(gap + h)));
        }
        items.push((kind, seg.clone(), parts));
        last_bottom = Some(max_bottom(seg));
    }

    let mut rows: Vec<Row> = Vec::new();
    let mut section: Option<String> = None;
    let mut pending_prefix: Vec<PWord> = Vec::new();
    for i in 0..items.len() {
        let (kind, seg, parts) = (&items[i].0, &items[i].1, &items[i].2);
        let mut kind: &str = kind;
        let (top, bottom) = (min_top(seg), max_bottom(seg));
        let h = if bottom - top == 0.0 { 1.0 } else { bottom - top };
        let prev = if i > 0 { items.get(i - 1) } else { None };
        let nxt = items.get(i + 1);
        let tight_above = prev.is_some_and(|p| top - max_bottom(&p.1) < TIGHT * h);
        let tight_below = nxt.is_some_and(|n| min_top(&n.1) - bottom < TIGHT * h);
        let prev_kind = prev.map(|p| p.0);
        if kind == "note" {
            if let Some(r) = rows.last_mut() {
                r.notes.push(text_of(seg));
            }
            continue;
        }
        if kind == "name_only" {
            if section_like(seg) && !(tight_above && matches!(prev_kind, Some("result" | "pending"))) {
                section = Some(text_of(seg));
            } else if tight_below && nxt.is_some_and(|n| n.0 == "value_only") {
                pending_prefix = seg.clone();
            } else if tight_above && !rows.is_empty() && matches!(prev_kind, Some("result" | "pending" | "value_only"))
            {
                if let Some(r) = rows.last_mut() {
                    r.words.extend(seg.iter().cloned());
                    let old = r
                        .cells
                        .get("name")
                        .cloned()
                        .unwrap_or_else(|| Cell::new(String::new(), None, 0.0));
                    let mut parts_b: Vec<[f64; 4]> = old.bbox.into_iter().collect();
                    parts_b.push(boxes(seg));
                    r.cells.set(
                        "name",
                        Cell::new(
                            format!("{} {}", old.text, text_of(seg)),
                            Some(union(&parts_b)),
                            old.conf.min(0.98),
                        ),
                    );
                }
            } else {
                section = Some(text_of(seg));
            }
            continue;
        }
        if kind == "range_only" {
            if !rows.is_empty() && tight_above {
                if let Some(r) = rows.last_mut() {
                    let mut rng: Vec<PWord> = part(parts, "ref_range").to_vec();
                    rng.extend(part(parts, "unit").iter().cloned());
                    r.words.extend(seg.iter().cloned());
                    let old = r.cells.get("ref_range").cloned();
                    let mut parts_b: Vec<[f64; 4]> = old.as_ref().and_then(|o| o.bbox).into_iter().collect();
                    parts_b.push(boxes(&rng));
                    let text = match &old {
                        Some(o) => format!("{} {}", o.text, text_of(&rng)),
                        None => text_of(&rng),
                    };
                    r.cells.set(
                        "ref_range",
                        Cell::new(text, Some(union(&parts_b)), old.map_or(1.0, |o| o.conf)),
                    );
                }
                continue;
            }
            kind = "orphan";
        }
        if kind == "value_only" {
            let value_text = text_of(part(parts, "value"));
            kind = if pending_prefix.is_empty() {
                "orphan"
            } else if is_pending(&value_text) {
                "pending"
            } else {
                "result"
            };
        }
        let mut row = make_row(kind, seg, parts, cols, spec, page_no, &pending_prefix);
        pending_prefix.clear();
        row.specimen = specimen_of(section.as_deref());
        row.section = section.clone();
        rows.push(row);
    }
    (rows, items.len())
}

fn make_row(
    kind: &str,
    seg: &[PWord],
    parts: &Parts,
    cols: &Region,
    spec: &TableSpec,
    page_no: i32,
    prefix: &[PWord],
) -> Row {
    let mut cells = Cells::default();
    let mut words: Vec<PWord> = seg.to_vec();
    words.extend(prefix.iter().cloned());
    let value_part = part(parts, "value");
    let name_part = part(parts, "name");
    let (value_words, unit_words, mut flag_words, spilled) = split_value(value_part, spec);
    let mut name_words: Vec<PWord> = prefix.to_vec();
    name_words.extend(name_part.iter().cloned());
    name_words.extend(spilled.iter().cloned());
    if !name_words.is_empty() {
        let mut conf: f64 = if prefix.is_empty() { 1.0 } else { 0.98 };
        if !spilled.is_empty() || straddles(name_part, "name", cols) {
            conf = conf.min(0.8);
        }
        cells.set("name", Cell::new(text_of(&name_words), Some(boxes(&name_words)), conf));
    } else {
        cells.set("name", Cell::new(String::new(), None, 0.0));
    }
    let vtext = text_of(&value_words);
    let mut vconf: f64 = if kind == "pending" {
        1.0
    } else if value_words.is_empty() {
        0.0
    } else if !is_value(&vtext) {
        0.5
    } else if value_words.iter().filter(|w| NUMBER_RE.is_match(&w.text)).count() > 1 {
        0.6 // two numbers in one value cell: ambiguous
    } else {
        1.0
    };
    if !value_words.is_empty() && straddles(value_part, "value", cols) {
        vconf = vconf.min(0.8);
    }
    if kind == "orphan" {
        vconf = vconf.min(0.5);
    }
    let name_all_bold = if name_part.is_empty() {
        false
    } else {
        name_part.iter().all(|w| w.bold)
    };
    let mut vcell = Cell::new(
        vtext.clone(),
        (!value_words.is_empty()).then(|| boxes(&value_words)),
        vconf,
    );
    vcell.bold = !value_words.is_empty() && value_words.iter().all(|w| w.bold) && !name_all_bold;
    cells.set("value", vcell);
    let unit_col = part(parts, "unit");
    let ucell = if !unit_col.is_empty() && !unit_words.is_empty() {
        Cell::new(text_of(unit_col), Some(boxes(unit_col)), 0.7) // unit printed twice
    } else if !unit_col.is_empty() {
        Cell::new(text_of(unit_col), Some(boxes(unit_col)), 1.0)
    } else if !unit_words.is_empty() {
        Cell::new(text_of(&unit_words), Some(boxes(&unit_words)), 1.0)
    } else {
        let numeric = !value_words.is_empty() && VALUE_RE.is_match(&fold(&vtext));
        Cell::new(String::new(), None, if numeric { 0.97 } else { 1.0 })
    };
    cells.set("unit", ucell);
    let mut rng: Vec<PWord> = part(parts, "ref_range").to_vec();
    rng.sort_by(|a, b| a.x0.total_cmp(&b.x0));
    // an unheaded flag column right of the range: "70 - 110      H"
    if region_get(cols, "flag").is_none()
        && rng.len() > 1
        && spec.is_flag(&rng[rng.len() - 1].text)
        && rng[rng.len() - 1].x0 - rng[rng.len() - 2].x1 > FLAG_GAP
        && let Some(w) = rng.pop()
    {
        flag_words.push(w);
    }
    if !rng.is_empty() {
        cells.set("ref_range", Cell::new(text_of(&rng), Some(boxes(&rng)), 1.0));
    }
    let mut flags: Vec<PWord> = part(parts, "flag").to_vec();
    flags.extend(flag_words);
    if !flags.is_empty() {
        let text = flags.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        cells.set("flag", Cell::new(text, Some(boxes(&flags)), 1.0));
    }
    Row {
        page_no,
        kind: if kind == "pending" { "pending" } else { "result" }.into(),
        cells,
        words,
        section: None,
        specimen: None,
        notes: Vec::new(),
    }
}
