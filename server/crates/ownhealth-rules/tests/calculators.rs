//! Calculators (ownhealth.calculators/v1): each formula reproduces its vectors in every accepted unit,
//! each refuses with a reason when a validity rule fails, and the document is deterministic and
//! leaves the state body alone.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use chrono::NaiveDate;
use common::{EXPECTED, as_of, install_fake_genome, seeded};
use ownhealth_rules::calc::{self, Calc, Data, Draw, Obs, Person, Sex};
use ownhealth_rules::{build_state, cited, state_hash};
use rust_decimal::Decimal;
use serde_json::{Value, json};

type R = anyhow::Result<()>;

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/calc");
const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/calculators");

fn data() -> &'static Data {
    calc::data().expect("the shipped calculator files load")
}

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("a number")
}

fn vectors(name: &str) -> Value {
    let text = std::fs::read_to_string(format!("{VECTORS}/{name}.json")).expect("vector file");
    serde_json::from_str(&text).expect("vector json")
}

/// Every case whose result, rounded to the file's precision, differs from its expected value.
fn mismatches(file: &Value) -> Vec<String> {
    let id = file["calculator"].as_str().unwrap_or_default();
    let places = u32::try_from(file["precision"].as_u64().unwrap_or(2)).unwrap_or(2);
    let mut bad = Vec::new();
    for (i, case) in file["cases"].as_array().into_iter().flatten().enumerate() {
        let printed: Vec<(String, Decimal, String)> = case["inputs"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(g, v)| {
                (
                    g.clone(),
                    d(v["value"].as_str().unwrap_or_default()),
                    v["unit"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let printed: Vec<(&str, Decimal, &str)> =
            printed.iter().map(|(g, v, u)| (g.as_str(), *v, u.as_str())).collect();
        let age = case["age"].as_i64().map(|a| a as i32);
        let sex = Sex::from_record(case["sex"].as_str());
        let got = calc::raw(data(), id, &printed, age, sex)
            .map(|v| v.map(|v| ownhealth_rules::num::quantize(v, places).to_string()));
        let want = case["expected"].as_str().unwrap_or_default();
        if got != Ok(Some(want.to_string())) {
            bad.push(format!("{id} case {i}: want {want}, got {got:?}"));
        }
    }
    bad
}

const FILES: [&str; 4] = ["ckd_epi_2021_cr", "ckd_epi_2021_cr_cys", "fib4", "homa_ir"];
/// The cardiovascular vectors (tests/cardiovascular.rs checks them); here only for unit coverage.
const CV_FILES: [&str; 4] = ["score2", "score2_op", "prevent_cvd", "prevent_ascvd"];

#[test]
fn every_calculator_reproduces_its_vectors() {
    for name in FILES {
        let file = vectors(name);
        assert!(
            file["source"].as_str().is_some_and(|s| s.len() > 20),
            "{name} names its source"
        );
        assert!(
            file["cases"].as_array().is_some_and(|c| c.len() >= 5),
            "{name} has vectors"
        );
        assert_eq!(mismatches(&file), Vec::<String>::new());
    }
}

#[test]
fn every_accepted_unit_has_a_vector() {
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for name in FILES.into_iter().chain(CV_FILES) {
        for case in vectors(name)["cases"].as_array().into_iter().flatten() {
            for (g, v) in case["inputs"].as_object().into_iter().flatten() {
                *seen
                    .entry((g.clone(), v["unit"].as_str().unwrap_or_default().to_string()))
                    .or_default() += 1;
            }
        }
    }
    for row in &data().units {
        assert!(
            seen.contains_key(&(row.group.clone(), row.unit.clone())),
            "no vector reads {} in {}",
            row.group,
            row.unit
        );
    }
}

/// The control: one wrong expected value turns the vector check red.
#[test]
fn a_wrong_vector_fails_the_suite() {
    for name in FILES {
        let mut file = vectors(name);
        let first = &mut file["cases"][0]["expected"];
        let wrong = d(first.as_str().unwrap_or_default()) + d("0.01");
        *first = json!(wrong.to_string());
        assert_eq!(mismatches(&file).len(), 1, "{name}");
    }
}

#[test]
fn a_unit_with_no_published_conversion_is_refused_not_guessed() {
    let got = calc::raw(
        data(),
        "ckd_epi_2021_cr",
        &[("creatinine", d("0.09"), "mmol/L")],
        Some(50),
        Some(Sex::Male),
    );
    assert_eq!(got, Err("creatinine in mmol/L".into()));
}

#[test]
fn the_shipped_files_pass_the_cited_loader() -> R {
    for (name, bytes) in calc::FILES {
        let path = Path::new(DATA).join(name);
        let f = cited::load(&path)?;
        assert_eq!(std::fs::read_to_string(&path)?, bytes, "{name} is the compiled-in copy");
        assert!(!f.version.is_empty());
    }
    // the loader in the binary refuses what the cited loader refuses
    let (cal, units) = (calc::FILES[0].1, calc::FILES[1].1);
    for (what, c, u, reason) in [
        (
            "http cite",
            cal.replacen("https://pubmed", "http://pubmed", 1),
            units.to_string(),
            "no https `cite`",
        ),
        (
            "unknown calculator",
            cal.replacen("id: homa_ir", "id: homa_beta", 1),
            units.to_string(),
            "homa_beta",
        ),
        (
            "missing es",
            cal.replacen(r#", es: "FIB-4""#, "", 1),
            units.to_string(),
            "no `es` text",
        ),
        (
            "advice",
            cal.replacen("Sin calcular: el registro no tiene {groups}.", "Evitar el alcohol.", 1),
            units.to_string(),
            "reads as advice",
        ),
        (
            "zero factor",
            cal.to_string(),
            units.replacen(r#"divide: "88.4""#, r#"divide: "0""#, 1),
            "divides by 0",
        ),
    ] {
        assert!(c != cal || u != units, "{what}: the mutation did not apply");
        let e = calc::parse(&c, &u).expect_err(what);
        assert!(e.0.contains(reason), "{what}: {e}");
    }
    Ok(())
}

/// A lab report's cystatin C, in any of the four languages, lands in the group the combined equation reads.
#[tokio::test]
async fn cystatin_c_resolves_in_es_en_de_fr() -> R {
    for (name, lang) in [
        ("Cistatina C", "es"),
        ("Cystatin C, serum", "en"),
        ("Cystatin C", "de"),
        ("Cystatine C", "fr"),
    ] {
        let q = ownhealth_normalize::ResolveQuery {
            lang,
            unit: Some("mg/L"),
            ..Default::default()
        };
        let r = ownhealth_normalize::resolve_analyte(None, name, &q).await?;
        assert_eq!(
            (r.alias_group.as_str(), r.loinc.as_deref()),
            ("cystatin_c", Some("33863-2")),
            "{name}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- refusals (pure) ----

fn calc_row(id: &str) -> &'static Calc {
    data().calcs.iter().find(|c| c.id == id).expect("a calculator row")
}

fn day(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("a date")
}

fn obs(group: &str, value: &str, unit: &str, date: &str, src: &str) -> Obs {
    Obs {
        group: group.into(),
        value: Some(d(value)),
        comparator: None,
        printed: value.into(),
        unit: Some(unit.into()),
        date: day(date),
        src: src.into(),
        method: None,
    }
}

fn draw(values: Vec<Obs>) -> Draw {
    let date = values.iter().map(|o| o.date).max();
    Draw {
        date,
        obs: values.into_iter().map(|o| (o.group.clone(), o)).collect(),
    }
}

fn person(birth: &str, sex: Option<Sex>) -> Person {
    Person {
        birth_date: Some(day(birth)),
        sex,
    }
}

fn one(id: &str, draws: &[Draw], p: &Person) -> Vec<Value> {
    calc::evaluate(data(), calc_row(id), draws, p, day("2026-11-07"))
}

#[test]
fn fib4_refuses_at_34_and_computes_at_35() {
    let liver = |date: &str| {
        draw(vec![
            obs("ast", "30", "U/L", date, "src_ast"),
            obs("alt", "25", "U/L", date, "src_alt"),
            obs("platelets", "220", "10*9/L", date, "src_plt"),
        ])
    };
    let p = person("1991-06-01", Some(Sex::Female));
    let items = one("fib4", &[liver("2026-05-31"), liver("2026-06-01")], &p);
    assert_eq!(items[0]["status"], "refused");
    assert_eq!(items[0]["reason_code"], "age_below_35");
    assert_eq!(items[0]["reason"]["age"], 34);
    assert!(items[0].get("value").is_none());
    assert_eq!(items[1]["status"], "ok");
    assert_eq!(items[1]["value"], "0.95");
    assert_eq!(items[1]["category"]["code"], "fib4.low");
    assert_eq!(items[1]["src"], json!(["src_ast", "src_alt", "src_plt"]));
}

#[test]
fn fib4_takes_the_lower_cutoff_of_2_from_age_65() {
    let p = person("1955-01-30", Some(Sex::Female));
    let items = one(
        "fib4",
        &[draw(vec![
            obs("ast", "35", "U/L", "2026-07-22", "a"),
            obs("alt", "28", "U/L", "2026-07-22", "b"),
            obs("platelets", "210", "10*9/L", "2026-07-22", "c"),
        ])],
        &p,
    );
    // 71 x 35 / (210 x sqrt 28) = 2.24: between 2.0 and 2.67 from 65, above 1.3 before it
    assert_eq!(items[0]["value"], "2.24");
    assert_eq!(
        items[0]["category"],
        json!({"code": "fib4.mid", "low": "2.0", "high": "2.67", "cite": "https://pubmed.ncbi.nlm.nih.gov/27725647/"})
    );
    assert_eq!(items[0]["limitations"], json!(["fib4_age_65"]));
}

#[test]
fn homa_ir_refuses_values_from_two_draws() {
    let p = person("1981-02-02", Some(Sex::Female));
    let draws = [
        draw(vec![obs("glucose", "92", "mg/dL", "2026-06-10", "src_glu")]),
        draw(vec![obs("insulin", "9.1", "u[IU]/mL", "2026-06-11", "src_ins")]),
    ];
    let items = one("homa_ir", &draws, &p);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["status"], "refused");
    assert_eq!(items[0]["reason_code"], "inputs_not_same_draw");
    assert_eq!(items[0]["src"], json!(["src_glu", "src_ins"]));
    assert!(items[0].get("value").is_none());
    // in one draw it computes, says fasting is not recorded, and names the pmol/L factor
    let same = [draw(vec![
        obs("glucose", "5.4", "mmol/L", "2026-06-10", "g"),
        obs("insulin", "60", "pmol/L", "2026-06-10", "i"),
    ])];
    let ok = &one("homa_ir", &same, &p)[0];
    assert_eq!(ok["value"], "2.40");
    assert_eq!(ok["limitations"], json!(["fasting_not_recorded", "insulin_factor"]));
    assert_eq!(ok["inputs"][1]["converted"], json!({"value": "10", "unit": "u[IU]/mL"}));
}

#[test]
fn creatinine_alone_shows_and_the_combined_equation_waits_for_cystatin_c() {
    let p = person("1979-05-14", Some(Sex::Male));
    let draws = [draw(vec![obs("creatinine", "86.6", "umol/L", "2026-09-14", "src_cr")])];
    let cr = one("ckd_epi_2021_cr", &draws, &p);
    assert_eq!(cr[0]["status"], "ok");
    assert_eq!(cr[0]["value"], "96");
    assert_eq!(cr[0]["unit"], "mL/min/{1.73_m2}");
    assert_eq!(cr[0]["person"], json!({"age": 47, "sex": "male"}));
    let both = one("ckd_epi_2021_cr_cys", &draws, &p);
    assert_eq!(both.len(), 1);
    assert_eq!(both[0]["status"], "not_assessed");
    assert_eq!(both[0]["reason_code"], "missing_input");
    assert_eq!(both[0]["reason"]["groups"], json!(["cystatin_c"]));
    assert_eq!(both[0]["inputs"][0]["src"], "src_cr");
}

#[test]
fn ckd_epi_refuses_a_child_an_unknown_sex_and_a_censored_value() {
    let draws = [draw(vec![obs("creatinine", "0.5", "mg/dL", "2026-02-18", "c")])];
    let child = one("ckd_epi_2021_cr", &draws, &person("2017-06-21", Some(Sex::Female)));
    assert_eq!(
        (child[0]["status"].clone(), child[0]["reason_code"].clone()),
        (json!("refused"), json!("not_adult"))
    );
    // the combined equation, with no cystatin C, refuses with the age at the same draw, not today's
    let both = one("ckd_epi_2021_cr_cys", &draws, &person("2017-06-21", Some(Sex::Female)));
    assert_eq!(both[0]["reason"], json!({"age": 8, "on": "2026-02-18"}));
    assert_eq!(child[0]["reason"], both[0]["reason"]);
    let nosex = one("ckd_epi_2021_cr", &draws, &person("1980-01-01", None));
    assert_eq!(nosex[0]["reason_code"], "sex_not_recorded");
    let mut censored = obs("creatinine", "0.3", "mg/dL", "2026-02-18", "c");
    censored.comparator = Some("<".into());
    let low = one(
        "ckd_epi_2021_cr",
        &[draw(vec![censored])],
        &person("1980-01-01", Some(Sex::Male)),
    );
    assert_eq!(low[0]["reason_code"], "not_a_number");
    let mmol = one(
        "ckd_epi_2021_cr",
        &[draw(vec![obs("creatinine", "0.09", "mmol/L", "2026-02-18", "c")])],
        &person("1980-01-01", Some(Sex::Male)),
    );
    assert_eq!(mmol[0]["reason_code"], "unit_unsupported");
    for items in [child, nosex, low, mmol] {
        assert!(items[0].get("value").is_none());
    }
}

#[test]
fn every_refusal_and_note_reads_in_both_languages() -> R {
    let p = person("1955-01-30", Some(Sex::Female));
    let draws = [draw(vec![
        obs("glucose", "5.4", "mmol/L", "2026-06-10", "g"),
        obs("insulin", "60", "pmol/L", "2026-06-10", "i"),
        obs("ast", "35", "U/L", "2026-06-10", "a"),
        obs("creatinine", "0.09", "mmol/L", "2026-06-10", "c"),
    ])];
    let doc = calc::build(data(), "carmen", day("2026-11-07"), &p, &draws, &Default::default())?;
    for lang in ["en", "es"] {
        let out = calc::render(&doc, lang)?;
        for it in out["items"].as_array().into_iter().flatten() {
            assert!(it["name"].as_str().is_some_and(|n| !n.is_empty()));
            if it["status"] != "ok" {
                let text = it["text"].as_str().unwrap_or_default();
                assert!(!text.is_empty() && !text.contains('{'), "{lang}: {text}");
            }
            for n in it["notes"].as_array().into_iter().flatten() {
                assert!(!n.as_str().unwrap_or("{").contains('{'), "{lang}: {n}");
            }
        }
        assert_eq!(out["calculators_hash"], doc["calculators_hash"]);
    }
    let es = calc::render(&doc, "es")?;
    let fib = es["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["id"] == "fib4")
        .cloned()
        .unwrap_or_default();
    assert_eq!(fib["text"], "Sin calcular: el registro no tiene ALT y plaquetas.");
    Ok(())
}

/// One import without a DiagnosticReport puts every value under one document: a draw that mixes
/// days is refused, never computed as if it were one specimen.
#[test]
fn inputs_from_two_days_are_never_one_draw() {
    let p = person("1955-01-30", Some(Sex::Female));
    let mixed = draw(vec![
        obs("ast", "35", "U/L", "2024-03-02", "a"),
        obs("alt", "28", "U/L", "2024-03-02", "b"),
        obs("platelets", "210", "10*9/L", "2025-11-20", "c"),
    ]);
    let items = one("fib4", &[mixed], &p);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["status"], "refused");
    assert_eq!(items[0]["reason_code"], "inputs_not_same_draw");
    assert!(items[0].get("value").is_none());
    assert_eq!(items[0]["src"], json!(["a", "b", "c"]));
}

/// Values of other calculators in the same report do not move this one's date or refuse it.
#[test]
fn a_result_is_dated_by_its_own_inputs() {
    let p = person("1979-05-14", Some(Sex::Male));
    let report = draw(vec![
        obs("creatinine", "0.98", "mg/dL", "2026-09-14", "cr"),
        obs("glucose", "5.4", "mmol/L", "2026-09-15", "g"),
        obs("insulin", "10", "u[IU]/mL", "2026-09-15", "i"),
    ]);
    let egfr = &one("ckd_epi_2021_cr", std::slice::from_ref(&report), &p)[0];
    assert_eq!(egfr["status"], "ok", "{egfr}");
    assert_eq!(egfr["date"], "2026-09-14");
    let homa = &one("homa_ir", &[report], &p)[0];
    assert_eq!(
        (homa["status"].clone(), homa["date"].clone()),
        (json!("ok"), json!("2026-09-15"))
    );
}

/// Results come oldest first by their own date, even when a report's other values are newer.
#[test]
fn results_are_ordered_by_their_own_date() {
    let p = person("1979-05-14", Some(Sex::Male));
    let a = draw(vec![
        obs("creatinine", "0.98", "mg/dL", "2026-09-14", "a"),
        obs("glucose", "5.4", "mmol/L", "2026-09-20", "g"),
    ]);
    let b = draw(vec![obs("creatinine", "0.90", "mg/dL", "2026-09-16", "b")]);
    // the draws are in Draw.date order: b (09-16), then a (09-20)
    let items = one("ckd_epi_2021_cr", &[b, a], &p);
    let dates: Vec<&str> = items.iter().filter_map(|i| i["date"].as_str()).collect();
    assert_eq!(dates, ["2026-09-14", "2026-09-16"]);
}

#[test]
fn homa_ir_says_fasting_is_not_recorded_only_when_the_glucose_row_does_not_say_it() {
    let p = person("1981-02-02", Some(Sex::Female));
    let with = |method: Option<&str>| {
        let mut g = obs("glucose", "5.4", "mmol/L", "2026-06-10", "g");
        g.method = method.map(str::to_string);
        one(
            "homa_ir",
            &[draw(vec![g, obs("insulin", "10", "u[IU]/mL", "2026-06-10", "i")])],
            &p,
        )[0]["limitations"]
            .clone()
    };
    assert_eq!(with(None), json!(["fasting_not_recorded"]));
    assert_eq!(with(Some("calculated")), json!(["fasting_not_recorded"]));
    assert_eq!(with(Some("fasting")), json!([]));
}

#[test]
fn a_zero_or_negative_input_is_out_of_range_not_a_value() {
    for (id, printed, age) in [
        (
            "fib4",
            vec![
                ("ast", "0", "U/L"),
                ("alt", "20", "U/L"),
                ("platelets", "210", "10*9/L"),
            ],
            Some(60),
        ),
        (
            "fib4",
            vec![
                ("ast", "-3", "U/L"),
                ("alt", "20", "U/L"),
                ("platelets", "210", "10*9/L"),
            ],
            Some(60),
        ),
        (
            "homa_ir",
            vec![("glucose", "0", "mmol/L"), ("insulin", "10", "u[IU]/mL")],
            None,
        ),
        (
            "homa_ir",
            vec![("glucose", "5.0", "mmol/L"), ("insulin", "0", "u[IU]/mL")],
            None,
        ),
    ] {
        let p: Vec<(&str, Decimal, &str)> = printed.iter().map(|(g, v, u)| (*g, d(v), *u)).collect();
        assert_eq!(calc::raw(data(), id, &p, age, None), Ok(None), "{id} {printed:?}");
        let values = printed.iter().map(|(g, v, u)| obs(g, v, u, "2026-06-10", g)).collect();
        let items = one(id, &[draw(values)], &person("1960-01-01", Some(Sex::Female)));
        assert_eq!(items[0]["reason_code"], "out_of_range", "{id} {printed:?}");
        assert!(items[0].get("value").is_none());
    }
}

#[test]
fn a_censored_input_keeps_its_comparator_in_the_inputs() {
    let mut ast = obs("ast", "5", "U/L", "2026-11-01", "a");
    ast.comparator = Some("<".into());
    ast.printed = "<5".into();
    let items = one(
        "fib4",
        &[draw(vec![
            ast,
            obs("alt", "20", "U/L", "2026-11-01", "b"),
            obs("platelets", "210", "10*9/L", "2026-11-01", "c"),
        ])],
        &person("1955-01-30", Some(Sex::Female)),
    );
    assert_eq!(items[0]["reason_code"], "not_a_number");
    assert_eq!(items[0]["inputs"][0]["value"], "<5");
    assert_eq!(items[0]["inputs"][1]["value"], "20");
}

// ---------------------------------------------------------------- on the demo family ----

#[tokio::test]
async fn the_demo_family_gets_results_and_reasons_twice_the_same() -> R {
    install_fake_genome();
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let mut first = Vec::new();
    for slug in ["alex", "sam", "lucia", "carmen"] {
        first.push(calc::document(&mut tx, s.pid(slug), as_of()).await?);
    }
    let by = |doc: &Value, id: &str| -> Vec<Value> {
        doc["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["id"] == id)
            .cloned()
            .collect()
    };
    let alex = &first[0];
    assert_eq!(alex["schema"], calc::SCHEMA);
    assert!(
        alex["calculators_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("sha256:"))
    );
    let egfr = by(alex, "ckd_epi_2021_cr");
    assert!(egfr.len() >= 3, "one result per draw with creatinine");
    assert!(
        egfr.iter()
            .all(|i| i["status"] == "ok" && i["src"].as_array().is_some_and(|s| s.len() == 1))
    );
    assert_eq!(by(alex, "ckd_epi_2021_cr_cys")[0]["reason_code"], "missing_input");
    // Lucía is 9: CKD-EPI refuses, FIB-4 refuses, before any value is read
    let lucia = &first[2];
    assert_eq!(by(lucia, "ckd_epi_2021_cr")[0]["reason_code"], "not_adult");
    assert_eq!(by(lucia, "fib4")[0]["reason_code"], "age_below_35");
    for doc in &first {
        for it in doc["items"].as_array().into_iter().flatten() {
            assert!(calc::STATUSES.contains(&it["status"].as_str().unwrap_or_default()));
            assert!(it["cite"].as_str().is_some_and(|c| c.starts_with("https://")));
            assert_eq!(it.get("value").is_some(), it["status"] == "ok", "{it}");
        }
    }
    // a second read gives the same bytes and the same hash
    for (i, slug) in ["alex", "sam", "lucia", "carmen"].into_iter().enumerate() {
        let again = calc::document(&mut tx, s.pid(slug), as_of()).await?;
        assert_eq!(
            ownhealth_core::canonical::canonical_json(&again)?,
            ownhealth_core::canonical::canonical_json(&first[i])?
        );
        assert_eq!(again["calculators_hash"], first[i]["calculators_hash"]);
        assert_eq!(
            calc::calculators_hash(&again)?,
            again["calculators_hash"].as_str().unwrap_or_default()
        );
    }
    // reading calculators leaves every state_hash as the fixtures pin it
    for slug in ["alex", "sam", "lucia", "carmen"] {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        assert_eq!(
            state_hash(&body)?,
            EXPECTED["bodies"][slug]["state_hash"].as_str().unwrap_or_default(),
            "{slug}"
        );
    }
    Ok(())
}

/// A FHIR bundle with no DiagnosticReport gives every Observation the bundle's document and no
/// report_key. Values from different days in it stay different draws.
#[tokio::test]
async fn one_document_without_a_report_key_splits_its_draws_by_day() -> R {
    install_fake_genome();
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let sam = s.pid("sam");
    let (doc, src): (uuid::Uuid, String) = sqlx::query_as(
        "select document_id, src_id from observation where profile_id = $1 and document_id is not null limit 1",
    )
    .bind(sam)
    .fetch_one(&mut *tx)
    .await?;
    for (group, value, unit, day) in [
        ("ast", "35", "U/L", "2024-03-02"),
        ("alt", "28", "U/L", "2024-03-02"),
        ("platelets", "210", "10*9/L", "2025-11-20"),
    ] {
        sqlx::query(
            "insert into observation (profile_id, category, document_id, source_kind, printed_name, printed_value, \
             printed_unit, value_num, alias_group, unit_ucum, derivation_version, effective_at, src_id, accepted_by, fhir) \
             values ($1, 'laboratory', $2, 'clinical', $3, $4, $5, $4::numeric, $3, $5, 'test', $6::date + time '08:00', \
             $7, 'import', '{}')",
        )
        .bind(sam)
        .bind(doc)
        .bind(group)
        .bind(value)
        .bind(unit)
        .bind(day)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
    }
    let got = calc::document(&mut tx, sam, as_of()).await?;
    let fib4: Vec<&Value> = got["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["id"] == "fib4" && i["status"] == "ok" && i["date"] == "2025-11-20")
        .collect();
    assert_eq!(fib4, Vec::<&Value>::new());
    Ok(())
}
