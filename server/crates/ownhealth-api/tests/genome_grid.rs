//! REST: the Genome screen's PGx gene grid (v0.3). Every CPIC gene answers with its call or why there
//! is none, and names the current medicines from the records that CPIC ties to it; a reader without
//! read:records gets `meds: null`, never an empty list that reads as "no medicine".

use ownhealth_api::routes::genome;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, PrincipalKind, scope_set, set_genome_switches};
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_core::testing::TestDb;
use ownhealth_genome::synthetic;
use serde_json::{Value, json};

type R = anyhow::Result<()>;

fn row<'a>(body: &'a Value, gene: &str) -> &'a Value {
    body["pgx_grid"]
        .as_array()
        .and_then(|g| g.iter().find(|r| r["gene"] == gene))
        .unwrap_or_else(|| panic!("no {gene} row in {}", body["pgx_grid"]))
}

#[tokio::test]
async fn the_grid_names_each_gene_and_the_current_medicines_it_touches() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let alex = fam.profile("alex");
    synthetic::seed_genome(
        &mut c,
        &rt.blobs,
        fam.household_id,
        alex,
        &synthetic::alex_facts(),
        &[],
        "genome-facts.json",
        None,
    )
    .await?;
    let mut src = SrcIn::new("manual");
    src.profile_id = Some(alex);
    src.actor_kind = "system".into();
    for (name, started) in [("Omeprazole 20 mg", "2026-04-01"), ("Paracetamol 1 g", "2026-05-01")] {
        let med = RecordIn::from_json(
            "medication",
            json!({"profile_id": alex, "name": name, "started": started}),
        )?;
        add_record(&mut c, &med, &SrcRef::New(src.clone()), RecordOptions::new("person")).await?;
    }
    drop(c);

    let person = Principal::session(fam.user("alex"), fam.household_id);
    let body = genome::overview(&rt, &person, "alex", None)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let cyp = row(&body, "CYP2C19");
    assert_eq!(
        (&cyp["status"], &cyp["diplotype"], &cyp["actionable"]),
        (&json!("found"), &json!("*1/*2"), &json!(true))
    );
    assert_eq!(cyp["meds"][0]["name"], "Omeprazole 20 mg", "{cyp}");
    assert!(
        cyp["meds"][0]["src"].as_str().is_some_and(|s| s.starts_with("src_")),
        "{cyp}"
    );
    // paracetamol has no CPIC gene; SLCO1B1 guides no current medicine here
    assert_eq!(row(&body, "SLCO1B1")["meds"], json!([]));
    assert!(!body["pgx_grid"].to_string().contains("Paracetamol"));
    // a CPIC gene with no finding and an assessed coverage entry
    assert_eq!(row(&body, "CYP3A5")["status"], "no_finding");
    // the demo was called with PharmCAT 3.1.0: an older minor than the shipped release
    assert_eq!(cyp["stale"]["rerun_steps"], json!([36, 7, 27]), "{cyp}");

    // an agent with read:genome alone sees the calls, not the medicines
    {
        let mut c = db.pool.acquire().await?;
        set_genome_switches(&mut c, alex, None, Some(true)).await?;
    }
    let mut agent = Principal::session(fam.user("alex"), fam.household_id);
    agent.kind = PrincipalKind::Token;
    agent.scopes = scope_set(["read:genome"]);
    agent.token_label = Some("test agent".into());
    let body = genome::overview(&rt, &agent, "alex", None)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let cyp = row(&body, "CYP2C19");
    assert_eq!((&cyp["status"], &cyp["meds"]), (&json!("found"), &Value::Null));
    assert!(!body.to_string().contains("Omeprazole"));
    Ok(())
}
