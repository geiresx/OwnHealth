//! The Rust normalizer answers what the Python one (0.1.1) answers, for every input in
//! `fixtures/python-reference.json` (written from the Python code by a generator that left with the backend in 0.2.0; see git history).

use std::str::FromStr;
use std::sync::LazyLock;

use ownhealth_core::pyjson::py_decimal_str;
use ownhealth_normalize::{self as n, DeriveIn, RefRange, Resolution, ResolveQuery, tables, text};
use rust_decimal::Decimal;
use serde_json::{Value, json};

static REF: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("fixtures/python-reference.json")).unwrap_or_else(|e| panic!("fixture: {e}"))
});

fn s(v: &Value) -> Option<&str> {
    v.as_str()
}

fn dec(d: Option<Decimal>) -> Value {
    d.map_or(Value::Null, |d| Value::String(py_decimal_str(&d)))
}

fn rng(r: Option<&RefRange>) -> Value {
    match r {
        None => Value::Null,
        Some(r) => json!([
            dec(r.low),
            dec(r.high),
            r.text,
            r.applies_to,
            r.low_inclusive,
            r.high_inclusive,
            r.text_code
        ]),
    }
}

fn resolution(r: &Resolution) -> Value {
    let c: Vec<Value> = r
        .candidates
        .iter()
        .take(3)
        .map(|c| {
            json!([
                c.alias_group,
                c.loinc,
                c.method,
                c.confidence,
                c.matched,
                c.unit_ok,
                c.specimen_ok,
                c.partial
            ])
        })
        .collect();
    json!([
        r.alias_group,
        r.loinc,
        r.property,
        r.method,
        r.refused,
        r.reasons,
        r.confidence,
        c
    ])
}

fn report(section: &str, bad: &[String], total: usize) {
    assert!(total > 0, "{section}: no cases");
    assert!(
        bad.is_empty(),
        "{section}: {} of {total} differ from Python, first: {:#?}",
        bad.len(),
        &bad[..bad.len().min(8)]
    );
}

#[test]
fn derivation_version_matches_python() {
    assert_eq!(
        n::derivation_version(),
        REF["derivation_version"].as_str().unwrap_or_default()
    );
    assert_eq!(tables::tables_hash(), REF["tables_hash"].as_str().unwrap_or_default());
}

#[test]
fn fold() {
    let cases = REF["fold"].as_object().cloned().unwrap_or_default();
    let bad: Vec<String> = cases
        .iter()
        .filter(|(k, v)| tables::fold(k) != v.as_str().unwrap_or_default())
        .map(|(k, v)| format!("{k:?}: python {v} rust {:?}", tables::fold(k)))
        .collect();
    report("fold", &bad, cases.len());
}

#[test]
fn to_ucum() {
    let cases = REF["to_ucum"].as_object().cloned().unwrap_or_default();
    let bad: Vec<String> = cases
        .iter()
        .filter(|(k, v)| n::to_ucum(k).as_deref() != v.as_str())
        .map(|(k, v)| format!("{k:?}: python {v} rust {:?}", n::to_ucum(k)))
        .collect();
    report("to_ucum", &bad, cases.len());
}

#[test]
fn parse_value() {
    let cases = REF["parse_value"].as_array().cloned().unwrap_or_default();
    let mut bad = Vec::new();
    for c in &cases {
        let pv = n::parse_value(s(&c[0]).unwrap_or_default(), s(&c[1]).unwrap_or_default(), s(&c[2]))
            .unwrap_or_else(|e| panic!("{e}"));
        let got = json!([c[0], c[1], c[2], dec(pv.value), pv.comparator, pv.text_code, pv.reason]);
        if &got != c {
            bad.push(format!("python {c} rust {got}"));
        }
    }
    report("parse_value", &bad, cases.len());
    let ranges = REF["looks_like_range"].as_object().cloned().unwrap_or_default();
    let bad: Vec<String> = ranges
        .iter()
        .filter(|(k, v)| Some(text::looks_like_range(k)) != v.as_bool())
        .map(|(k, v)| format!("{k:?}: python {v}"))
        .collect();
    report("looks_like_range", &bad, ranges.len());
}

#[test]
fn parse_and_pick_range() {
    let cases = REF["parse_range"].as_array().cloned().unwrap_or_default();
    let mut bad = Vec::new();
    for c in &cases {
        let parsed = n::parse_range(s(&c[0]).unwrap_or_default(), s(&c[1]).unwrap_or_default(), s(&c[2]))
            .unwrap_or_else(|e| panic!("{e}"));
        let mut picks = Vec::new();
        for sex in [Some("male"), Some("female"), None] {
            for age in [Some(5), Some(17), Some(18), Some(40), Some(71), None] {
                picks.push(match n::pick_range(&parsed, sex, age) {
                    None => Value::Null,
                    Some(p) => json!(parsed.iter().position(|x| std::ptr::eq(x, p))),
                });
            }
        }
        let got = json!([
            c[0],
            c[1],
            c[2],
            parsed.iter().map(|r| rng(Some(r))).collect::<Vec<_>>(),
            picks
        ]);
        if &got != c {
            bad.push(format!("python {c}\n  rust {got}"));
        }
    }
    report("parse_range", &bad, cases.len());
}

#[test]
fn interpret() {
    let cases = REF["interpret"].as_array().cloned().unwrap_or_default();
    let mut bad = Vec::new();
    for c in &cases {
        let parsed = n::parse_range(s(&c[0]).unwrap_or_default(), "es", None).unwrap_or_else(|e| panic!("{e}"));
        let v = s(&c[1]).map(|v| Decimal::from_str(v).unwrap_or_else(|e| panic!("{e}")));
        let got = n::interpret(v, s(&c[2]), parsed.first(), s(&c[3]));
        if got != s(&c[4]) {
            bad.push(format!("python {c} rust {got:?}"));
        }
    }
    report("interpret", &bad, cases.len());
    let flags = REF["printed_flag_code"].as_object().cloned().unwrap_or_default();
    let bad: Vec<String> = flags
        .iter()
        .filter(|(k, v)| text::printed_flag_code(Some(k)) != v.as_str())
        .map(|(k, v)| format!("{k:?}: python {v} rust {:?}", text::printed_flag_code(Some(k))))
        .collect();
    report("printed_flag_code", &bad, flags.len());
}

#[test]
fn convert() {
    let cases = REF["convert"].as_array().cloned().unwrap_or_default();
    let mut bad = Vec::new();
    for c in &cases {
        let v = Decimal::from_str(s(&c[3]).unwrap_or_default()).unwrap_or_else(|e| panic!("{e}"));
        let got = n::convert(
            v,
            s(&c[1]).unwrap_or_default(),
            s(&c[2]).unwrap_or_default(),
            s(&c[0]).unwrap_or_default(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        if dec(got) != c[4] {
            bad.push(format!("python {c} rust {:?}", got.map(|d| py_decimal_str(&d))));
        }
    }
    report("convert", &bad, cases.len());
}

#[tokio::test]
async fn resolve_without_a_database() {
    let cases = REF["resolve"].as_array().cloned().unwrap_or_default();
    let mut bad = Vec::new();
    for c in &cases {
        let q = ResolveQuery {
            lang: s(&c[1]).unwrap_or_default(),
            unit: s(&c[2]),
            specimen: s(&c[3]),
            provider: Some("LabNorte"),
            text_result: c[4].as_bool().unwrap_or_default(),
            ..Default::default()
        };
        let r = n::resolve_analyte(None, s(&c[0]).unwrap_or_default(), &q)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let got = resolution(&r);
        if got != c[5] {
            bad.push(format!(
                "{} {} {:?} {:?}: python {}\n  rust {got}",
                c[0], c[1], c[2], c[3], c[5]
            ));
        }
    }
    report("resolve", &bad, cases.len());
}

#[tokio::test]
async fn derive_without_a_database() {
    let cases = REF["derive"].as_array().cloned().unwrap_or_default();
    let mut bad = Vec::new();
    for c in &cases {
        let (name, value, unit, range_text, flag, lang) = (
            s(&c[0]).unwrap_or_default(),
            s(&c[1]).unwrap_or_default(),
            s(&c[2]),
            s(&c[3]),
            s(&c[4]),
            s(&c[5]).unwrap_or_default(),
        );
        let pv = n::parse_value(value, lang, unit).unwrap_or_else(|e| panic!("{e}"));
        let parsed = match range_text.filter(|r| !r.is_empty()) {
            Some(r) => n::parse_range(r, lang, unit).unwrap_or_else(|e| panic!("{e}")),
            None => Vec::new(),
        };
        let range = n::pick_range(&parsed, Some("male"), Some(47));
        let (d, _, checks) = n::derive(
            None,
            &DeriveIn {
                printed_name: name,
                printed_unit: unit,
                value: pv.value,
                comparator: pv.comparator.as_deref(),
                text_code: pv.text_code.as_deref(),
                range,
                printed_flag: flag,
                lang,
                specimen: s(&c[6]),
                method: None,
                provider: Some("LabNorte"),
                loinc: s(&c[7]),
                household_id: None,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
        let derived = json!({
            "loinc": d.loinc, "alias_group": d.alias_group, "method": d.method, "unit_ucum": d.unit_ucum,
            "value_canonical": dec(d.value_canonical), "unit_canonical": d.unit_canonical,
            "interpretation": d.interpretation, "specimen": d.specimen,
        });
        let checks: Vec<Value> = checks
            .iter()
            .map(|k| json!([k.name, k.ok, k.reason, k.detail]))
            .collect();
        if derived != c[8] || json!(checks) != c[9] {
            bad.push(format!(
                "{name} {value} {unit:?}: python {} {}\n  rust {derived} {}",
                c[8],
                c[9],
                json!(checks)
            ));
        }
    }
    report("derive", &bad, cases.len());
}
