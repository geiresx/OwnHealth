//! Rule pack 0.2.0: every RCV cites the EuBIVAS paper its CVs come from, a change is never judged
//! across a lab or method switch, a value outside the person's own usual level is flagged, and the
//! drug check reads the FDA inhibitor table.

mod common;

use std::collections::BTreeMap;

use chrono::NaiveDate;
use common::{as_of, seeded};
use ownhealth_core::access::all_scopes;
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_rules::engine::{Point, baseline, baseline_band, rcv, rcv_percent, trend};
use ownhealth_rules::pack::{build_pack, shipped_files};
use ownhealth_rules::{build_state, check_drug, load_pack, render};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use uuid::Uuid;

type R = anyhow::Result<()>;

fn dec(s: &str) -> Decimal {
    s.parse().unwrap_or_default()
}

fn pt(day: &str, value: &str, lab: Option<&str>, method: Option<&str>) -> Point {
    Point {
        id: Uuid::new_v4(),
        date: NaiveDate::parse_from_str(day, "%Y-%m-%d").unwrap_or_default(),
        value: Some(dec(value)),
        canonical: Some(dec(value)),
        canonical_unit: Some("mg/dL".into()),
        printed_value: value.into(),
        unit: Some("mg/dL".into()),
        unit_ucum: Some("mg/dL".into()),
        ref_text: None,
        ref_low: None,
        ref_high: None,
        flag: None,
        loinc: Some("13457-7".into()),
        method: method.map(str::to_string),
        lab: lab.map(str::to_string),
        src: format!("src_{day}"),
        src_label: None,
        src_kind: "document_region".into(),
        derivation: "test".into(),
    }
}

fn rule(id: &str) -> anyhow::Result<Map<String, Value>> {
    load_pack()?
        .rule(id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no rule {id}"))
}

#[test]
fn the_pack_is_0_2_0_and_every_variation_number_cites_a_paper() -> R {
    let p = load_pack()?;
    assert_eq!(p.version, "ownhealth-rules 0.2.0");
    let rcv_groups: Vec<String> = p
        .rules_of("rcv")
        .map(|r| r["match"][0].as_str().unwrap_or_default().to_string())
        .collect();
    // HbA1c and ferritin have no EuBIVAS estimate we may ship
    assert_eq!(rcv_groups, ["ldl_c", "crp_hs", "creatinine", "alt", "tsh"]);
    for r in p.rules_of("rcv").chain(p.rules_of("baseline")) {
        let cite = r["cite"].as_str().unwrap_or_default();
        assert!(cite.starts_with("https://doi.org/"), "{}: {cite}", r["id"]);
        assert!(!cite.contains("biologicalvariation.eu"), "{}", r["id"]);
    }
    for r in p.rules_of("rcv") {
        assert!(
            r["primary"].as_str().is_some_and(|d| d.starts_with("https://doi.org/")),
            "{}",
            r["id"]
        );
    }
    Ok(())
}

#[test]
fn a_rcv_rule_without_a_doi_does_not_load() -> R {
    let mut files: BTreeMap<String, Value> = shipped_files()?;
    let liver = files
        .get_mut("liver.yaml")
        .ok_or_else(|| anyhow::anyhow!("liver.yaml"))?;
    liver["rules"][1]["cite"] = json!("https://biologicalvariation.eu/");
    assert_eq!(liver["rules"][1]["id"], "rcv.alt");
    let err = build_pack(&files, true)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("rcv.alt") && err.contains("DOI"), "{err}");
    // the baseline rule's z and formula come from a paper too
    let mut files: BTreeMap<String, Value> = shipped_files()?;
    let pack = files.get_mut("pack.yaml").ok_or_else(|| anyhow::anyhow!("pack.yaml"))?;
    assert_eq!(pack["rules"][1]["id"], "baseline.personal");
    pack["rules"][1]["cite"] = json!("https://example.org/");
    let err = build_pack(&files, true)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("baseline.personal") && err.contains("DOI"), "{err}");
    Ok(())
}

#[test]
fn the_rcv_follows_its_cited_cvs() -> R {
    let ldl = rule("rcv.ldl_c")?;
    // 1.96 * sqrt(2) * sqrt(8.46^2 + 1.77^2) = 23.96
    assert_eq!(rcv_percent(&ldl), "24.0");
    let mut moved = ldl.clone();
    moved.insert("cv_i".into(), json!("9.46"));
    assert_eq!(rcv_percent(&moved), "26.7");
    Ok(())
}

#[test]
fn a_change_inside_one_lab_is_judged_and_a_lab_or_method_switch_is_not() -> R {
    let ldl = rule("rcv.ldl_c")?;
    let lab = Some("LabNorte");
    let same = [pt("2026-01-10", "100", lab, None), pt("2026-06-10", "130", lab, None)];
    let flag = rcv(&ldl, &same).ok_or_else(|| anyhow::anyhow!("no flag"))?;
    assert_eq!(
        (&flag["change_pct"], &flag["rcv_pct"]),
        (&json!("30.0"), &json!("24.0"))
    );
    let other_lab = [
        pt("2026-01-10", "100", lab, None),
        pt("2026-06-10", "130", Some("Hospital Norte"), None),
    ];
    assert!(rcv(&ldl, &other_lab).is_none());
    let other_method = [
        pt("2026-01-10", "100", lab, Some("calculated")),
        pt("2026-06-10", "130", lab, Some("direct")),
    ];
    assert!(rcv(&ldl, &other_method).is_none());
    // case and spacing are not a switch
    let spelled = [
        pt("2026-01-10", "100", lab, None),
        pt("2026-06-10", "130", Some(" labnorte "), None),
    ];
    assert!(rcv(&ldl, &spelled).is_some());
    Ok(())
}

/// What docs/calculators/rcv.md says about a missing lab or method: a missing value counts as one
/// more value, so two missing ones match and a missing one never matches a present one.
#[test]
fn a_missing_lab_or_method_counts_as_one_more_value() -> R {
    let ldl = rule("rcv.ldl_c")?;
    let no_lab = [pt("2026-01-10", "100", None, None), pt("2026-06-10", "130", None, None)];
    assert!(rcv(&ldl, &no_lab).is_some());
    let one_lab = [
        pt("2026-01-10", "100", None, None),
        pt("2026-06-10", "130", Some("LabNorte"), None),
    ];
    assert!(rcv(&ldl, &one_lab).is_none());
    // 2089-1 has no method in the shipped LOINC table
    let uncoded = |day, value, method: Option<&str>| {
        let mut p = pt(day, value, Some("LabNorte"), method);
        p.loinc = Some("2089-1".into());
        p
    };
    let no_method = [uncoded("2026-01-10", "100", None), uncoded("2026-06-10", "130", None)];
    assert!(rcv(&ldl, &no_method).is_some());
    let one_method = [
        uncoded("2026-01-10", "100", None),
        uncoded("2026-06-10", "130", Some("direct")),
    ];
    assert!(rcv(&ldl, &one_method).is_none());
    Ok(())
}

/// mg/dL (LOINC 2160-0) and umol/L (14682-9) are the same creatinine measurement: a lab that prints
/// SI units on one report and conventional on the next has not changed its method.
#[test]
fn a_unit_change_alone_is_not_a_method_change() -> R {
    let crea = rule("rcv.creatinine")?;
    let lab = Some("LabNorte");
    let mut a = pt("2026-01-10", "0.81", lab, None);
    a.loinc = Some("2160-0".into());
    let mut b = pt("2026-06-10", "0.99525", lab, None);
    (b.loinc, b.printed_value, b.unit, b.unit_ucum) = (
        Some("14682-9".into()),
        "88".into(),
        Some("umol/L".into()),
        Some("umol/L".into()),
    );
    b.value = Some(dec("88"));
    let flag = rcv(&crea, &[a.clone(), b.clone()]).ok_or_else(|| anyhow::anyhow!("no flag"))?;
    assert_eq!(flag["reason"], "RCV_CHANGE", "{flag:?}");
    // calculated and direct LDL are still two methods, whatever codes name them
    let (mut calc, mut direct) = (pt("2026-01-10", "100", lab, None), pt("2026-06-10", "130", lab, None));
    calc.loinc = Some("39469-2".into());
    direct.loinc = Some("18262-6".into());
    assert!(rcv(&rule("rcv.ldl_c")?, &[calc, direct]).is_none());
    Ok(())
}

#[test]
fn a_trend_runs_inside_one_lab() -> R {
    let up = rule("trend.hba1c.rise")?;
    let lab = Some("LabNorte");
    let one_lab = [
        pt("2024-01-10", "5.4", lab, None),
        pt("2025-01-10", "5.6", lab, None),
        pt("2026-01-10", "5.9", lab, None),
    ];
    assert!(trend(&up, &one_lab).is_some());
    let switched = [
        pt("2024-01-10", "5.4", lab, None),
        pt("2025-01-10", "5.6", Some("Hospital Norte"), None),
        pt("2026-01-10", "5.9", lab, None),
    ];
    assert!(trend(&up, &switched).is_none());
    Ok(())
}

#[test]
fn the_baseline_band_is_the_formula() -> R {
    let base = rule("baseline.personal")?;
    let crea = rule("rcv.creatinine")?;
    let band = baseline_band(&base, &crea, &[dec("0.80"), dec("0.81"), dec("0.90")])
        .ok_or_else(|| anyhow::anyhow!("no band"))?;
    // set point 0.8367; half width 0.8367 * 1.96 * sqrt(4.4^2 + 1.1^2) % * sqrt(1 + 1/3) = 0.0859
    assert_eq!(band.n, 3);
    assert_eq!(band.set_point.round_dp(4), dec("0.8367"));
    assert_eq!(
        (band.low.round_dp(3), band.high.round_dp(3)),
        (dec("0.751"), dec("0.923"))
    );
    assert!(baseline_band(&base, &crea, &[dec("0.80"), dec("0.81")]).is_none());
    // the set point is the mean of the latest five at most
    let seven: Vec<Decimal> = ["9", "9", "1", "1", "1", "1", "1"].iter().map(|v| dec(v)).collect();
    let b = baseline_band(&base, &crea, &seven).ok_or_else(|| anyhow::anyhow!("no band"))?;
    assert_eq!((b.n, b.set_point), (5, Decimal::ONE));
    // a rule whose max is under its min never gives a band from fewer than min points
    let mut odd = base.clone();
    odd.insert("max_points".into(), json!(2));
    assert!(baseline_band(&odd, &crea, &[dec("0.80"), dec("0.81"), dec("0.90")]).is_none());
    // only same-method earlier points count
    let lab = Some("LabNorte");
    let pts = [
        pt("2026-01-10", "0.80", lab, None),
        pt("2026-02-10", "0.81", lab, None),
        pt("2026-03-10", "0.90", Some("Hospital Norte"), None),
        pt("2026-04-10", "0.98", lab, None),
    ];
    assert!(baseline(&base, &crea, &pts).is_none());
    Ok(())
}

async fn manual_creatinine(conn: &mut sqlx::PgConnection, pid: Uuid, day: &str, value: &str) -> R {
    let o = ObservationIn::from_json(json!({
        "profile_id": pid, "printed_name": "Creatinina", "printed_value": value, "printed_unit": "mg/dL",
        "printed_ref_range": "0.5 - 1.1", "value_num": value, "ref_low": "0.5", "ref_high": "1.1",
        "loinc": "2160-0", "alias_group": "creatinine", "unit_ucum": "mg/dL", "value_canonical": value,
        "unit_canonical": "mg/dL", "derivation_version": "test", "effective_at": format!("{day}T08:00:00+00:00"),
    }))?;
    commit_observation(
        conn,
        &o,
        &SrcRef::New(SrcIn::new("manual")),
        CommitOptions::new("person"),
    )
    .await?;
    Ok(())
}

fn flag_for<'a>(body: &'a Value, group: &str) -> Option<&'a Value> {
    body["flags"].as_array()?.iter().find(|f| f["group"] == group)
}

/// Carmen's eGFR fell from 64 to 58; the creatinine behind it drifts up, each step inside the RCV.
#[tokio::test]
async fn carmen_creatinine_drift_is_outside_her_usual_level() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    for (day, v) in [
        ("2026-08-05", "0.80"),
        ("2026-08-19", "0.81"),
        ("2026-09-02", "0.90"),
        ("2026-09-16", "0.98"),
    ] {
        manual_creatinine(&mut tx, carmen, day, v).await?;
    }
    let body = build_state(&mut tx, carmen, as_of(), None).await?;
    let f = flag_for(&body, "creatinine").cloned().unwrap_or(Value::Null);
    assert_eq!(
        (
            &f["reason"],
            &f["rule"],
            &f["value"],
            &f["low"],
            &f["high"],
            &f["set_point"]
        ),
        (
            &json!("BASELINE_OUT"),
            &json!("baseline.personal"),
            &json!("0.98"),
            &json!("0.75"),
            &json!("0.92"),
            &json!("0.84")
        ),
        "{f}"
    );
    // three earlier results and this one, never the LabNorte value from another lab
    assert_eq!(f["src"].as_array().map(Vec::len), Some(4), "{f}");
    assert_eq!(body["systems"]["kidney"], "out"); // eGFR 58 is under the lab's range
    let text = render(&body, "es", "as_reported", "full", None)?;
    let shown = text["data"]["flags"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["rule"] == "baseline.personal"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        shown["text"],
        "Creatinina 0,98 mg/dL el 2026-09-16 queda fuera de su nivel habitual, de 0,75 a 0,92 mg/dL, según la \
         media de 3 resultados anteriores del mismo laboratorio y método."
    );
    assert_eq!((&shown["kind"], &shown["points"]), (&json!("baseline"), &json!(3)));
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn the_baseline_stays_silent_with_two_earlier_results() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    for (day, v) in [("2026-08-19", "0.81"), ("2026-09-02", "0.90"), ("2026-09-16", "0.98")] {
        manual_creatinine(&mut tx, carmen, day, v).await?;
    }
    let body = build_state(&mut tx, carmen, as_of(), None).await?;
    assert!(flag_for(&body, "creatinine").is_none(), "{}", body["flags"]);
    tx.rollback().await?;
    Ok(())
}

/// The same lab prints creatinine in mg/dL three times and then in umol/L: the stored canonical values
/// compare, so a rise above the RCV is still a change.
#[tokio::test]
async fn a_rise_printed_in_si_units_is_still_a_change() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    for (day, v) in [("2026-08-05", "0.80"), ("2026-08-19", "0.82"), ("2026-09-02", "0.81")] {
        manual_creatinine(&mut tx, carmen, day, v).await?;
    }
    let o = ObservationIn::from_json(json!({
        "profile_id": carmen, "printed_name": "Creatinina", "printed_value": "88", "printed_unit": "umol/L",
        "printed_ref_range": "44 - 97", "value_num": "88", "ref_low": "44", "ref_high": "97",
        "loinc": "14682-9", "alias_group": "creatinine", "unit_ucum": "umol/L", "value_canonical": "0.99525",
        "unit_canonical": "mg/dL", "derivation_version": "test", "effective_at": "2026-09-16T08:00:00+00:00",
    }))?;
    commit_observation(
        &mut tx,
        &o,
        &SrcRef::New(SrcIn::new("manual")),
        CommitOptions::new("person"),
    )
    .await?;
    let body = build_state(&mut tx, carmen, as_of(), None).await?;
    let f = flag_for(&body, "creatinine").cloned().unwrap_or(Value::Null);
    assert_eq!(
        (&f["reason"], &f["rule"]),
        (&json!("RCV_CHANGE"), &json!("rcv.creatinine")),
        "{f}"
    );
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn piperine_shows_as_a_cyp2c9_inhibitor_on_a_warfarin_check() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let rec = RecordIn::from_json(
        "medication",
        json!({"profile_id": s.pid("alex"), "kind": "supplement", "name": "Piperine", "strength": "5 mg",
               "started": "2026-09-01"}),
    )?;
    add_record(
        &mut tx,
        &rec,
        &SrcRef::New(SrcIn::new("manual")),
        RecordOptions::new("person"),
    )
    .await?;
    let out = check_drug(&mut tx, s.pid("alex"), "warfarina", &all_scopes(), "en", Some(as_of())).await?;
    let piperine = out["current_medications_relevant"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["name"] == "Piperine 5 mg"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        (&piperine["kind"], &piperine["relation"], &piperine["relation_text"]),
        (
            &json!("supplement"),
            &json!("inhibitor:CYP2C9:moderate"),
            &json!("CYP2C9 inhibitor (moderate)")
        ),
        "{out}"
    );
    // omeprazole left the FDA's CYP2C19 list: on a clopidogrel check it is not assessed, and Alex's
    // cenobamate (moderate, added to the list) is the inhibitor named
    let rec = RecordIn::from_json(
        "medication",
        json!({"profile_id": s.pid("alex"), "name": "Omeprazole", "strength": "20 mg", "started": "2026-09-01"}),
    )?;
    add_record(
        &mut tx,
        &rec,
        &SrcRef::New(SrcIn::new("manual")),
        RecordOptions::new("person"),
    )
    .await?;
    let out = check_drug(
        &mut tx,
        s.pid("alex"),
        "clopidogrel",
        &all_scopes(),
        "en",
        Some(as_of()),
    )
    .await?;
    let pcs = out["phenoconversion"].as_array().cloned().unwrap_or_default();
    assert_eq!(pcs.len(), 1, "{out}");
    assert_eq!(
        (&pcs[0]["gene"], &pcs[0]["by"], &pcs[0]["inhibitor_strength"]),
        (&json!("CYP2C19"), &json!("Cenobamate 50 mg"), &json!("moderate")),
        "{out}"
    );
    let omeprazole = out["current_medications_relevant"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["name"] == "Omeprazole 20 mg"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(omeprazole["relation"], "not_assessed", "{out}");
    tx.rollback().await?;
    Ok(())
}
