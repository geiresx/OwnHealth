//! Cardiovascular risk (SCORE2, SCORE2-OP, PREVENT): each formula reproduces its published vectors in
//! both cholesterol units, the country picks the SCORE2 region, every validity limit of the papers is
//! a refusal that names its reason, and the document stays deterministic and outside the state body.

mod common;

use std::path::Path;
use std::str::FromStr;

use chrono::NaiveDate;
use common::{EXPECTED, as_of, install_fake_genome, seeded};
use ownhealth_core::access::{all_scopes, scope_set};
use ownhealth_rules::calc::cardio::{self, CvData, Fact, Factors, Facts, Sbp};
use ownhealth_rules::calc::{self, Calc, Data, Draw, Obs, Person, Sex};
use ownhealth_rules::{build_state, cited, state_hash};
use rust_decimal::Decimal;
use serde_json::{Value, json};

type R = anyhow::Result<()>;

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/calc");
const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/calculators");
const FILES: [&str; 4] = ["score2", "score2_op", "prevent_cvd", "prevent_ascvd"];
const ON: &str = "2026-11-07";

fn data() -> &'static Data {
    calc::data().expect("the shipped calculator files load")
}

fn cv() -> &'static CvData {
    cardio::data().expect("the shipped cardiovascular files load")
}

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).expect("a number")
}

fn day(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("a date")
}

fn vectors(name: &str) -> Value {
    let text = std::fs::read_to_string(format!("{VECTORS}/{name}.json")).expect("vector file");
    serde_json::from_str(&text).expect("vector json")
}

/// Every case whose result, rounded to the file's precision, differs from its expected value.
fn mismatches(file: &Value) -> Vec<String> {
    let id = file["calculator"].as_str().unwrap_or_default();
    let places = u32::try_from(file["precision"].as_u64().unwrap_or(1)).unwrap_or(1);
    let mut bad = Vec::new();
    for (i, c) in file["cases"].as_array().into_iter().flatten().enumerate() {
        let printed: Vec<(String, Decimal, String)> = c["inputs"]
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
        let flag = |k: &str| c[k].as_bool().unwrap_or(false);
        let f = Factors {
            region: c["region"].as_str(),
            smoker: flag("smoker"),
            sbp: d(c["sbp"].as_str().unwrap_or("0")),
            diabetes: flag("diabetes"),
            egfr: d(c["egfr"].as_str().unwrap_or("0")),
            bp_treated: flag("bp_treated"),
            statin: flag("statin"),
        };
        let age = u32::try_from(c["age"].as_u64().unwrap_or_default()).unwrap_or_default();
        let sex = Sex::from_record(c["sex"].as_str()).expect("a sex");
        let got = cardio::raw(data(), cv(), id, &printed, age, sex, &f)
            .map(|v| v.map(|v| ownhealth_rules::num::quantize(v, places).to_string()));
        let want = c["expected"].as_str().unwrap_or_default();
        if got != Ok(Some(want.to_string())) {
            bad.push(format!("{id} case {i}: want {want}, got {got:?}"));
        }
    }
    bad
}

#[test]
fn every_risk_equation_reproduces_its_published_vectors() {
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
        assert!(
            file["cases"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|c| c["from"].as_str().is_some_and(|s| !s.is_empty())),
            "{name}: every case says where it comes from"
        );
        assert_eq!(mismatches(&file), Vec::<String>::new());
    }
}

/// The control: one wrong expected value turns the vector check red.
#[test]
fn a_wrong_risk_vector_fails_the_suite() {
    for name in FILES {
        let mut file = vectors(name);
        let first = &mut file["cases"][0]["expected"];
        let wrong = d(first.as_str().unwrap_or_default()) + d("0.1");
        *first = json!(wrong.to_string());
        assert_eq!(mismatches(&file).len(), 1, "{name}");
    }
}

#[test]
fn the_cardiovascular_files_pass_the_cited_loader_and_refuse_a_bad_row() -> R {
    for (name, bytes) in cardio::FILES {
        let path = Path::new(DATA).join(name);
        let f = cited::load(&path)?;
        assert_eq!(std::fs::read_to_string(&path)?, bytes, "{name} is the compiled-in copy");
        assert!(!f.version.is_empty());
    }
    let [r, g, c] = cardio::FILES.map(|f| f.1);
    for (what, rr, gg, cc, reason) in [
        (
            "http cite",
            r.replacen("https://academic", "http://academic", 1),
            g.to_string(),
            c.to_string(),
            "cite",
        ),
        (
            "country twice",
            r.replacen("[AT, ", "[ES, AT, ", 1),
            g.to_string(),
            c.to_string(),
            "ES is in both",
        ),
        (
            "unknown region",
            r.replacen("region: high", "region: medium", 1),
            g.to_string(),
            c.to_string(),
            "medium",
        ),
        (
            "no scale",
            r.replacen(", female: [\"-0.52\", \"1.01\"]", "", 1),
            g.to_string(),
            c.to_string(),
            "no score2_op scale",
        ),
        (
            "unknown class",
            r.to_string(),
            g.replacen("id: statin", "id: fibrate", 1),
            c.to_string(),
            "fibrate",
        ),
        (
            "unknown condition",
            r.to_string(),
            g.to_string(),
            c.replacen("id: fh", "id: gout", 1),
            "gout",
        ),
    ] {
        assert!(rr != r || gg != g || cc != c, "{what}: the mutation did not apply");
        let e = cardio::parse(&rr, &gg, &cc).expect_err(what);
        assert!(e.0.contains(reason), "{what}: {e}");
    }
    Ok(())
}

/// A report's systolic pressure or smoking status, in any of the four languages, lands in the group
/// the risk calculators read, with its LOINC code.
#[tokio::test]
async fn systolic_pressure_and_smoking_status_resolve_in_es_en_de_fr() -> R {
    for (name, lang, unit, group, loinc) in [
        (
            "Presión arterial sistólica",
            "es",
            Some("mmHg"),
            "bp_systolic",
            "8480-6",
        ),
        ("Systolic blood pressure", "en", Some("mm[Hg]"), "bp_systolic", "8480-6"),
        ("Systolischer Blutdruck", "de", Some("mmHg"), "bp_systolic", "8480-6"),
        (
            "Pression artérielle systolique",
            "fr",
            Some("mmHg"),
            "bp_systolic",
            "8480-6",
        ),
        ("Tabaquismo", "es", None, "smoking_status", "72166-2"),
        ("Smoking status", "en", None, "smoking_status", "72166-2"),
        ("Raucherstatus", "de", None, "smoking_status", "72166-2"),
        ("Statut tabagique", "fr", None, "smoking_status", "72166-2"),
    ] {
        let q = ownhealth_normalize::ResolveQuery {
            lang,
            unit,
            ..Default::default()
        };
        let r = ownhealth_normalize::resolve_analyte(None, name, &q).await?;
        assert_eq!(
            (r.alias_group.as_str(), r.loinc.as_deref()),
            (group, Some(loinc)),
            "{name}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- regions and classes ----

#[test]
fn the_country_picks_the_score2_region() {
    let region = |c: &str| cv().region_of(c).map(|r| r.region.clone());
    assert_eq!(region("ES"), Some("low".into()));
    assert_eq!(region("es"), Some("low".into()));
    assert_eq!(region("DE"), Some("moderate".into()));
    assert_eq!(region("PL"), Some("high".into()));
    assert_eq!(region("RO"), Some("very_high".into()));
    assert_eq!(region("US"), None);
    // the same person is at higher risk in a higher-risk region
    let f = |r| Factors {
        region: Some(r),
        smoker: true,
        sbp: d("140"),
        diabetes: false,
        egfr: d("90"),
        bp_treated: false,
        statin: false,
    };
    let risk = |r| cardio::compute(cv(), "score2", d("6.3"), d("1.4"), 50, Sex::Male, &f(r)).unwrap_or_default();
    assert!(risk("low") < risk("moderate") && risk("moderate") < risk("high") && risk("high") < risk("very_high"));
}

#[test]
fn a_statin_is_found_by_its_spanish_brand_and_a_condition_by_its_words_or_icd10_code() {
    assert_eq!(
        cardio::drug_class(cv(), &["CARDYL 20 mg comprimidos recubiertos", ""]),
        vec!["statin"]
    );
    assert_eq!(
        cardio::drug_class(cv(), &["Atorvastatina Cinfa 40 mg", ""]),
        vec!["statin"]
    );
    assert_eq!(
        cardio::drug_class(cv(), &["Cozaar", "losartán"]),
        vec!["antihypertensive"]
    );
    assert!(cardio::drug_class(cv(), &["Omeprazol 20 mg", "omeprazole"]).is_empty());
    assert_eq!(
        cardio::condition_classes(cv(), "Diabetes mellitus tipo 2", None, None),
        vec!["diabetes"]
    );
    assert_eq!(
        cardio::condition_classes(cv(), "Infarto agudo de miocardio", None, None),
        vec!["ascvd"]
    );
    assert_eq!(
        cardio::condition_classes(cv(), "Hipercolesterolemia familiar", None, None),
        vec!["fh"]
    );
    assert_eq!(
        cardio::condition_classes(cv(), "Enfermedad renal crónica estadio 3", None, None),
        vec!["ckd"]
    );
    // a high risk is no reason to drop a diagnosis; a risk of the disease is not the disease
    assert_eq!(
        cardio::condition_classes(cv(), "Diabetes mellitus tipo 2, alto riesgo cardiovascular", None, None),
        vec!["diabetes"]
    );
    assert_eq!(
        cardio::condition_classes(cv(), "Type 2 diabetes, high cardiovascular risk", None, None),
        vec!["diabetes"]
    );
    assert_eq!(
        cardio::condition_classes(cv(), "Enfermedad renal crónica, riesgo alto", None, None),
        vec!["ckd"]
    );
    assert!(cardio::condition_classes(cv(), "Riesgo de diabetes", None, None).is_empty());
    // an exclusion holds only in its own clause: a risk or a family history named beside the
    // diagnosis does not erase it
    for text in [
        "Diabetes mellitus tipo 2 con riesgo de complicaciones",
        "Type 2 diabetes with risk of complications",
        "Antecedentes familiares de diabetes; diabetes mellitus tipo 2",
        "Family history of diabetes, type 2 diabetes",
    ] {
        assert_eq!(
            cardio::condition_classes(cv(), text, None, None),
            vec!["diabetes"],
            "{text}"
        );
    }
    assert!(cardio::condition_classes(cv(), "Antecedentes familiares de diabetes", None, None).is_empty());
    // a heading before a colon qualifies its whole list, and a bracket qualifies the words before it
    for text in [
        "Family history: diabetes mellitus",
        "Antecedentes familiares: cardiopatía isquémica, diabetes",
        "Screening for diabetes mellitus (DM2)",
        "Diabetes mellitus (gestational)",
        "Diabetes mellitus (tipo 2, gestacional)",
        "Diabetes mellitus (gestational with DM2)",
        "Diabetes mellitus (with gestational onset)",
        "Screening for diabetes mellitus (with DM2)",
    ] {
        assert!(cardio::condition_classes(cv(), text, None, None).is_empty(), "{text}");
    }
    // the heading is only the clause right before the colon, and it ends with its statement
    assert_eq!(
        cardio::condition_classes(
            cv(),
            "Type 2 diabetes, screening for diabetes mellitus: negative",
            None,
            None
        ),
        vec!["diabetes"]
    );
    assert_eq!(
        cardio::condition_classes(
            cv(),
            "Antecedentes familiares: infarto; diabetes mellitus (DM2)",
            None,
            None
        ),
        vec!["diabetes"]
    );
    // a risk, a screening or a family history excludes only a disease named after it: one named after
    // the disease is a complication of a recorded diagnosis, which stays
    for (text, class) in [
        ("Stroke (risk of recurrence)", "ascvd"),
        ("Ictus isquémico (riesgo de recurrencia)", "ascvd"),
        ("Diabetes mellitus tipo 2 (riesgo de hipoglucemia)", "diabetes"),
        ("Diabetes mellitus tipo 2: riesgo de hipoglucemia", "diabetes"),
        ("Type 2 diabetes: screening for retinopathy negative", "diabetes"),
        ("Heart failure: at risk of decompensation", "heart_failure"),
        ("Chronic kidney disease (risk of progression)", "ckd"),
    ] {
        assert_eq!(cardio::condition_classes(cv(), text, None, None), vec![class], "{text}");
    }
    // a family history is not this person, before or after the disease; "family history of" names
    // only what follows it, so a diagnosis before it stays
    for text in [
        "Diabetes (family history)",
        "Diabetes (antecedentes familiares)",
        "Stroke (family history)",
        "Infarto (antecedentes familiares)",
        "Diabetes: family history",
        "Chronic kidney disease (family history)",
        "Heart failure - family history",
        "Familial hypercholesterolaemia (family history)",
        "Hipercolesterolemia familiar (antecedentes familiares)",
    ] {
        assert!(cardio::condition_classes(cv(), text, None, None).is_empty(), "{text}");
    }
    for (text, class) in [
        ("Diabetes tipo 2 y antecedentes familiares de infarto", "diabetes"),
        ("Type 2 diabetes: family history of stroke", "diabetes"),
        ("Type 2 diabetes (no family history)", "diabetes"),
        ("Diabetes tipo 2 sin antecedentes familiares", "diabetes"),
        (
            "Familial hypercholesterolaemia: family history of premature coronary disease",
            "fh",
        ),
    ] {
        assert_eq!(cardio::condition_classes(cv(), text, None, None), vec![class], "{text}");
    }
    // a screening is not the disease
    for text in [
        "Screening for diabetes mellitus",
        "Cribado de diabetes",
        "Screening for chronic kidney disease",
        "Cribado de enfermedad arterial periférica",
        "Screening for heart failure",
        "Screening for familial hypercholesterolaemia",
        "Cribado de hipercolesterolemia familiar",
        "Riesgo de hipercolesterolemia familiar",
    ] {
        assert!(cardio::condition_classes(cv(), text, None, None).is_empty(), "{text}");
    }
    assert_eq!(
        cardio::condition_classes(cv(), "Chronic kidney disease (screening for anaemia)", None, None),
        vec!["ckd"]
    );
    assert!(cardio::condition_classes(cv(), "At risk of heart failure", None, None).is_empty());
    // words that mean something else, and codes read only as received
    assert!(cardio::condition_classes(cv(), "Diabetes gestacional", None, None).is_empty());
    assert!(cardio::condition_classes(cv(), "Gestational diabetes", None, None).is_empty());
    assert!(cardio::condition_classes(cv(), "Diabetes insípida", None, None).is_empty());
    // a negation before the disease in its clause is not the disease; after it, the disease stays
    for text in [
        "Hypertension without diabetes",
        "Hipertensión sin diabetes",
        "No diabetes",
    ] {
        assert!(cardio::condition_classes(cv(), text, None, None).is_empty(), "{text}");
    }
    for text in ["Diabetes without complications", "Diabetes tipo 2 sin complicaciones"] {
        assert_eq!(
            cardio::condition_classes(cv(), text, None, None),
            vec!["diabetes"],
            "{text}"
        );
    }
    assert!(cardio::condition_classes(cv(), "Antecedentes familiares de infarto", None, None).is_empty());
    assert!(cardio::condition_classes(cv(), "Hypercholesterolaemia", Some("icd10"), Some("E78.0")).is_empty());
    assert_eq!(
        cardio::condition_classes(cv(), "Old MI", Some("http://hl7.org/fhir/sid/icd-10"), Some("I25.2")),
        vec!["ascvd"]
    );
    assert!(cardio::condition_classes(cv(), "Old MI", Some("http://snomed.info/sct"), Some("I25.2")).is_empty());
}

// ---------------------------------------------------------------- refusals (pure) ----

fn calc_row(id: &str) -> &'static Calc {
    data().calcs.iter().find(|c| c.id == id).expect("a calculator row")
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

fn lipids(tc: &str, hdl: &str, unit: &str) -> Draw {
    Draw {
        date: Some(day("2026-09-14")),
        obs: [
            (
                "total_cholesterol".to_string(),
                obs("total_cholesterol", tc, unit, "2026-09-14", "src_tc"),
            ),
            ("hdl_c".to_string(), obs("hdl_c", hdl, unit, "2026-09-14", "src_hdl")),
        ]
        .into_iter()
        .collect(),
    }
}

fn fact(value: &str, src: &str) -> Fact {
    Fact {
        value: value.into(),
        date: Some(day("2026-01-10")),
        src: src.into(),
        printed: None,
    }
}

/// A non-smoker in Spain with an office reading of 128 mmHg.
fn facts() -> Facts {
    Facts {
        country: Some("ES".into()),
        smoking: Some(fact("never", "src_smk")),
        sbp: Some(Sbp::Office(obs("bp_systolic", "128", "mm[Hg]", "2026-10-02", "src_bp"))),
        ..Facts::default()
    }
}

fn born(year: i32, sex: Sex) -> Person {
    Person {
        birth_date: Some(NaiveDate::from_ymd_opt(year, 3, 1).expect("a date")),
        sex: Some(sex),
    }
}

/// A computed eGFR, as slice 9's CKD-EPI item gives it.
fn egfr(value: &str) -> Value {
    json!({"id": "ckd_epi_2021_cr", "status": "ok", "value": value, "unit": "mL/min/{1.73_m2}", "date": "2026-09-14", "src": ["src_cr"]})
}

fn run(id: &str, draws: &[Draw], p: &Person, f: &Facts, computed: &[Value]) -> Vec<Value> {
    cardio::evaluate(data(), cv(), calc_row(id), draws, p, f, computed, day(ON))
}

fn reason(items: &[Value]) -> (String, Value) {
    let it = items.first().cloned().unwrap_or_default();
    assert!(it.get("value").is_none(), "a refusal carries no value: {it}");
    (
        it["reason_code"].as_str().unwrap_or_default().to_string(),
        it["reason"].clone(),
    )
}

#[test]
fn a_person_in_spain_gets_score2_for_the_low_risk_region_with_every_source() {
    let items = run(
        "score2",
        &[lipids("5.8", "1.3", "mmol/L")],
        &born(1971, Sex::Male),
        &facts(),
        &[],
    );
    assert_eq!(items.len(), 1);
    let it = &items[0];
    assert_eq!(it["status"], "ok", "{it}");
    assert_eq!(it["region"], "low");
    assert_eq!(it["person"], json!({"age": 55, "sex": "male"}));
    assert_eq!(it["src"], json!(["src_tc", "src_hdl", "src_bp", "src_smk"]));
    let groups: Vec<&str> = it["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["group"].as_str())
        .collect();
    assert_eq!(
        groups,
        [
            "total_cholesterol",
            "hdl_c",
            "bp_systolic",
            "smoking_status",
            "ascvd",
            "diabetes",
            "ckd",
            "fh"
        ]
    );
    assert_eq!(
        it["inputs"][5],
        json!({"group": "diabetes", "value": "not_recorded", "unit": null})
    );
    // the same number the equation gives
    let f = Factors {
        region: Some("low"),
        smoker: false,
        sbp: d("128"),
        diabetes: false,
        egfr: Decimal::ZERO,
        bp_treated: false,
        statin: false,
    };
    let raw = cardio::compute(cv(), "score2", d("5.8"), d("1.3"), 55, Sex::Male, &f).unwrap_or_default();
    assert_eq!(it["value"], ownhealth_rules::num::quantize(raw, 1).to_string());
    // SCORE2-OP does not answer below 70, nor SCORE2 from 70
    assert!(
        run(
            "score2_op",
            &[lipids("5.8", "1.3", "mmol/L")],
            &born(1971, Sex::Male),
            &facts(),
            &[]
        )
        .is_empty()
    );
    let old = born(1954, Sex::Female);
    assert!(run("score2", &[lipids("5.8", "1.3", "mmol/L")], &old, &facts(), &[]).is_empty());
    let op = run("score2_op", &[lipids("5.8", "1.3", "mmol/L")], &old, &facts(), &[]);
    assert_eq!(
        (op[0]["status"].clone(), op[0]["region"].clone()),
        (json!("ok"), json!("low"))
    );
}

#[test]
fn a_us_profile_gets_prevent_and_score2_says_its_country_has_no_region() {
    let mut f = facts();
    f.country = Some("US".into());
    let p = born(1971, Sex::Female);
    let draws = [lipids("200", "45", "mg/dL")];
    for id in ["prevent_cvd", "prevent_ascvd"] {
        let it = &run(id, &draws, &p, &f, &[egfr("90")])[0];
        assert_eq!(it["status"], "ok", "{it}");
        assert_eq!(it["limitations"], json!([]), "a US profile is inside PREVENT's cohorts");
        assert!(
            it["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|i| i["group"] == "egfr" && i["src"] == "src_cr")
        );
    }
    assert_eq!(
        reason(&run("score2", &draws, &p, &f, &[])),
        ("region_not_covered".into(), json!({"country": "US"}))
    );
    // outside the US PREVENT still computes, and says where it was built
    let es = &run("prevent_cvd", &draws, &p, &facts(), &[egfr("90")])[0];
    assert_eq!(es["limitations"], json!(["prevent_us_cohorts"]));
    f.country = None;
    assert_eq!(reason(&run("score2", &draws, &p, &f, &[])).0, "country_not_recorded");
}

#[test]
fn every_published_validity_limit_is_a_refusal_with_its_reason() {
    let p = born(1971, Sex::Male);
    let mmol = [lipids("5.8", "1.3", "mmol/L")];
    let with = |c: &str| {
        let mut f = facts();
        f.conditions.insert(c.into(), fact("recorded", "src_cond"));
        f
    };
    // age: SCORE2 from 40, PREVENT to 79
    assert_eq!(
        reason(&run("score2", &mmol, &born(1987, Sex::Male), &facts(), &[])),
        (
            "age_out_of_range".into(),
            json!({"low": 40, "high": 69, "age": 39, "on": ON})
        )
    );
    assert_eq!(
        reason(&run(
            "prevent_cvd",
            &mmol,
            &born(1946, Sex::Male),
            &facts(),
            &[egfr("90")]
        ))
        .1["age"],
        80
    );
    // SCORE2 is for apparently healthy people
    for c in ["diabetes", "ascvd", "ckd", "fh"] {
        assert_eq!(
            reason(&run("score2", &mmol, &p, &with(c), &[])),
            ("outside_score2_scope".into(), json!({"condition": c}))
        );
    }
    // PREVENT refuses established disease, and reads diabetes as an input
    assert_eq!(
        reason(&run("prevent_cvd", &mmol, &p, &with("ascvd"), &[egfr("90")])).0,
        "outside_prevent_scope"
    );
    assert_eq!(
        reason(&run("prevent_cvd", &mmol, &p, &with("heart_failure"), &[egfr("90")])),
        ("outside_prevent_scope".into(), json!({"condition": "heart_failure"}))
    );
    assert_eq!(
        run("prevent_cvd", &mmol, &p, &with("diabetes"), &[egfr("90")])[0]["status"],
        "ok"
    );
    // total cholesterol above 8 mmol/L (310 mg/dL)
    for (tc, unit, limit) in [("8.2", "mmol/L", "8"), ("320", "mg/dL", "310")] {
        let hdl = if unit == "mmol/L" { "1.3" } else { "50" };
        let (code, why) = reason(&run("score2", &[lipids(tc, hdl, unit)], &p, &facts(), &[]));
        assert_eq!(
            (code.as_str(), why["group"].clone(), why["limit"].clone()),
            ("marked_risk_factor", json!("total_cholesterol"), json!(limit)),
            "{tc} {unit}"
        );
    }
    // at exactly 8 mmol/L the rule (above 8) does not hold
    assert_eq!(
        run("score2", &[lipids("8.0", "1.3", "mmol/L")], &p, &facts(), &[])[0]["status"],
        "ok"
    );
    // markedly raised single risk factors
    let mut ldl = lipids("5.8", "1.3", "mmol/L");
    ldl.obs
        .insert("ldl_c".into(), obs("ldl_c", "5.0", "mmol/L", "2026-09-14", "src_ldl"));
    let (code, why) = reason(&run("score2", &[ldl], &p, &facts(), &[]));
    assert_eq!(
        (code.as_str(), why["group"].clone(), why["limit"].clone()),
        ("marked_risk_factor", json!("ldl_c"), json!("4.9"))
    );
    // a newer total cholesterol with no HDL beside it still counts for the rule, and the risk uses the pair
    let newer_tc = Draw {
        date: Some(day("2026-10-20")),
        obs: [(
            "total_cholesterol".to_string(),
            obs("total_cholesterol", "9", "mmol/L", "2026-10-20", "src_tc2"),
        )]
        .into_iter()
        .collect(),
    };
    let (code, why) = reason(&run(
        "score2",
        &[lipids("5.8", "1.3", "mmol/L"), newer_tc],
        &p,
        &facts(),
        &[],
    ));
    assert_eq!(
        (code.as_str(), why["group"].clone(), why["on"].clone()),
        ("marked_risk_factor", json!("total_cholesterol"), json!("2026-10-20"))
    );
    // a second draw on the pair's own day counts too, when its total cholesterol is the high one
    let same_day_tc = Draw {
        date: Some(day("2026-09-14")),
        obs: [(
            "total_cholesterol".to_string(),
            obs("total_cholesterol", "9", "mmol/L", "2026-09-14", "src_tc_same_day"),
        )]
        .into_iter()
        .collect(),
    };
    let items = run(
        "score2",
        &[lipids("5.8", "1.3", "mmol/L"), same_day_tc.clone()],
        &p,
        &facts(),
        &[],
    );
    let (code, why) = reason(&items);
    assert_eq!(
        (code.as_str(), why["group"].clone(), why["value"].clone()),
        ("marked_risk_factor", json!("total_cholesterol"), json!("9")),
        "{items:?}"
    );
    // the same two draws in the other order give the same refusal
    let items = run(
        "score2",
        &[same_day_tc, lipids("5.8", "1.3", "mmol/L")],
        &p,
        &facts(),
        &[],
    );
    let (code, why) = reason(&items);
    assert_eq!(
        (code.as_str(), why["group"].clone(), why["value"].clone()),
        ("marked_risk_factor", json!("total_cholesterol"), json!("9")),
        "{items:?}"
    );
    // an LDL printed as above a value at or over the limit is over it too
    let mut censored = lipids("5.8", "1.3", "mmol/L");
    let mut over = obs("ldl_c", "5.2", "mmol/L", "2026-09-14", "src_ldl");
    over.comparator = Some(">".into());
    over.printed = ">5.2".into();
    censored.obs.insert("ldl_c".into(), over);
    let (code, why) = reason(&run("score2", &[censored], &p, &facts(), &[]));
    assert_eq!(
        (code.as_str(), why["group"].clone(), why["value"].clone()),
        ("marked_risk_factor", json!("ldl_c"), json!(">5.2"))
    );
    // an LDL printed as above the limit itself (">4.9") is over it; one printed as at least the limit is not
    for (comparator, refused) in [(">", true), (">=", false)] {
        let mut at_limit = lipids("5.8", "1.3", "mmol/L");
        let mut o = obs("ldl_c", "4.9", "mmol/L", "2026-09-14", "src_ldl");
        o.comparator = Some(comparator.into());
        o.printed = format!("{comparator}4.9");
        at_limit.obs.insert("ldl_c".into(), o);
        let items = run("score2", &[at_limit], &p, &facts(), &[]);
        assert_eq!(items[0]["status"] != "ok", refused, "{comparator} {items:?}");
        if refused {
            assert_eq!(reason(&items).1["value"], json!(">4.9"));
        }
    }
    let mut high_bp = facts();
    high_bp.sbp = Some(Sbp::Office(obs("bp_systolic", "210", "mm[Hg]", "2026-10-02", "src_bp")));
    assert_eq!(reason(&run("score2", &mmol, &p, &high_bp, &[])).0, "marked_risk_factor");
    // the pressure rule holds from 180 itself, and the words say so
    let mut at_180 = facts();
    at_180.sbp = Some(Sbp::Office(obs("bp_systolic", "180", "mm[Hg]", "2026-10-02", "src_bp")));
    let items = run("score2", &mmol, &p, &at_180, &[]);
    assert_eq!(reason(&items).0, "marked_risk_factor");
    let doc = json!({"schema": calc::SCHEMA, "profile": "x", "as_of": ON, "items": items});
    assert_eq!(
        calc::render(&doc, "en").expect("render")["items"][0]["text"],
        "Not computed: systolic blood pressure 180 mmHg on 2026-10-02 is at or above 180 mmHg, a markedly raised single risk factor. SCORE2 does not apply (ESC 2021)."
    );
    assert_eq!(
        calc::render(&doc, "es").expect("render")["items"][0]["text"],
        "Sin calcular: presión arterial sistólica de 180 mmHg el 2026-10-02 alcanza o supera 180 mmHg, un factor de riesgo aislado muy elevado. SCORE2 no se aplica (ESC 2021)."
    );
    let mut below = facts();
    below.sbp = Some(Sbp::Office(obs("bp_systolic", "179", "mm[Hg]", "2026-10-02", "src_bp")));
    assert_eq!(run("score2", &mmol, &p, &below, &[])[0]["status"], "ok");
    // a pressure of zero or below is no reading: refused, never a risk
    let mut zero = facts();
    zero.sbp = Some(Sbp::Office(obs("bp_systolic", "0", "mm[Hg]", "2026-10-02", "src_bp")));
    assert_eq!(reason(&run("score2", &mmol, &p, &zero, &[])).0, "out_of_range");
    assert_eq!(
        reason(&run("prevent_cvd", &mmol, &p, &high_bp, &[egfr("90")])),
        (
            "input_out_of_range".into(),
            json!({"group": "bp_systolic", "value": "210", "unit": "mmHg", "low": "90", "high": "200"})
        )
    );
    // kidney function: PREVENT's range, SCORE2's chronic kidney disease
    assert_eq!(
        reason(&run("prevent_cvd", &mmol, &p, &facts(), &[egfr("12")])).1["group"],
        "egfr"
    );
    assert_eq!(
        reason(&run("score2", &mmol, &p, &facts(), &[egfr("52")])).0,
        "egfr_below_60"
    );
    // total and HDL cholesterol from two days are not one draw; both show with their sources
    let mut hdl_later = lipids("5.8", "1.3", "mmol/L");
    hdl_later.obs.remove("hdl_c");
    let two = [
        hdl_later,
        Draw {
            date: Some(day("2026-10-01")),
            obs: [(
                "hdl_c".to_string(),
                obs("hdl_c", "1.3", "mmol/L", "2026-10-01", "src_hdl2"),
            )]
            .into_iter()
            .collect(),
        },
    ];
    let items = run("score2", &two, &p, &facts(), &[]);
    assert_eq!(reason(&items).0, "inputs_not_same_draw");
    assert_eq!(items[0]["src"], json!(["src_tc", "src_hdl2", "src_bp", "src_smk"]));
    // PREVENT's cholesterol range
    assert_eq!(
        reason(&run(
            "prevent_cvd",
            &[lipids("340", "50", "mg/dL")],
            &p,
            &facts(),
            &[egfr("90")]
        ))
        .1["group"],
        "total_cholesterol"
    );
    // a missing input is named, never assumed
    let mut no_smoking = facts();
    no_smoking.smoking = None;
    let items = run("score2", &mmol, &p, &no_smoking, &[]);
    assert_eq!(items[0]["status"], "not_assessed");
    assert_eq!(
        reason(&items),
        ("missing_input".into(), json!({"groups": ["smoking_status"]}))
    );
    let items = run("prevent_cvd", &mmol, &p, &facts(), &[]);
    assert_eq!(reason(&items).1, json!({"groups": ["egfr"]}));
    let nothing = Facts {
        country: Some("ES".into()),
        ..Facts::default()
    };
    assert_eq!(
        reason(&run("score2", &[], &p, &nothing, &[])).1,
        json!({"groups": ["total_cholesterol", "hdl_c", "bp_systolic", "smoking_status"]})
    );
    // no sex or birth date recorded
    assert_eq!(
        reason(&run(
            "score2",
            &mmol,
            &Person {
                birth_date: None,
                sex: Some(Sex::Male)
            },
            &facts(),
            &[]
        ))
        .0,
        "birth_date_not_recorded"
    );
    assert_eq!(
        reason(&run(
            "score2",
            &mmol,
            &Person {
                birth_date: p.birth_date,
                sex: None
            },
            &facts(),
            &[]
        ))
        .0,
        "sex_not_recorded"
    );
}

#[test]
fn a_wearable_mean_stands_in_for_an_office_reading_and_says_so() {
    let mut f = facts();
    f.sbp = Some(Sbp::Wearable {
        mean: d("124.5"),
        since: day("2026-10-09"),
        until: day("2026-11-07"),
        days: 21,
        source: 7,
    });
    let it = &run(
        "score2",
        &[lipids("5.8", "1.3", "mmol/L")],
        &born(1971, Sex::Male),
        &f,
        &[],
    )[0];
    assert_eq!(it["status"], "ok");
    assert_eq!(it["limitations"], json!(["sbp_from_wearable"]));
    assert_eq!(
        it["inputs"][2]["window"],
        json!({"since": "2026-10-09", "until": "2026-11-07", "days": 21})
    );
    // wearables have no src id: the line names the wearable_source the daily means came from
    assert_eq!(it["inputs"][2]["wearable_source"], 7);
    let doc = json!({"schema": calc::SCHEMA, "profile": "x", "as_of": ON, "items": [it]});
    for (lang, want) in [
        (
            "en",
            "systolic blood pressure 124.5 mmHg, mean of 21 days of wearable readings (2026-10-09 to 2026-11-07)",
        ),
        (
            "es",
            "presión arterial sistólica 124,5 mmHg, media de 21 días de lecturas del wearable (2026-10-09 a 2026-11-07)",
        ),
    ] {
        let out = calc::render(&doc, lang).expect("render");
        assert_eq!(out["items"][0]["inputs"][2]["text"], want, "{lang}");
    }
}

/// The input lines say what the record holds, never what the person does not have.
#[test]
fn every_risk_line_and_refusal_reads_in_both_languages() -> R {
    let p = born(1971, Sex::Male);
    let mut items = run("score2", &[lipids("5.8", "1.3", "mmol/L")], &p, &facts(), &[]);
    let mut f = facts();
    f.drugs.insert("statin".into(), fact("recorded", "src_med"));
    items.extend(run(
        "prevent_ascvd",
        &[lipids("5.8", "1.3", "mmol/L")],
        &p,
        &f,
        &[egfr("90")],
    ));
    items.extend(run(
        "score2",
        &[lipids("5.8", "1.3", "mmol/L")],
        &born(1987, Sex::Male),
        &facts(),
        &[],
    ));
    let doc = json!({"schema": calc::SCHEMA, "profile": "x", "as_of": ON, "items": items});
    for lang in ["en", "es"] {
        let out = calc::render(&doc, lang)?;
        for it in out["items"].as_array().into_iter().flatten() {
            if it["status"] != "ok" {
                let t = it["text"].as_str().unwrap_or_default();
                assert!(!t.is_empty() && !t.contains('{'), "{lang}: {t}");
            }
            for i in it["inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| i["unit"].is_null())
            {
                assert!(i["text"].as_str().is_some_and(|t| !t.is_empty()), "{lang}: {i}");
            }
        }
    }
    let en = calc::render(&doc, "en")?;
    assert_eq!(
        en["items"][0]["name"],
        "SCORE2, 10-year cardiovascular risk, low-risk region"
    );
    assert_eq!(en["items"][0]["inputs"][5]["text"], "no diabetes recorded");
    assert_eq!(
        en["items"][1]["inputs"]
            .as_array()
            .and_then(|a| a.last())
            .map(|i| i["text"].clone()),
        Some(json!("statin recorded"))
    );
    let es = calc::render(&doc, "es")?;
    assert_eq!(es["items"][0]["inputs"][3]["text"], "nunca ha fumado");
    assert_eq!(
        es["items"][2]["text"],
        "Sin calcular: la ecuación abarca de 40 a 69 años; 39 años el 2026-11-07."
    );
    // missing inputs read as a list
    let no_facts = Facts {
        country: Some("ES".into()),
        ..Facts::default()
    };
    let missing = run("score2", &[lipids("5.8", "1.3", "mmol/L")], &p, &no_facts, &[]);
    let doc = json!({"schema": calc::SCHEMA, "profile": "x", "as_of": ON, "items": missing});
    assert_eq!(
        calc::render(&doc, "es")?["items"][0]["text"],
        "Sin calcular: el registro no tiene presión arterial sistólica y tabaquismo."
    );
    let none = run("score2", &[], &p, &no_facts, &[]);
    let doc = json!({"schema": calc::SCHEMA, "profile": "x", "as_of": ON, "items": none});
    assert_eq!(
        calc::render(&doc, "en")?["items"][0]["text"],
        "Not computed: no total cholesterol, HDL cholesterol, systolic blood pressure and smoking status on record."
    );
    Ok(())
}

// ---------------------------------------------------------------- on the demo family ----

/// Alex, 47, in Spain: with an office reading and a smoking status he gets SCORE2 for the low-risk
/// region, and PREVENT reads his statin and his eGFR. Twice the same bytes, and no state_hash moves.
#[tokio::test]
async fn the_demo_family_reads_its_facts_and_keeps_every_state_hash() -> R {
    install_fake_genome();
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let alex = s.pid("alex");
    // with no country SCORE2 has no region; the seed records ES
    sqlx::query("update profile set country = null where id = $1")
        .bind(alex)
        .execute(&mut *tx)
        .await?;
    let before = calc::document(&mut tx, alex, as_of()).await?;
    let score2 = |doc: &Value, id: &str| {
        doc["items"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["id"] == id)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(score2(&before, "score2")["reason_code"], "country_not_recorded");
    sqlx::query("update profile set country = 'ES' where id = $1")
        .bind(alex)
        .execute(&mut *tx)
        .await?;
    let missing = calc::document(&mut tx, alex, as_of()).await?;
    // the seed holds LDL-C only
    assert_eq!(
        score2(&missing, "score2")["reason"]["groups"],
        json!(["total_cholesterol", "hdl_c", "bp_systolic", "smoking_status"])
    );
    let (doc_id, src): (Option<uuid::Uuid>, String) =
        sqlx::query_as("select document_id, src_id from observation where profile_id = $1 limit 1")
            .bind(alex)
            .fetch_one(&mut *tx)
            .await?;
    for (group, printed, num, code, unit, when) in [
        (
            "total_cholesterol",
            "212",
            Some("212"),
            None,
            Some("mg/dL"),
            "2026-09-14",
        ),
        ("hdl_c", "52", Some("52"), None, Some("mg/dL"), "2026-09-14"),
        ("bp_systolic", "131", Some("131"), None, Some("mm[Hg]"), "2025-12-01"),
        // an office reading from before 12 months is not read
        ("bp_systolic", "150", Some("150"), None, Some("mm[Hg]"), "2025-06-01"),
        (
            "smoking_status",
            "Never smoked",
            None,
            Some("266919005"),
            None,
            "2026-02-01",
        ),
    ] {
        sqlx::query(
            "insert into observation (profile_id, category, document_id, source_kind, printed_name, printed_value, \
             printed_unit, value_num, value_code, alias_group, unit_ucum, derivation_version, effective_at, src_id, accepted_by, fhir) \
             values ($1, 'vital-signs', $2, 'clinical', $3, $4, $6, $5::numeric, $7, $3, $6, 'test', $8::date + time '08:00', \
             $9, 'import', '{}')",
        )
        .bind(alex)
        .bind(doc_id)
        .bind(group)
        .bind(printed)
        .bind(num)
        .bind(unit)
        .bind(code)
        .bind(when)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
    }
    let doc = calc::document(&mut tx, alex, as_of()).await?;
    let s2 = score2(&doc, "score2");
    assert_eq!(
        (s2["status"].clone(), s2["region"].clone()),
        (json!("ok"), json!("low")),
        "{s2}"
    );
    assert_eq!(s2["inputs"][2]["value"], "131");
    assert_eq!(s2["inputs"][3]["value"], "never");
    assert!(
        s2["src"]
            .as_array()
            .is_some_and(|a| a.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty())))
    );
    let pv = score2(&doc, "prevent_cvd");
    assert_eq!(pv["status"], "ok", "{pv}");
    let line = |g: &str| {
        pv["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["group"] == g)
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(line("statin")["value"], "recorded", "the active atorvastatin");
    assert_eq!(line("statin")["src"], "src_m02");
    assert_eq!(line("egfr")["calc"], "ckd_epi_2021_cr");
    assert!(doc["cardiovascular"].as_array().is_some_and(|v| v.len() == 3));
    // a second read gives the same bytes and hash
    let again = calc::document(&mut tx, alex, as_of()).await?;
    assert_eq!(
        ownhealth_core::canonical::canonical_json(&again)?,
        ownhealth_core::canonical::canonical_json(&doc)?
    );
    assert_eq!(
        calc::calculators_hash(&again)?,
        doc["calculators_hash"].as_str().unwrap_or_default()
    );
    // the hash moves with the facts it read
    assert_ne!(doc["calculators_hash"], missing["calculators_hash"]);
    tx.rollback().await?;
    // reading calculators leaves every state_hash as the fixtures pin it
    let mut tx = s.pool().begin().await?;
    for slug in ["alex", "sam", "lucia", "carmen"] {
        calc::document(&mut tx, s.pid(slug), as_of()).await?;
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        assert_eq!(
            state_hash(&body)?,
            EXPECTED["bodies"][slug]["state_hash"].as_str().unwrap_or_default(),
            "{slug}"
        );
    }
    Ok(())
}

/// No office reading in 12 months: the mean of the last 30 days of wearable daily means, with its window.
#[tokio::test]
async fn without_an_office_reading_the_wearable_mean_is_read_with_its_window() -> R {
    install_fake_genome();
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let alex = s.pid("alex");
    // the seed writes its own source ids, so this one takes the next free id
    let source: i16 = sqlx::query_scalar(
        "insert into wearable_source (id, profile_id, provider, device) overriding system value \
         values ((select coalesce(max(id), 0) + 1 from wearable_source), $1, 'test', 'cuff') returning id",
    )
    .bind(alex)
    .fetch_one(&mut *tx)
    .await?;
    // 31 days ago is outside the window; the last 3 days give a mean of 122
    for (ago, v) in [(31, 180.0_f32), (2, 120.0), (1, 122.0), (0, 124.0)] {
        sqlx::query(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_avg, coverage) \
             values ($1, 'bp_systolic', $2, 'day', $3::date - $4::int, 1, $5, 1)",
        )
        .bind(alex)
        .bind(source)
        .bind(as_of())
        .bind(ago)
        .bind(v)
        .execute(&mut *tx)
        .await?;
    }
    // a second cuff with more days, all older than the window, is not the one chosen: the source is
    // picked from the same 30 days the mean reads
    let old_cuff: i16 = sqlx::query_scalar(
        "insert into wearable_source (id, profile_id, provider, device) overriding system value \
         values ((select coalesce(max(id), 0) + 1 from wearable_source), $1, 'test', 'old cuff') returning id",
    )
    .bind(alex)
    .fetch_one(&mut *tx)
    .await?;
    for ago in 35..45 {
        sqlx::query(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_avg, coverage) \
             values ($1, 'bp_systolic', $2, 'day', $3::date - $4::int, 1, 150, 1)",
        )
        .bind(alex)
        .bind(old_cuff)
        .bind(as_of())
        .bind(ago)
        .execute(&mut *tx)
        .await?;
    }
    let f = cardio::facts(&mut tx, cv(), alex, Some("ES"), as_of(), &all_scopes()).await?;
    // without read:wearables the mean is not read
    let no_wear = scope_set(["read:labs", "read:records", "read:vitals"]);
    let hidden = cardio::facts(&mut tx, cv(), alex, Some("ES"), as_of(), &no_wear).await?;
    assert!(hidden.sbp.is_none() && hidden.withheld.is_empty(), "{:?}", hidden.sbp);
    match f.sbp {
        Some(Sbp::Wearable {
            mean,
            since,
            until,
            days,
            source: from,
        }) => {
            assert_eq!(from, source);
            assert_eq!((mean, days), (d("122.0"), 3));
            assert_eq!(until, as_of());
            assert_eq!(since, as_of() - chrono::Duration::days(29));
        }
        other => panic!("want the wearable mean, got {other:?}"),
    }
    Ok(())
}

/// A smoking status whose code is none of the known answers is refused with its words and source,
/// never read as a non-smoker and never passed over for an older answer.
#[test]
fn an_uncoded_smoking_status_is_refused_with_its_words() -> R {
    let mut f = facts();
    f.smoking = Some(Fact {
        value: cardio::NOT_CODED.into(),
        date: Some(day("2026-06-01")),
        src: "src_smk2".into(),
        printed: Some("Fumador ocasional".into()),
    });
    let items = run(
        "score2",
        &[lipids("5.8", "1.3", "mmol/L")],
        &born(1971, Sex::Male),
        &f,
        &[],
    );
    assert_eq!(items[0]["status"], "refused");
    assert_eq!(
        reason(&items),
        (
            "smoking_status_not_coded".into(),
            json!({"printed": "Fumador ocasional", "on": "2026-06-01"})
        )
    );
    assert!(
        items[0]["src"]
            .as_array()
            .is_some_and(|a| a.contains(&json!("src_smk2")))
    );
    let doc = json!({"schema": calc::SCHEMA, "profile": "x", "as_of": ON, "items": items});
    let es = calc::render(&doc, "es")?;
    assert_eq!(
        es["items"][0]["text"],
        "Sin calcular: el tabaquismo más reciente, del 2026-06-01, dice «Fumador ocasional» sin un código que la ecuación lea (nunca, en el pasado o actualmente)."
    );
    assert_eq!(
        es["items"][0]["inputs"][3]["text"],
        "tabaquismo según el registro: Fumador ocasional"
    );
    let en = calc::render(&doc, "en")?;
    assert_eq!(
        en["items"][0]["inputs"][3]["text"],
        "smoking status as recorded: Fumador ocasional"
    );
    Ok(())
}

/// Without read:records or read:vitals a risk result is not assessed for that scope and shows no
/// input line at all.
#[test]
fn a_caller_without_the_scopes_of_the_facts_gets_no_input_line() {
    let mut f = facts();
    f.conditions.insert("diabetes".into(), fact("recorded", "src_cond"));
    f.drugs.insert("statin".into(), fact("recorded", "src_med"));
    f.withheld = vec!["read:records".into()];
    for id in ["score2", "prevent_cvd"] {
        let items = run(
            id,
            &[lipids("5.8", "1.3", "mmol/L")],
            &born(1971, Sex::Male),
            &f,
            &[egfr("90")],
        );
        assert_eq!(items[0]["status"], "not_assessed");
        assert_eq!(
            reason(&items),
            ("scope_missing".into(), json!({"scopes": ["read:records"]}))
        );
        assert_eq!(
            (items[0]["inputs"].clone(), items[0]["src"].clone()),
            (json!([]), json!([]))
        );
    }
}

/// The newest smoking status counts whatever its code; and facts reads conditions, medicines and
/// vitals only with their scopes.
#[tokio::test]
async fn the_newest_smoking_status_counts_and_facts_read_only_under_their_scopes() -> R {
    install_fake_genome();
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let alex = s.pid("alex");
    let (doc_id, src): (Option<uuid::Uuid>, String) =
        sqlx::query_as("select document_id, src_id from observation where profile_id = $1 limit 1")
            .bind(alex)
            .fetch_one(&mut *tx)
            .await?;
    for (printed, code, when) in [
        ("Never smoked", Some("266919005"), "2020-02-01"),
        ("Occasional cigarette smoker", Some("230059006"), "2026-02-01"),
    ] {
        sqlx::query(
            "insert into observation (profile_id, category, document_id, source_kind, printed_name, printed_value, \
             value_code, alias_group, derivation_version, effective_at, src_id, accepted_by, fhir) \
             values ($1, 'survey', $2, 'clinical', 'Smoking status', $3, $4, 'smoking_status', 'test', \
             $5::date + time '08:00', $6, 'import', '{}')",
        )
        .bind(alex)
        .bind(doc_id)
        .bind(printed)
        .bind(code)
        .bind(when)
        .bind(&src)
        .execute(&mut *tx)
        .await?;
    }
    let f = cardio::facts(&mut tx, cv(), alex, Some("ES"), as_of(), &all_scopes()).await?;
    let smoking = f.smoking.expect("a smoking status");
    assert_eq!(
        (smoking.value.as_str(), smoking.printed.as_deref(), smoking.date),
        (
            cardio::NOT_CODED,
            Some("Occasional cigarette smoker"),
            Some(day("2026-02-01"))
        )
    );
    assert!(f.drugs.contains_key("statin"), "the control reads the statin");
    let labs_vitals = scope_set(["read:summary", "read:labs", "read:vitals"]);
    let hidden = cardio::facts(&mut tx, cv(), alex, Some("ES"), as_of(), &labs_vitals).await?;
    assert_eq!(hidden.withheld, vec!["read:records".to_string()]);
    assert!(
        hidden.smoking.is_none() && hidden.sbp.is_none() && hidden.drugs.is_empty() && hidden.conditions.is_empty()
    );
    let doc = calc::document_for(&mut tx, alex, as_of(), &labs_vitals).await?;
    assert!(!doc.to_string().contains("src_m02"), "{doc}");
    Ok(())
}

/// A report's draw date is its newest value, so a report with old lipids and a new creatinine sorts
/// after a newer lipid draw: the pair and the LDL-C are taken by their own dates.
#[test]
fn the_newest_lipids_count_by_their_own_date_not_by_their_report() {
    let mut newer = lipids("5.8", "1.3", "mmol/L");
    let mut old = Draw {
        date: Some(day("2026-11-01")),
        obs: [
            (
                "total_cholesterol".to_string(),
                obs("total_cholesterol", "6.5", "mmol/L", "2026-01-10", "src_tc_old"),
            ),
            (
                "hdl_c".to_string(),
                obs("hdl_c", "1.0", "mmol/L", "2026-01-10", "src_hdl_old"),
            ),
            (
                "creatinine".to_string(),
                obs("creatinine", "0.9", "mg/dL", "2026-11-01", "src_cr_new"),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let it = &run(
        "score2",
        &[newer.clone(), old.clone()],
        &born(1971, Sex::Male),
        &facts(),
        &[],
    )[0];
    assert_eq!(it["status"], "ok", "{it}");
    assert_eq!(it["inputs"][0]["src"], "src_tc");
    assert_eq!(it["inputs"][1]["src"], "src_hdl");
    // the newer LDL-C of 5.2 mmol/L is the one the marked-factor rule reads
    newer.obs.insert(
        "ldl_c".into(),
        obs("ldl_c", "5.2", "mmol/L", "2026-09-14", "src_ldl_new"),
    );
    old.obs.insert(
        "ldl_c".into(),
        obs("ldl_c", "2.0", "mmol/L", "2026-01-10", "src_ldl_old"),
    );
    let items = run("score2", &[newer, old], &born(1971, Sex::Male), &facts(), &[]);
    assert_eq!(reason(&items).0, "marked_risk_factor", "{items:?}");
}
