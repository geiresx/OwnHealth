//! check_drug (0.1.1 tests/state/test_drug_check.py): PGx, current medications, relevant labs and
//! CYP2D6 phenoconversion, with CPIC text quoted verbatim.

mod common;

use std::collections::BTreeSet;

use common::{as_of, seeded};
use ownhealth_core::Error;
use ownhealth_core::access::{all_scopes, scope_set};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{MedicationIn, RecordIn, RecordOptions, add_record};
use ownhealth_rules::drugs::band;
use ownhealth_rules::{check_drug, cpic_snapshot, load_pack, resolve_drug};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

type R = anyhow::Result<()>;

const CLOPIDOGREL_IM_ACS: &str = "Avoid standard dose (75 mg) clopidogrel if possible. Use prasugrel or ticagrelor at \
                                  standard dose if no contraindication.";
const CODEINE_NM: &str = "Use codeine label recommended age- or weight-specific dosing.";
const CODEINE_PM: &str = "Avoid codeine use because of possibility of diminished analgesia. If opioid use is warranted, \
                          consider a non-tramadol opioid.";

async fn med(conn: &mut PgConnection, pid: Uuid, name: &str) -> anyhow::Result<String> {
    let m: MedicationIn = serde_json::from_value(json!({
        "profile_id": pid, "name": name, "ingredient": name.to_lowercase(), "strength": "20 mg", "started": "2026-04-01",
    }))?;
    let src = SrcIn {
        label: Some(format!("Rx · {name}")),
        ..SrcIn::new("manual")
    };
    Ok(add_record(
        conn,
        &RecordIn::Medication(m),
        &SrcRef::New(src),
        RecordOptions::new("person"),
    )
    .await?
    .src_id)
}

fn find<'a>(out: &'a Value, key: &str, val: &str) -> &'a Value {
    out["guidance"]
        .as_array()
        .and_then(|g| g.iter().find(|g| g[key] == val))
        .unwrap_or(&Value::Null)
}

#[tokio::test]
async fn alex_clopidogrel() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = check_drug(&mut tx, s.pid("alex"), "Plavix", &all_scopes(), "en", Some(as_of())).await?;
    assert_eq!(
        (&out["drug"]["name"], &out["drug"]["cpic_drug_id"]),
        (&json!("Clopidogrel"), &json!("RxNorm:32968"))
    );
    assert_eq!(out["drug"]["currently_taking"], false);
    assert_eq!(
        out["pgx"],
        json!([{"gene": "CYP2C19", "diplotype": "*1/*2", "phenotype": "Intermediate Metabolizer", "activity_score": null,
                "evidence": "E1", "confidence": "high", "call_state": "alt", "cpic_level": "A", "src": "src_g07"}])
    );
    let acs = find(&out, "population", "CVI ACS PCI");
    assert_eq!(acs["quote"], CLOPIDOGREL_IM_ACS);
    assert_eq!(
        (&acs["source"], &acs["level"], &acs["strength"], &acs["license"]),
        (&json!("CPIC"), &json!("A"), &json!("Strong"), &json!("CC0-1.0"))
    );
    assert!(acs["url"].as_str().is_some_and(|u| u.starts_with("https://")) && acs["based_on"] == json!(["src_g07"]));
    assert_eq!(find(&out, "population", "NVI")["strength"], "Moderate");
    // omeprazole is a moderate CYP2C19 inhibitor; CPIC has no conversion rule for CYP2C19
    let pc = &out["phenoconversion"][0];
    assert_eq!(
        out["phenoconversion"],
        json!([{"gene": "CYP2C19", "by": "Omeprazole 20 mg", "inhibitor_strength": "moderate", "computed": false,
                "functional_phenotype": null, "activity_score": null, "activity_score_functional": null, "rule": null,
                "basis": pc["basis"], "src": "src_m05", "text": pc["text"]}])
    );
    assert!(pc["text"].as_str().is_some_and(|t| t.contains("not computed")));
    let rel: Vec<_> = out["current_medications_relevant"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            (
                m["name"].as_str().unwrap_or_default(),
                m["relation"].as_str().unwrap_or_default(),
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        rel,
        [
            ("Atorvastatin 20 mg", "not_assessed"),
            ("Omeprazole 20 mg", "inhibitor:CYP2C19:moderate")
        ]
    );
    let labs = out["relevant_labs"].as_array().cloned().unwrap_or_default();
    let groups: Vec<_> = labs
        .iter()
        .map(|l| l["alias_group"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(groups, ["platelets", "hemoglobin", "egfr", "alt"]);
    assert_eq!(
        (&labs[0]["value"], &labs[0]["status"], &labs[0]["date"]),
        (&json!("231"), &json!("ok"), &json!("2026-09-14"))
    );
    assert!(
        labs.iter()
            .all(|l| l["src"].as_str().is_some_and(|s| s.starts_with("src_")))
    );
    let not_assessed = out["not_assessed"].as_array().cloned().unwrap_or_default();
    assert!(
        not_assessed
            .iter()
            .any(|n| n.as_str().is_some_and(|n| n.contains("drug-drug interactions")))
    );
    assert_eq!(out["withheld"], json!({"by_scope": []}));
    assert!(
        out["notice"]
            .as_str()
            .is_some_and(|n| n.starts_with("Information from your records, not medical advice."))
    );
    Ok(())
}

#[tokio::test]
async fn every_quote_is_verbatim_cpic_text() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let snap: BTreeSet<(String, String)> = cpic_snapshot()?["recommendations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| (r["id"].to_string(), r["drugrecommendation"].to_string()))
        .collect();
    for drug in ["clopidogrel", "atorvastatin", "omeprazole", "simvastatin", "citalopram"] {
        let out = check_drug(&mut tx, s.pid("alex"), drug, &all_scopes(), "en", Some(as_of())).await?;
        let g = out["guidance"].as_array().cloned().unwrap_or_default();
        assert!(!g.is_empty(), "{drug}");
        for g in g {
            assert!(
                snap.contains(&(g["recommendation_id"].to_string(), g["quote"].to_string())),
                "{drug}: {g}"
            );
        }
    }
    Ok(())
}

fn kind_of(g: &Value) -> String {
    let a = g["applies_to"].as_str().unwrap_or_default();
    let tail = a.rsplit('(').next().unwrap_or_default();
    tail.split(',')
        .next()
        .unwrap_or_default()
        .trim_end_matches(')')
        .to_string()
}

#[tokio::test]
async fn sam_codeine_with_a_strong_cyp2d6_inhibitor_is_phenoconverted() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("sam");
    let src = med(&mut tx, pid, "Paroxetine").await?;
    let out = check_drug(&mut tx, pid, "codeína", &all_scopes(), "en", Some(as_of())).await?;
    let pcs = out["phenoconversion"].as_array().cloned().unwrap_or_default();
    assert_eq!(pcs.len(), 1);
    let pc = &pcs[0];
    assert_eq!(
        (&pc["gene"], &pc["inhibitor_strength"], &pc["computed"]),
        (&json!("CYP2D6"), &json!("strong"), &json!(true))
    );
    assert_eq!(
        (
            &pc["activity_score"],
            &pc["activity_score_functional"],
            &pc["functional_phenotype"]
        ),
        (&json!("2.0"), &json!("0.0"), &json!("Poor Metabolizer"))
    );
    assert!(pc["src"] == src.as_str() && pc["based_on"] == json!(["src_g27", src]));
    let guidance = out["guidance"].as_array().cloned().unwrap_or_default();
    let by = |k: &str| {
        guidance
            .iter()
            .rev()
            .find(|g| kind_of(g) == k)
            .cloned()
            .unwrap_or_default()
    };
    assert!(by("genotype")["quote"] == CODEINE_NM && by("genotype")["strength"] == "Strong");
    assert_eq!(by("functional")["quote"], CODEINE_PM);
    assert_eq!(by("functional")["based_on"], json!(["src_g27", src]));
    assert_eq!(out["pgx"][0]["phenotype"], "Normal Metabolizer"); // the genotype phenotype is kept beside it
    assert_eq!(
        out["current_medications_relevant"][0]["relation"],
        "inhibitor:CYP2D6:strong"
    );
    Ok(())
}

#[tokio::test]
async fn moderate_inhibitor_halves_the_activity_score() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("sam");
    med(&mut tx, pid, "Duloxetine").await?;
    let out = check_drug(&mut tx, pid, "tramadol", &all_scopes(), "en", Some(as_of())).await?;
    let pc = &out["phenoconversion"];
    assert_eq!(pc.as_array().map(Vec::len), Some(1));
    assert_eq!(
        (&pc[0]["activity_score_functional"], &pc[0]["functional_phenotype"]),
        (&json!("1.0"), &json!("Intermediate Metabolizer"))
    );
    Ok(())
}

#[tokio::test]
async fn no_inhibitor_no_phenoconversion() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = check_drug(&mut tx, s.pid("sam"), "codeine", &all_scopes(), "en", Some(as_of())).await?;
    assert_eq!(out["phenoconversion"], json!([]));
    let quotes: Vec<_> = out["guidance"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|g| g["quote"].clone())
        .collect();
    assert_eq!(quotes, [json!(CODEINE_NM)]);
    Ok(())
}

fn tpmt() -> Value {
    json!({"category": "pgx", "gene": "TPMT", "diplotype": "*1/*3A", "phenotype": "Intermediate Metabolizer",
           "evidence": "E1", "call_state": "alt", "src": "src_g61"})
}

#[tokio::test]
async fn multi_gene_thiopurine_lookup() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("carmen");
    s.set_findings("carmen", vec![tpmt()]);
    let only_tpmt = check_drug(&mut tx, pid, "azatioprina", &all_scopes(), "en", Some(as_of())).await?;
    s.set_findings(
        "carmen",
        vec![
            tpmt(),
            json!({"category": "pgx", "gene": "NUDT15", "phenotype": "Normal Metabolizer", "call_state": "ref",
                   "evidence": "E1", "src": "src_g62"}),
        ],
    );
    let both = check_drug(&mut tx, pid, "azathioprine", &all_scopes(), "en", Some(as_of())).await?;
    let g1 = only_tpmt["guidance"].as_array().cloned().unwrap_or_default();
    assert_eq!(g1.len(), 1);
    assert!(
        g1[0]["quote"]
            .as_str()
            .is_some_and(|q| q.starts_with("Based on TPMT, initiate therapy with reduced starting doses"))
    );
    assert!(
        only_tpmt["not_assessed"]
            .as_array()
            .is_some_and(|n| n.contains(&json!("NUDT15: no genome finding on record")))
    );
    let g2 = both["guidance"].as_array().cloned().unwrap_or_default();
    assert_eq!(g2.len(), 1);
    assert!(g2[0]["quote"].as_str().is_some_and(|q| {
        q.starts_with("Initiate therapy with reduced starting doses (30-80% of standard starting dose)")
    }));
    assert_eq!(g2[0]["based_on"], json!(["src_g61", "src_g62"]));
    Ok(())
}

#[tokio::test]
async fn warfarin_is_named_not_quoted() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    s.set_findings(
        "carmen",
        vec![json!({"category": "pgx", "gene": "CYP2C9", "phenotype": "Intermediate Metabolizer", "activity_score": "1.0",
                    "call_state": "alt", "evidence": "E1", "src": "src_g63"})],
    );
    let out = check_drug(
        &mut tx,
        s.pid("carmen"),
        "Aldocumar",
        &all_scopes(),
        "en",
        Some(as_of()),
    )
    .await?;
    assert_eq!(out["guidance"], json!([]));
    assert!(
        out["not_assessed"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|n| n.as_str().is_some_and(|n| n.contains("algorithm")))
    );
    let groups: Vec<_> = out["relevant_labs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| l["alias_group"].clone())
        .collect();
    assert_eq!(groups, [json!("inr"), json!("hemoglobin"), json!("platelets")]);
    assert_eq!(out["relevant_labs"][0]["status"], "nodata"); // missing is explicit, never normal
    Ok(())
}

#[tokio::test]
async fn scopes_withhold_parts() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("alex");
    let labs_only = check_drug(
        &mut tx,
        pid,
        "clopidogrel",
        &scope_set(["read:labs"]),
        "en",
        Some(as_of()),
    )
    .await?;
    assert!(labs_only["pgx"] == json!([]) && labs_only["guidance"] == json!([]));
    assert_eq!(labs_only["current_medications_relevant"], json!([]));
    assert_eq!(labs_only["withheld"]["by_scope"], json!(["genome", "records"]));
    assert!(labs_only["relevant_labs"].as_array().is_some_and(|l| !l.is_empty()));
    let genome_only = check_drug(
        &mut tx,
        pid,
        "clopidogrel",
        &scope_set(["read:genome"]),
        "en",
        Some(as_of()),
    )
    .await?;
    assert!(genome_only["pgx"].as_array().is_some_and(|p| !p.is_empty()));
    assert!(genome_only["guidance"].as_array().is_some_and(|g| !g.is_empty()));
    assert_eq!(genome_only["phenoconversion"], json!([]));
    assert!(genome_only["relevant_labs"] == json!([]) && genome_only["drug"]["currently_taking"].is_null());
    Ok(())
}

#[tokio::test]
async fn unknown_drug_is_an_error_never_no_interaction() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = check_drug(
        &mut tx,
        s.pid("alex"),
        "not-a-drug-xyz",
        &all_scopes(),
        "en",
        Some(as_of()),
    )
    .await;
    assert!(matches!(out, Err(Error::NotFound(_))), "{out:?}");
    Ok(())
}

#[tokio::test]
async fn resolves_drug_names() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    assert_eq!(
        resolve_drug(&mut tx, "Omeprazol").await?,
        Some(json!({"cpic_drug_id": "RxNorm:7646", "name": "omeprazole"}))
    );
    assert_eq!(
        resolve_drug(&mut tx, "RxNorm:32968").await?.map(|d| d["name"].clone()),
        Some(json!("clopidogrel"))
    );
    assert_eq!(
        resolve_drug(&mut tx, "Atorvastatina Cinfa 20 mg")
            .await?
            .map(|d| d["name"].clone()),
        Some(json!("atorvastatin"))
    );
    assert_eq!(
        resolve_drug(&mut tx, "fluconazol").await?,
        Some(json!({"cpic_drug_id": null, "name": "fluconazole"}))
    );
    assert_eq!(resolve_drug(&mut tx, "zzz").await?, None);
    Ok(())
}

#[tokio::test]
async fn a_drug_without_a_level_a_pair() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = check_drug(&mut tx, s.pid("alex"), "fluoxetine", &all_scopes(), "en", Some(as_of())).await?;
    assert!(out["pgx"] == json!([]) && out["guidance"] == json!([]));
    let na = out["not_assessed"].as_array().cloned().unwrap_or_default();
    assert!(
        na.iter()
            .any(|n| n.as_str().is_some_and(|n| n.contains("no CPIC level A gene pair")))
    );
    Ok(())
}

#[tokio::test]
async fn lab_reasons_follow_the_language() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let reason = |out: &Value| -> Value {
        out["relevant_labs"]
            .as_array()
            .and_then(|l| l.iter().find(|l| l["alias_group"] == "egfr"))
            .map(|l| l["reason"].clone())
            .unwrap_or_default()
    };
    let en = check_drug(&mut tx, s.pid("alex"), "Plavix", &all_scopes(), "en", Some(as_of())).await?;
    let es = check_drug(&mut tx, s.pid("alex"), "Plavix", &all_scopes(), "es", Some(as_of())).await?;
    assert!(reason(&en) == "kidney function" && reason(&es) == "función renal");
    Ok(())
}

/// CPIC 2020: 0 poor, above 0 and below 1.25 intermediate, 1.25 to 2.25 normal, above 2.25 ultrarapid.
#[test]
fn every_converted_activity_score_falls_in_a_cpic_band() -> R {
    let pack = load_pack()?;
    for (score, phenotype) in [
        ("0", "Poor Metabolizer"),
        ("0.125", "Intermediate Metabolizer"), // a moderate inhibitor on activity score 0.25
        ("1", "Intermediate Metabolizer"),
        ("1.125", "Intermediate Metabolizer"), // a moderate inhibitor on activity score 2.25
        ("1.25", "Normal Metabolizer"),
        ("2.25", "Normal Metabolizer"),
        ("2.375", "Ultrarapid Metabolizer"),
        ("3", "Ultrarapid Metabolizer"),
    ] {
        assert_eq!(
            band(&pack, score.parse::<Decimal>()?).as_deref(),
            Some(phenotype),
            "{score}"
        );
    }
    Ok(())
}
