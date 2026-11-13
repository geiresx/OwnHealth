//! The Genome screen (v0.3): a PGx call made against an older CPIC or PharmCAT release reads Stale
//! with the OwnGenome steps to re-run, a PRS outside its validated coverage or ancestry has no
//! percentile, a rare P/LP from a chip carries the cited PPV note, and the CPIC gene grid answers
//! every gene with its call or why there is none.

mod common;

use chrono::NaiveDate;
use common::{R, env, fact_mut};
use ownhealth_genome::findings::{CpicChange, PgxReleases, Release, pgx_grid};
use ownhealth_genome::{FindingsQuery, Gate, list_findings_typed, pgx_for_drug, synthetic};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or_default()
}

fn e4() -> FindingsQuery<'static> {
    FindingsQuery {
        min_evidence: Some("E4"),
        ..Default::default()
    }
}

fn finding<'a>(found: &'a [Value], gene: &str) -> &'a Value {
    found
        .iter()
        .find(|f| f["gene"] == gene)
        .unwrap_or_else(|| panic!("no {gene} finding"))
}

/// The plan's case: a call made against CPIC 2025-01, a TPMT change in 2026-02, nothing for CYP2C19.
#[test]
fn a_cpic_change_after_the_call_marks_only_its_gene_stale() {
    let rel = PgxReleases {
        cpic_changes: vec![
            CpicChange {
                date: "2026-02-03".into(),
                genes: vec!["TPMT".into()],
            },
            CpicChange {
                date: "2024-11-20".into(),
                genes: vec!["CYP2C19".into()],
            },
        ],
        pharmcat: Some(Release {
            version: "3.4.0".into(),
            released: "2026-07-14".into(),
        }),
        rerun_steps: vec![36, 7, 27],
    };
    let dbv = json!({"cpic": "2025-01", "pharmcat": "3.4.0"});
    let on = day(2026, 11, 7);
    assert_eq!(
        rel.stale("TPMT", &dbv, on),
        Some(json!({"since": "2026-02-03", "by": ["CPIC 2026-02-03"], "rerun_steps": [36, 7, 27]}))
    );
    assert_eq!(rel.stale("CYP2C19", &dbv, on), None);
    // a change inside the month the call was made against is not newer than it
    assert_eq!(
        rel.stale("TPMT", &json!({"cpic": "2026-02", "pharmcat": "3.4.0"}), on),
        None
    );
    // nor one the reader's as_of has not reached yet
    assert_eq!(rel.stale("TPMT", &dbv, day(2026, 1, 31)), None);
    // an older PharmCAT minor makes every gene stale, from the day that release came out
    let old = json!({"cpic": "2026-09", "pharmcat": "3.1.0"});
    assert_eq!(
        rel.stale("CYP2C19", &old, on),
        Some(json!({"since": "2026-07-14", "by": ["PharmCAT 3.4.0"], "rerun_steps": [36, 7, 27]}))
    );
    assert_eq!(rel.stale("CYP2C19", &old, day(2026, 7, 13)), None);
    // a patch release is not a new minor; an unreadable version is never called stale
    assert_eq!(
        rel.stale("CYP2C19", &json!({"cpic": "2026-09", "pharmcat": "v3.4.2"}), on),
        None
    );
    assert_eq!(
        rel.stale("CYP2C19", &json!({"cpic": "latest", "pharmcat": "dev"}), on),
        None
    );
}

/// The shipped files load: the CPIC change log, the latest PharmCAT release and the steps to re-run.
#[test]
fn the_shipped_release_data_loads() {
    let rel = PgxReleases::shipped();
    assert!(rel.cpic_changes.len() > 100, "{}", rel.cpic_changes.len());
    assert!(rel.cpic_changes.iter().any(|c| c.genes.iter().any(|g| g == "TPMT")));
    assert!(rel.pharmcat.as_ref().is_some_and(|p| p.version.starts_with('3')));
    assert_eq!(rel.rerun_steps, vec![36, 7, 27]);
}

#[tokio::test]
async fn findings_and_check_drug_say_stale_with_the_steps_to_rerun() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let pharmcat = PgxReleases::shipped()
        .pharmcat
        .as_ref()
        .map(|p| p.version.clone())
        .unwrap_or_default();
    // the control: called against the snapshot's own CPIC date and the latest PharmCAT, nothing is stale
    let mut facts = synthetic::alex_facts();
    facts["provenance"]["db_versions"]["cpic"] = json!("2026-11-07");
    facts["provenance"]["db_versions"]["pharmcat"] = json!(pharmcat);
    let current = env().await?;
    let mut cc = current.conn().await?;
    current.seed(&mut cc, "alex", &facts, &[]).await?;
    let found = list_findings_typed(&mut cc, current.profile("alex"), &Gate::new(true, false), &e4()).await?;
    assert!(
        found.findings.iter().all(|f| f["stale"].is_null()),
        "{:?}",
        found.findings
    );
    // the same file called against CPIC 2025-01: TPMT changed in CPIC since
    facts["provenance"]["db_versions"]["cpic"] = json!("2025-01");
    let alex = e.profile("alex");
    e.seed(&mut c, "alex", &facts, &[]).await?;
    let found = list_findings_typed(&mut c, alex, &Gate::new(true, false), &e4()).await?;
    let tpmt = &finding(&found.findings, "TPMT")["stale"];
    assert_eq!(tpmt["rerun_steps"], json!([36, 7, 27]), "{tpmt}");
    assert!(
        tpmt["by"][0].as_str().unwrap_or_default().starts_with("CPIC 20"),
        "{tpmt}"
    );
    // only PGx calls go stale
    assert_eq!(finding(&found.findings, "CFTR")["stale"], Value::Null);
    let aza = pgx_for_drug(&mut c, alex, "RxNorm:1256", None).await?;
    let row = aza.iter().find(|r| r["gene"] == "TPMT").cloned().unwrap_or_default();
    assert_eq!(row["stale"], *tpmt);
    Ok(())
}

#[tokio::test]
async fn a_prs_outside_its_coverage_or_ancestry_has_no_percentile() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let alex = e.profile("alex");
    let mut facts = synthetic::alex_facts();
    let prs = |facts: &mut Value| -> Value {
        let at = facts["facts"]
            .as_array_mut()
            .into_iter()
            .flatten()
            .find(|f| f["category"] == "prs");
        at.cloned().unwrap_or_default()
    };
    assert_eq!(prs(&mut facts)["value"]["percentile"], 86);
    e.seed(&mut c, "alex", &facts, &[]).await?;
    let found = list_findings_typed(&mut c, alex, &Gate::new(true, false), &e4()).await?;
    let ok = found
        .findings
        .iter()
        .find(|f| f["category"] == "prs")
        .cloned()
        .unwrap_or_default();
    assert_eq!(ok["prs"], json!({"status": "interpretable", "reason_code": null}));
    assert_eq!(ok["value"]["percentile"], 86);

    for (field, value, code) in [
        ("matched_pct", json!(70.0), "low_variant_match"),
        ("evaluated_in", json!(["EAS"]), "ancestry_outside_evaluation"),
    ] {
        let e = env().await?;
        let mut c = e.conn().await?;
        let mut facts = synthetic::alex_facts();
        for f in facts["facts"].as_array_mut().into_iter().flatten() {
            if f["category"] == "prs" {
                f["value"][field] = value.clone();
            }
        }
        e.seed(&mut c, "alex", &facts, &[]).await?;
        let found = list_findings_typed(&mut c, e.profile("alex"), &Gate::new(true, false), &e4()).await?;
        let f = found
            .findings
            .iter()
            .find(|f| f["category"] == "prs")
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            f["prs"],
            json!({"status": "not_interpretable", "reason_code": code}),
            "{field}"
        );
        let text = serde_json::to_string(&f)?;
        // the number is gone everywhere, the producer's own sentence that quotes it too
        assert!(!text.contains("86") && !text.contains("percentile"), "{field}: {text}");
        assert_eq!(f["value"]["pgs_id"], "PGS000018");
    }
    Ok(())
}

#[tokio::test]
async fn a_rare_plp_from_a_chip_carries_the_cited_ppv_note() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let sam = e.profile("sam");
    e.seed(&mut c, "sam", &synthetic::chip_with_rare_brca2(), &[]).await?;
    let found = list_findings_typed(&mut c, sam, &Gate::new(true, false), &e4()).await?;
    let brca2 = finding(&found.findings, "BRCA2");
    assert_eq!(brca2["confidence"], "unconfirmed");
    assert_eq!(
        brca2["chip_note"],
        json!({"code": "chip_rare_unconfirmed", "ppv_pct": 16, "cite": "https://pubmed.ncbi.nlm.nih.gov/33589468/"})
    );
    // a common PGx call from the same chip is not a rare P/LP: no note
    assert_eq!(finding(&found.findings, "CYP2C19")["chip_note"], Value::Null);
    let chip = found
        .limitations
        .iter()
        .find(|l| l.starts_with("Chip"))
        .cloned()
        .unwrap_or_default();
    assert!(chip.contains("16%") && chip.contains("PMID 33589468"), "{chip}");
    Ok(())
}

#[tokio::test]
async fn the_grid_answers_every_cpic_gene_with_its_call_or_why_not() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let mut facts = synthetic::alex_facts();
    // FMR1 is not a CPIC gene; drop NUDT15 from the coverage map and mark UGT1A1 not assessed
    let cov: Vec<Value> = facts["coverage"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["id"] != "NUDT15")
        .map(|c| {
            let mut c = c.clone();
            if c["id"] == "UGT1A1" {
                c = json!({"scope": "gene", "id": "UGT1A1", "panel": "cpic", "state": "not_assessed", "reason": "not_run"});
            }
            c
        })
        .collect();
    facts["coverage"] = json!(cov);
    fact_mut(&mut facts, "TPMT")["value"]["diplotype"] = json!("*1/*1");
    let alex = e.profile("alex");
    e.seed(&mut c, "alex", &facts, &[]).await?;
    let found = list_findings_typed(&mut c, alex, &Gate::new(false, false), &e4()).await?;
    let mut meds = BTreeMap::new();
    meds.insert(
        "CYP2C19".to_string(),
        vec![json!({"name": "Omeprazole 20 mg", "src": "src_med"})],
    );
    let grid = pgx_grid(&found, Some(&meds));
    let row = |g: &str| grid.iter().find(|r| r["gene"] == g).cloned().unwrap_or_default();
    let genes: Vec<&str> = grid.iter().filter_map(|r| r["gene"].as_str()).collect();
    for g in [
        "CYP2C19", "CYP2D6", "TPMT", "NUDT15", "UGT1A1", "HLA-A", "HLA-B", "VKORC1",
    ] {
        assert!(genes.contains(&g), "{g} missing from {genes:?}");
    }
    let cyp = row("CYP2C19");
    assert_eq!(
        (
            &cyp["status"],
            &cyp["diplotype"],
            &cyp["phenotype"],
            &cyp["call_state"],
            &cyp["actionable"]
        ),
        (
            &json!("found"),
            &json!("*1/*2"),
            &json!("Intermediate Metabolizer"),
            &json!("alt"),
            &json!(true)
        )
    );
    assert_eq!(cyp["meds"], json!([{"name": "Omeprazole 20 mg", "src": "src_med"}]));
    assert!(cyp["src"].as_str().is_some_and(|s| s.starts_with("src_")), "{cyp}");
    assert_eq!(row("TPMT")["actionable"], false);
    assert_eq!(row("TPMT")["meds"], json!([]));
    assert_eq!(
        row("HLA-B")["markers"],
        json!([{"marker": "HLA-B*58:01", "present": false}])
    );
    // assessed with no finding, not assessed with its reason, absent from the coverage map
    assert_eq!(
        (&row("CYP3A5")["status"], &row("CYP3A5")["coverage"]["state"]),
        (&json!("no_finding"), &json!("assessed"))
    );
    assert_eq!(
        (&row("UGT1A1")["status"], &row("UGT1A1")["coverage"]["reason"]),
        (&json!("not_assessed"), &json!("not_run"))
    );
    assert_eq!(
        (&row("NUDT15")["status"], &row("NUDT15")["coverage"]["reason"]),
        (&json!("not_assessed"), &json!("not_in_coverage_map"))
    );
    // without read:records the meds are withheld, not empty
    assert_eq!(pgx_grid(&found, None)[0]["meds"], Value::Null);
    Ok(())
}
