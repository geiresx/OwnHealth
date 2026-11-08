//! list_findings and pgx_for_drug: gating, minors, evidence, coverage first, untrusted text
//! (`tests/genome/test_genome_findings.py`).

mod common;

use chrono::{NaiveDate, TimeZone, Utc};
use common::{R, env, fact_mut};
use ownhealth_core::Error;
use ownhealth_core::access::{Principal, PrincipalKind, effective_permission, scope_set, set_genome_switches};
use ownhealth_genome::findings::pgx_for_drug_with;
use ownhealth_genome::{Findings, FindingsQuery, Gate, gate_for, list_findings_typed, pgx_for_drug, synthetic};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

fn closed() -> Gate {
    Gate::new(false, false)
}

fn open() -> Gate {
    Gate::new(true, false)
}

fn as_of() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default()
}

fn genes(found: &Findings) -> Vec<String> {
    found
        .findings
        .iter()
        .map(|f| f["gene"].as_str().unwrap_or_default().to_string())
        .collect()
}

async fn find(conn: &mut PgConnection, p: Uuid, gate: &Gate, q: FindingsQuery<'_>) -> ownhealth_core::Result<Findings> {
    list_findings_typed(conn, p, gate, &q).await
}

fn ev(min: &str) -> FindingsQuery<'_> {
    FindingsQuery {
        min_evidence: Some(min),
        ..Default::default()
    }
}

#[tokio::test]
async fn sensitive_finding_is_counted_never_described() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let found = find(&mut c, e.profile("alex"), &closed(), ev("E3")).await?;
    assert!(!genes(&found).contains(&"APOE".into()) && found.withheld_sensitive == 1);
    let text = serde_json::to_string(&found.findings)?;
    assert!(!text.contains("APOE") && !text.contains("e3/e4"));
    let opened = find(&mut c, e.profile("alex"), &open(), ev("E3")).await?;
    assert!(genes(&opened).contains(&"APOE".into()) && opened.withheld_sensitive == 0);
    Ok(())
}

#[tokio::test]
async fn minor_rule_withholds_whatever_the_switches() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let lucia = e.profile("lucia");
    e.seed(&mut c, "lucia", &synthetic::lucia_facts(), &[]).await?;
    for gate in [Gate::new(true, true), Gate::new(false, true)] {
        let found = find(&mut c, lucia, &gate, ev("E4")).await?;
        assert_eq!(genes(&found), vec!["TPMT"]); // PGx guides a child's prescriptions; it stays
        assert_eq!(found.withheld_minor, 3); // carrier, APOE, PRS
    }
    let adult = find(&mut c, lucia, &open(), ev("E4")).await?;
    assert_eq!((adult.withheld_minor, adult.findings.len()), (0, 4));
    Ok(())
}

fn token(user: Uuid, household: Uuid, scopes: &[&str]) -> Principal {
    Principal {
        kind: PrincipalKind::Token,
        scopes: scope_set(scopes.iter().copied()),
        ..Principal::session(user, household)
    }
}

#[tokio::test]
async fn gate_for_uses_the_right_switch() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let alex = e.profile("alex");
    let owner = Principal::session(e.fam.user("alex"), e.fam.household_id);
    let perm = effective_permission(&mut c, &owner, alex).await?;
    let g = gate_for(&perm, "ui", as_of(), 18)?;
    assert_eq!((g.sensitive, g.minor), (false, false));
    set_genome_switches(&mut c, alex, Some(true), None).await?;
    let perm = effective_permission(&mut c, &owner, alex).await?;
    assert!(gate_for(&perm, "ui", as_of(), 18)?.sensitive);
    assert!(!gate_for(&perm, "agents", as_of(), 18)?.sensitive); // seeing is not consent to share
    set_genome_switches(&mut c, alex, None, Some(true)).await?;
    let t = token(e.fam.user("alex"), e.fam.household_id, &["read:genome"]);
    assert!(!gate_for(&effective_permission(&mut c, &t, alex).await?, "agents", as_of(), 18)?.sensitive);
    let t2 = token(
        e.fam.user("alex"),
        e.fam.household_id,
        &["read:genome", "read:sensitive"],
    );
    assert!(gate_for(&effective_permission(&mut c, &t2, alex).await?, "agents", as_of(), 18)?.sensitive);
    let guardian = Principal::session(e.fam.user("alex"), e.fam.household_id);
    let perm = effective_permission(&mut c, &guardian, e.profile("lucia")).await?;
    assert!(gate_for(&perm, "ui", as_of(), 18)?.minor);
    assert!(matches!(gate_for(&perm, "fax", as_of(), 18), Err(Error::Invalid(_))));
    Ok(())
}

#[tokio::test]
async fn evidence_filter_and_hidden_by_default() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let e2 = find(&mut c, alex, &closed(), FindingsQuery::default()).await?;
    assert!(
        e2.findings
            .iter()
            .all(|f| f["evidence"] == "E1" || f["evidence"] == "E2")
    );
    assert!(!serde_json::to_string(&e2.findings)?.contains("CAD"));
    let e3 = find(&mut c, alex, &closed(), ev("E3")).await?;
    assert!(e3.findings.iter().any(|f| f["category"] == "prs") && !genes(&e3).contains(&"MTHFR".into()));
    let e4 = find(&mut c, alex, &closed(), ev("E4")).await?;
    let mthfr = e4
        .findings
        .iter()
        .find(|f| f["gene"] == "MTHFR")
        .cloned()
        .unwrap_or_default();
    assert!(mthfr["evidence"] == "E4" && mthfr["hidden_by_default"] == true);
    assert!(
        mthfr["evidence_native"]
            .as_str()
            .unwrap_or_default()
            .starts_with("not supported")
    );
    let levels: Vec<String> = e4.findings.iter().map(|f| f["evidence"].to_string()).collect();
    let mut sorted = levels.clone();
    sorted.sort();
    assert_eq!(levels, sorted);
    Ok(())
}

#[tokio::test]
async fn findings_carry_evidence_confidence_call_state_and_src() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let q = FindingsQuery {
        category: Some("pgx"),
        ..Default::default()
    };
    let found = find(&mut c, e.profile("alex"), &closed(), q).await?;
    let cyp = found
        .findings
        .iter()
        .find(|f| f["gene"] == "CYP2C19")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (&cyp["evidence"], &cyp["evidence_native"]),
        (&json!("E1"), &json!("CPIC A"))
    );
    assert_eq!(
        (&cyp["confidence"], &cyp["confidence_basis"]),
        (&json!("high"), &json!("two callers agree"))
    );
    assert_eq!(
        (&cyp["call_state"], &cyp["diplotype"]),
        (&json!("alt"), &json!("*1/*2"))
    );
    assert!(cyp["src"].as_str().unwrap_or_default().starts_with("src_g_"));
    assert_eq!(
        cyp["summary"],
        json!({"trust": "untrusted_text", "origin": "genome_facts", "text": "CYP2C19 *1/*2: Intermediate Metabolizer."})
    );
    assert_eq!(cyp["drug_guidance"][0]["recommendation"]["trust"], "untrusted_text");
    assert_eq!(found.db_versions["cpic"], "2026-09");
    assert_eq!(found.assay.as_ref().map(|a| a["type"].clone()), Some(json!("wgs")));
    assert!(found.findings.iter().all(|f| f["category"] == "pgx"));
    Ok(())
}

#[tokio::test]
async fn not_assessed_is_listed_explicitly() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let found = find(&mut c, e.profile("alex"), &closed(), FindingsQuery::default()).await?;
    for want in [
        json!({"gene": "FMR1", "panel": "carrier", "reason": "not_run"}),
        json!({"panel": "repeat_expansions", "reason": "not_run"}),
        json!({"gene": "SMN1", "panel": "carrier", "reason": "paralog"}),
    ] {
        assert!(found.not_assessed.contains(&want), "{want}");
    }
    Ok(())
}

fn gene(g: &str) -> FindingsQuery<'_> {
    FindingsQuery {
        gene: Some(g),
        ..Default::default()
    }
}

#[tokio::test]
async fn gene_query_answers_coverage_first() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let pms2 = find(&mut c, alex, &closed(), gene("PMS2")).await?;
    assert!(
        pms2.findings.is_empty() && pms2.coverage[0]["state"] == "partial" && pms2.coverage[0]["reason"] == "paralog"
    );
    assert_eq!(pms2.coverage[0]["fraction_callable"], json!(0.81));
    let fmr1 = find(&mut c, alex, &closed(), gene("fmr1")).await?;
    assert!(fmr1.coverage[0]["state"] == "not_assessed" && fmr1.not_assessed[0]["gene"] == "FMR1");
    let unknown = find(&mut c, alex, &closed(), gene("NOTAGENE")).await?;
    assert_eq!(
        unknown.not_assessed,
        vec![json!({"gene": "NOTAGENE", "panel": null, "reason": "not_in_coverage_map"})]
    );
    let apoe = find(&mut c, alex, &closed(), gene("APOE")).await?;
    assert!(apoe.findings.is_empty() && apoe.withheld_sensitive == 1 && apoe.not_assessed.is_empty());
    Ok(())
}

#[tokio::test]
async fn no_genome_on_file() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let found = find(&mut c, e.profile("carmen"), &closed(), FindingsQuery::default()).await?;
    assert!(found.findings.is_empty() && found.assay.is_none() && found.limitations[0].contains("OwnGenome"));
    Ok(())
}

#[tokio::test]
async fn as_of_before_the_import_sees_nothing() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &e.blobs, &raw)?;
    let at = Utc.with_ymd_and_hms(2026, 10, 2, 8, 0, 0).single();
    synthetic::seed_genome(
        &mut c,
        &e.blobs,
        e.fam.household_id,
        e.profile("alex"),
        &facts,
        &att,
        "g.json",
        at,
    )
    .await?;
    let day = |d: u32| FindingsQuery {
        as_of: NaiveDate::from_ymd_opt(2026, 10, d),
        ..Default::default()
    };
    assert!(
        find(&mut c, e.profile("alex"), &closed(), day(1))
            .await?
            .findings
            .is_empty()
    );
    assert!(
        !find(&mut c, e.profile("alex"), &closed(), day(2))
            .await?
            .findings
            .is_empty()
    );
    Ok(())
}

fn entry<'a>(entries: &'a [Value], gene: &str) -> &'a Value {
    entries
        .iter()
        .find(|e| e["gene"].as_str().is_some_and(|g| g.eq_ignore_ascii_case(gene)))
        .unwrap_or_else(|| panic!("no {gene} in {entries:?}"))
}

#[tokio::test]
async fn pgx_for_drug_entries() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let clop = pgx_for_drug(&mut c, alex, "RxNorm:32968", None).await?;
    let cyp = entry(&clop, "CYP2C19");
    assert!(cyp["status"] == "found" && cyp["phenotype"] == "Intermediate Metabolizer" && cyp["actionable"] == true);
    let drugs: Vec<&Value> = cyp["guidance"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|g| &g["drug"])
        .collect();
    assert_eq!(drugs, vec![&json!("clopidogrel")]);
    assert_eq!(cyp["call_state"], "alt");
    assert_eq!(cyp["guidance"][0]["recommendation"]["trust"], "untrusted_text");
    let fu = pgx_for_drug(&mut c, alex, "RxNorm:4492", None).await?;
    let dpyd = entry(&fu, "DPYD");
    assert!(dpyd["call_state"] == "ref" && dpyd["actionable"] == false && dpyd["activity_score"] == json!(2.0));
    // an explicit gene list (from lane E's CPIC pairs): a gene with no finding says why
    let wanted = ["CYP2D6", "UGT1A1", "FMR1"].map(String::from);
    let some = pgx_for_drug_with(&mut c, alex, "RxNorm:2670", None, Some(&wanted), None).await?;
    assert_eq!(entry(&some, "CYP2D6")["activity_score"], json!(1.5));
    assert_eq!(entry(&some, "UGT1A1")["status"], "no_finding");
    assert_eq!(entry(&some, "FMR1")["status"], "not_assessed");
    let sam = e.profile("sam");
    e.seed(&mut c, "sam", &synthetic::sam_facts(), &[]).await?;
    let codeine = pgx_for_drug(&mut c, sam, "RxNorm:2670", None).await?;
    let d6 = entry(&codeine, "CYP2D6");
    assert!(d6["diplotype"] == "*1/*1" && d6["call_state"] == "ref" && d6["confidence"] == "medium");
    Ok(())
}

#[tokio::test]
async fn bad_filters() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    for q in [
        ev("E5"),
        FindingsQuery {
            category: Some("astrology"),
            ..Default::default()
        },
    ] {
        assert!(matches!(
            find(&mut c, e.profile("alex"), &closed(), q).await,
            Err(Error::Invalid(_))
        ));
    }
    Ok(())
}

/// HLA results carry `present`, VKORC1 results no phenotype: neither may read as no action needed.
#[tokio::test]
async fn actionable_without_a_phenotype() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let mut doc = synthetic::alex_facts();
    let hla = fact_mut(&mut doc, "HLA-B");
    hla["value"]["present"] = json!(true);
    hla["drug_guidance"][0]["phenotype"] = json!("HLA-B*58:01 positive");
    let alex = e.profile("alex");
    e.seed(&mut c, "alex", &doc, &[]).await?;
    let allopurinol = pgx_for_drug(&mut c, alex, "RxNorm:519", None).await?;
    let hla = entry(&allopurinol, "HLA-B");
    assert!(hla["status"] == "found" && hla["actionable"] == true);
    let warfarin = pgx_for_drug(&mut c, alex, "RxNorm:11289", None).await?;
    let vk = entry(&warfarin, "VKORC1");
    assert!(vk["phenotype"].is_null() && vk["actionable"] == true);
    assert_eq!(entry(&warfarin, "CYP2C9")["actionable"], false); // Normal Metabolizer stays not actionable
    Ok(())
}

/// Two sources of one person: the higher confidence wins the call, the guidance is quoted from the
/// source with the newest CPIC version for that phenotype (genomics.md 4).
#[tokio::test]
async fn guidance_comes_from_the_newest_cpic_version() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let alex = e.profile("alex");
    e.seed(&mut c, "alex", &synthetic::alex_facts(), &[]).await?; // cpic 2026-09, high confidence
    let mut newer = synthetic::alex_facts();
    newer["subject"]["sample_id"] = json!("AR-02");
    newer["provenance"]["db_versions"]["cpic"] = json!("2026-11");
    newer["provenance"]["generated_utc"] = json!("2026-11-01T09:00:00Z");
    let cyp = fact_mut(&mut newer, "CYP2C19");
    cyp["confidence"]["level"] = json!("medium");
    for g in cyp["drug_guidance"].as_array_mut().into_iter().flatten() {
        let drug = g["drug"].as_str().unwrap_or_default().to_string();
        g["recommendation"] = json!(format!("Synthetic newer stand-in for {drug}."));
    }
    e.seed(&mut c, "alex", &newer, &[]).await?;
    let clop = pgx_for_drug(&mut c, alex, "RxNorm:32968", None).await?;
    let ent = entry(&clop, "CYP2C19");
    assert_eq!(ent["confidence"], "high"); // the call from the higher-confidence source
    assert_eq!(ent["guidance_cpic_version"], "2026-11");
    assert!(serde_json::to_string(&ent["guidance"])?.contains("newer stand-in"));
    let found = find(&mut c, alex, &closed(), FindingsQuery::default()).await?;
    let f = found
        .findings
        .iter()
        .find(|x| x["gene"] == "CYP2C19")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (&f["n_sources"], &f["guidance_cpic_version"]),
        (&json!(2), &json!("2026-11"))
    );
    Ok(())
}
