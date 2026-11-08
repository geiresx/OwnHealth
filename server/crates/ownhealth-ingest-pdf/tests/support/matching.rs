//! Match extracted rows against ground truth: row F1 and dropped rows (0.1.1's `fixtures/labslips/match.py`).

use std::collections::BTreeMap;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

pub fn norm(s: Option<&str>) -> String {
    let s: String = s
        .unwrap_or("")
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase();
    s.replace(['\u{b5}', '\u{3bc}'], "u")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub type Key = (i64, String, String);

pub fn key(page: i64, name: &str, value: &str) -> Key {
    (page, norm(Some(name)), norm(Some(value)))
}

pub fn inside(inner: &[f64], outer: &[f64]) -> bool {
    let pad = 4.0;
    let (cx, cy) = ((inner[0] + inner[2]) / 2.0, (inner[1] + inner[3]) / 2.0);
    outer[0] - pad <= cx && cx <= outer[2] + pad && outer[1] - pad <= cy && cy <= outer[3] + pad
}

#[derive(Debug, Clone)]
pub struct TruthRow {
    pub page: i64,
    pub name: String,
    pub value: String,
    pub unit: String,
    pub bbox: Vec<f64>,
    pub ucum: Option<String>,
    pub loinc: String,
    pub value_num: Option<String>,
}

impl TruthRow {
    pub fn from_json(v: &serde_json::Value) -> TruthRow {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        TruthRow {
            page: v["page"].as_i64().unwrap_or(0),
            name: s("name"),
            value: s("value"),
            unit: s("unit"),
            bbox: v["bbox"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
                .unwrap_or_default(),
            ucum: v["ucum"].as_str().map(String::from),
            loinc: s("loinc"),
            value_num: v["value_num"].as_str().map(String::from),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Extracted {
    pub page: i64,
    pub name: String,
    pub value: String,
    pub unit: String,
    pub bbox: Vec<f64>,
}

#[derive(Debug, Default)]
pub struct SlipScore {
    pub truth: usize,
    pub extracted: usize,
    pub tp: usize,
    pub unit_ok: usize,
    pub bbox_ok: usize,
    pub missing: Vec<Key>,
    pub extra: Vec<Key>,
}

/// A truth row matches on page, name and value.
pub fn score_rows(truth: &[TruthRow], extracted: &[Extracted]) -> SlipScore {
    let mut want: BTreeMap<Key, Vec<&TruthRow>> = BTreeMap::new();
    for t in truth {
        want.entry(key(t.page, &t.name, &t.value)).or_default().push(t);
    }
    let mut s = SlipScore {
        truth: truth.len(),
        extracted: extracted.len(),
        ..SlipScore::default()
    };
    for e in extracted {
        let k = key(e.page, &e.name, &e.value);
        match want.get_mut(&k).and_then(|v| v.pop()) {
            Some(t) => {
                s.tp += 1;
                if norm(Some(&e.unit)) == norm(Some(&t.unit)) {
                    s.unit_ok += 1;
                }
                if !e.bbox.is_empty() && inside(&t.bbox, &e.bbox) && inside(&e.bbox, &t.bbox) {
                    s.bbox_ok += 1;
                }
            }
            None => s.extra.push(k),
        }
    }
    s.missing = want
        .into_iter()
        .flat_map(|(k, v)| v.into_iter().map(move |_| k.clone()))
        .collect();
    s
}
