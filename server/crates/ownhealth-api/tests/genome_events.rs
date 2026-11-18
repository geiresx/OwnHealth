//! REST: `GET /profiles/{p}/genome/events`, the ClinVar diff's history, gated per reader. All data
//! is synthetic.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth_api::routes::genome::events::events;
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, PrincipalKind, scope_set};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use ownhealth_genome::{clinvar, synthetic};
use serde_json::Value;
use tower::ServiceExt;

type R = anyhow::Result<()>;

struct Env {
    db: TestDb,
    dir: tempfile::TempDir,
    rt: Runtime,
    app: Router,
    fam: Family,
    /// Alex's genome-facts document.
    doc: uuid::Uuid,
}

const HEAD: &str = "##fileformat=VCFv4.1\n#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n";

/// Two releases: APOE (sensitive) and CFTR change between them; Alex carries both sites.
fn releases(dir: &std::path::Path) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let row = |pos: &str, sig: &str, rev: &str| -> String {
        let (chrom, p, vid, r, a, gene) = match pos {
            "apoe" => ("19", 44908684, 17864, "T", "C", "APOE"),
            _ => ("7", 117559590, 7105, "ATCT", "A", "CFTR"),
        };
        format!("{chrom}\t{p}\t{vid}\t{r}\t{a}\t.\t.\tCLNREVSTAT={rev};CLNSIG={sig};GENEINFO={gene}:1\n")
    };
    let one = dir.join("one.vcf");
    let two = dir.join("two.vcf");
    std::fs::write(
        &one,
        format!(
            "##fileDate=20261005\n{HEAD}{}{}",
            row("cftr", "Pathogenic", "reviewed_by_expert_panel"),
            row("apoe", "risk_factor", "no_assertion_criteria_provided")
        ),
    )?;
    std::fs::write(
        &two,
        format!(
            "##fileDate=20261012\n{HEAD}{}{}",
            row("cftr", "Uncertain_significance", "criteria_provided,_single_submitter"),
            row("apoe", "Pathogenic", "criteria_provided,_single_submitter")
        ),
    )?;
    Ok((one, two))
}

async fn env() -> anyhow::Result<Env> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    let rt = Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &rt.blobs, &raw)?;
    let (doc, _) = synthetic::seed_genome(
        &mut c,
        &rt.blobs,
        fam.household_id,
        fam.profile("alex"),
        &facts,
        &att,
        "g.json",
        None,
    )
    .await?;
    // Carmen, whom Alex cares for without read:genome, carries the same CFTR site.
    let mut carmen = synthetic::alex_facts();
    carmen["subject"]["sample_id"] = serde_json::json!("CO-01");
    synthetic::seed_genome(
        &mut c,
        &rt.blobs,
        fam.household_id,
        fam.profile("carmen"),
        &carmen,
        &[],
        "c.json",
        None,
    )
    .await?;
    drop(c);
    let (one, two) = releases(dir.path())?;
    for r in [one, two] {
        clinvar::diff_release(&db.pool, &rt.blobs, &r).await?;
    }
    let app = Router::new()
        .nest("/api", ownhealth_api::router())
        .with_state(rt.clone());
    Ok(Env {
        db,
        dir,
        rt,
        app,
        fam,
        doc,
    })
}

impl Env {
    async fn get(&self, slug: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        self.send(slug, Request::get(uri), Body::empty()).await
    }

    async fn post(&self, slug: &str, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
        let req = Request::post(uri).header("content-type", "application/json");
        self.send(slug, req, Body::from(body.to_string())).await
    }

    async fn send(
        &self,
        slug: &str,
        req: axum::http::request::Builder,
        body: Body,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let mut c = self.db.pool.acquire().await?;
        let secret =
            sessions::create_session_with(&mut c, self.fam.user(slug), None, None, "passkey", Some(false)).await?;
        let req = req
            .header("cookie", format!("{}={secret}", sessions::COOKIE))
            .body(body)?;
        let res = self.app.clone().oneshot(req).await?;
        let status = res.status();
        let body = to_bytes(res.into_body(), 1 << 24).await?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    fn token(&self, scopes: &[&str]) -> Principal {
        let mut p = Principal::session(self.fam.user("alex"), self.fam.household_id);
        p.kind = PrincipalKind::Token;
        p.scopes = scope_set(scopes);
        p.token_label = Some("test agent".into());
        p
    }
}

fn genes(body: &Value) -> Vec<String> {
    body["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| e["gene"].as_str().unwrap_or("-").to_string())
        .collect()
}

#[tokio::test]
async fn the_reader_gate_decides_whether_an_event_is_named() -> R {
    let e = env().await?;
    assert!(e.dir.path().exists());
    // The sensitive switch is off: APOE reads as a withheld result, CFTR is named.
    let (status, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(genes(&body), vec!["CFTR", "-"], "{body}");
    assert_eq!(body["withheld"]["sensitive"], 1);
    let hidden = &body["events"][1];
    assert_eq!(hidden["withheld"], "sensitive");
    assert_eq!(hidden["text"]["en"], "A withheld result changed in ClinVar.");
    assert_eq!(
        hidden["text"]["es"],
        "Ha cambiado en ClinVar un resultado que está oculto."
    );
    for leak in ["APOE", "17864", "upgraded", "Pathogenic"] {
        assert!(!hidden.to_string().contains(leak), "{leak} in {hidden}");
    }
    // Nothing but the release window, the chip flag and the generic text: not even whether it alerted,
    // which would narrow the change to an upgrade, a shown downgrade or an SF gene.
    for key in ["kind", "gene", "variation_id", "old", "new", "alerted"] {
        assert!(hidden.get(key).is_none(), "{key} in {hidden}");
    }
    let cftr = &body["events"][0];
    assert_eq!(cftr["kind"], "downgraded_from_plp");
    assert_eq!(cftr["new"]["class"], "vus");
    assert_eq!(cftr["release_from"], "2026-10-05");
    assert_eq!(cftr["alerted"], true);

    // Switch on: the owner sees the APOE event named.
    sqlx::query("update profile set genome_sensitive_ui = true where id = $1")
        .bind(e.fam.profile("alex"))
        .execute(&e.db.pool)
        .await?;
    let (_, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(genes(&body), vec!["CFTR", "APOE"], "{body}");
    assert!(
        body["events"][1]["text"]["es"]
            .as_str()
            .is_some_and(|s| s.contains("APOE") && s.contains("Antes: otro tipo de clasificación")),
        "{body}"
    );

    // An agent needs the agents switch and read:sensitive too; without them APOE stays withheld.
    let agent = events(&e.rt, &e.token(&["read:genome"]), "alex")
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(genes(&agent), vec!["CFTR", "-"]);
    // Without read:genome nothing is answered.
    let refused = events(&e.rt, &e.token(&["read:labs"]), "alex").await;
    assert!(refused.is_err());

    // Every read wrote one audit row with ids only.
    let audits: Vec<(Option<String>, Option<Value>)> = sqlx::query_as(
        "select tool, withheld from audit_event where profile_id = $1 and resource_type = 'genome_event' order by id",
    )
    .bind(e.fam.profile("alex"))
    .fetch_all(&e.db.pool)
    .await?;
    assert_eq!(audits.len(), 3);
    assert_eq!(audits[0].0.as_deref(), Some("GET /api/profiles/{p}/genome/events"));
    Ok(())
}

#[tokio::test]
async fn a_minor_s_events_are_withheld_by_the_minor_rule() -> R {
    let e = env().await?;
    let mut c = e.db.pool.acquire().await?;
    synthetic::seed_genome(
        &mut c,
        &e.rt.blobs,
        e.fam.household_id,
        e.fam.profile("lucia"),
        &synthetic::lucia_facts(),
        &[],
        "l.json",
        None,
    )
    .await?;
    drop(c);
    let (_, two) = releases(e.dir.path())?;
    let three = e.dir.path().join("three.vcf");
    std::fs::write(
        &three,
        std::fs::read_to_string(two)?.replace("20261012", "20261019").replace(
            "CLNREVSTAT=criteria_provided,_single_submitter;CLNSIG=Uncertain_significance",
            "CLNREVSTAT=criteria_provided,_single_submitter;CLNSIG=Benign",
        ),
    )?;
    clinvar::diff_release(&e.db.pool, &e.rt.blobs, &three).await?;
    // Lucía's first run against her facts: CFTR P -> benign, APOE risk factor -> P.
    let (status, body) = e.get("alex", "/api/profiles/lucia/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["withheld"]["minor_rule"], 2, "{body}");
    assert_eq!(genes(&body), vec!["-", "-"]);
    assert!(!body.to_string().contains("CFTR") && !body.to_string().contains("APOE"));
    for ev in body["events"].as_array().into_iter().flatten() {
        assert_eq!(ev["withheld"], "minor", "{ev}");
        for key in ["kind", "gene", "variation_id", "old", "new", "alerted"] {
            assert!(ev.get(key).is_none(), "{key} in {ev}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_caregiver_without_read_genome_sees_no_gene_in_the_review_queue() -> R {
    let e = env().await?;
    let (status, body) = e.get("alex", "/api/profiles/carmen/genome/events").await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = e
        .get(
            "alex",
            "/api/review?status=open&profile=carmen&kind=genome_reinterpretation",
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().cloned().unwrap_or_default();
    assert!(!items.is_empty(), "{body}");
    // Nor which way the class moved: that alone would say Carmen carries a pathogenic variant.
    for leak in ["CFTR", "7105", "APOE", "17864"]
        .into_iter()
        .chain(clinvar::EVENT_KINDS)
    {
        assert!(!body.to_string().contains(leak), "{leak} in {body}");
    }
    // The single-item view says no more than the list.
    for item in &items {
        let id = item["id"].as_str().unwrap_or_default();
        let (status, one) = e.get("alex", &format!("/api/review/{id}")).await?;
        assert_eq!(status, StatusCode::OK, "{one}");
        for leak in ["CFTR", "7105", "APOE", "17864"]
            .into_iter()
            .chain(clinvar::EVENT_KINDS)
        {
            assert!(!one.to_string().contains(leak), "{leak} in {one}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn retracting_the_genome_hides_its_events_and_dismisses_its_alerts() -> R {
    let e = env().await?;
    let open = "/api/review?status=open&profile=alex&kind=genome_reinterpretation";
    let (_, before) = e.get("alex", open).await?;
    assert!(
        !before["items"].as_array().cloned().unwrap_or_default().is_empty(),
        "{before}"
    );
    let (status, body) = e
        .post(
            "alex",
            &format!("/api/documents/{}/retract", e.doc),
            serde_json::json!({"reason": "another person's file"}),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["events"], serde_json::json!([]), "{body}");
    let (_, after) = e.get("alex", open).await?;
    assert_eq!(after["items"], serde_json::json!([]), "{after}");
    Ok(())
}

/// Alex has a second genome file that shows the same changes. Retracting the first file moves its
/// open alerts to the second file's events instead of dismissing them.
#[tokio::test]
async fn retracting_one_genome_file_keeps_the_alerts_another_file_still_shows() -> R {
    let e = env().await?;
    let mut c = e.db.pool.acquire().await?;
    let mut second = synthetic::alex_facts();
    second["subject"]["sample_id"] = serde_json::json!("AR-02");
    let (second_doc, _) = synthetic::seed_genome(
        &mut c,
        &e.rt.blobs,
        e.fam.household_id,
        e.fam.profile("alex"),
        &second,
        &[],
        "g2.json",
        None,
    )
    .await?;
    drop(c);
    let (_, two) = releases(e.dir.path())?;
    clinvar::diff_release(&e.db.pool, &e.rt.blobs, &two).await?;
    let open = "/api/review?status=open&profile=alex&kind=genome_reinterpretation";
    let (_, before) = e.get("alex", open).await?;
    let n = before["items"].as_array().map_or(0, Vec::len);
    assert!(n > 0, "{before}");
    let (status, body) = e
        .post(
            "alex",
            &format!("/api/documents/{}/retract", e.doc),
            serde_json::json!({"reason": "another person's file"}),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, after) = e.get("alex", open).await?;
    let items = after["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), n, "{after}");
    let docs: Vec<Option<uuid::Uuid>> = sqlx::query_scalar(
        "select document_id from review_item where kind = 'genome_reinterpretation' and status = 'open'",
    )
    .fetch_all(&e.db.pool)
    .await?;
    assert!(docs.iter().all(|d| *d != Some(e.doc)), "{docs:?}");
    assert!(docs.contains(&Some(second_doc)), "{docs:?}");
    Ok(())
}

/// A record new to ClinVar at a site the person carries, on a later run, reads "not in the previous
/// ClinVar release" as its before: the genome file does carry the variant, so "not reported in your
/// genome file" would be false there.
#[tokio::test]
async fn a_record_new_to_clinvar_reads_as_not_in_the_previous_release() -> R {
    let e = env().await?;
    let three = e.dir.path().join("three.vcf");
    std::fs::write(
        &three,
        format!(
            "##fileDate=20261019\n{HEAD}7\t117559590\t7105\tATCT\tA\t.\t.\tCLNREVSTAT=criteria_provided,_single_submitter;\
             CLNSIG=Uncertain_significance;GENEINFO=CFTR:1\n7\t117559590\t990001\tATCT\tA\t.\t.\t\
             CLNREVSTAT=criteria_provided,_single_submitter;CLNSIG=Likely_benign;GENEINFO=CFTR:1\n"
        ),
    )?;
    clinvar::diff_release(&e.db.pool, &e.rt.blobs, &three).await?;
    let (status, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new = body["events"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["kind"] == "new_assertion_on_carried_variant")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        new["text"]["en"],
        "ClinVar's classification of a variant you carry in CFTR is now: benign or likely benign (1 star). \
         Before: not in the previous ClinVar release.",
        "{body}"
    );
    assert!(
        new["text"]["es"]
            .as_str()
            .is_some_and(|s| s.ends_with("Antes: sin registro en la versión anterior de ClinVar.")),
        "{body}"
    );
    Ok(())
}

/// Wait until `n` statements of this test's database wait on a lock.
async fn wait_for_lock_waits(pool: &sqlx::PgPool, n: i64) -> anyhow::Result<()> {
    for _ in 0..600 {
        let waiting: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity where datname = current_database() and wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await?;
        if waiting >= n {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("fewer than {n} statements ever waited on a lock")
}

#[tokio::test]
async fn a_retraction_during_a_comparison_still_dismisses_what_it_wrote() -> R {
    let e = env().await?;
    let three = e.dir.path().join("three.vcf");
    std::fs::write(
        &three,
        format!(
            "##fileDate=20261019\n{HEAD}7\t117559590\t7105\tATCT\tA\t.\t.\tCLNREVSTAT=reviewed_by_expert_panel;\
             CLNSIG=Pathogenic;GENEINFO=CFTR:1\n19\t44908684\t17864\tT\tC\t.\t.\t\
             CLNREVSTAT=criteria_provided,_single_submitter;CLNSIG=Pathogenic;GENEINFO=APOE:1\n"
        ),
    )?;
    // Hold the comparison of the third release just before it writes Alex's events, while it already
    // holds Alex's genome file; the file is retracted in that moment.
    let mut blocker = e.db.pool.begin().await?;
    sqlx::query("lock table genome_event in exclusive mode")
        .execute(&mut *blocker)
        .await?;
    let (pool, blobs) = (e.db.pool.clone(), e.rt.blobs.clone());
    let run = tokio::spawn(async move { clinvar::diff_release(&pool, &blobs, &three).await });
    wait_for_lock_waits(&e.db.pool, 1).await?;
    let uri = format!("/api/documents/{}/retract", e.doc);
    let retract = e.post("alex", &uri, serde_json::json!({"reason": "another person's file"}));
    let release = async {
        wait_for_lock_waits(&e.db.pool, 2).await?;
        blocker.commit().await?;
        anyhow::Ok(run.await??)
    };
    let ((status, body), r) = tokio::try_join!(retract, release)?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(r.datasets.iter().any(|d| d.profile == "alex" && d.alerts > 0), "{r:?}");
    let (_, after) = e
        .get(
            "alex",
            "/api/review?status=open&profile=alex&kind=genome_reinterpretation",
        )
        .await?;
    assert_eq!(after["items"], serde_json::json!([]), "{after}");
    Ok(())
}

/// The history says which ClinVar release is in use, whether the weekly check is on, and where a
/// reclassified call is made again. The weekly check is one switch: on and off through the inbox API,
/// never deleted, and its local file is checked when saved.
#[tokio::test]
async fn the_history_names_the_release_in_use_and_the_weekly_switch() -> R {
    let e = env().await?;
    let (status, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["release"], "2026-10-12");
    assert_eq!(body["weekly"], false);
    assert_eq!(
        body["rerun"]["en"],
        "OwnGenome makes the call again with bin/og recheck --sample <name> --rerun."
    );
    assert_eq!(
        body["rerun"]["es"],
        "OwnGenome lo obtiene de nuevo con bin/og recheck --sample <name> --rerun."
    );
    // Lucía has no genome: nothing compared yet.
    let (_, lucia) = e.get("alex", "/api/profiles/lucia/genome/events").await?;
    assert_eq!(lucia["release"], Value::Null, "{lucia}");

    let new = |config: Value| serde_json::json!({"kind": "clinvar", "label": "ClinVar", "config": config});
    let (status, bad) = e
        .post(
            "alex",
            "/api/inboxes",
            new(serde_json::json!({"path": "clinvar.vcf.gz"})),
        )
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    let (status, bad) = e
        .post(
            "alex",
            "/api/inboxes",
            new(serde_json::json!({"url": "http://mirror.example/clinvar.vcf.gz"})),
        )
        .await?;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "plain http only on the person's network: {bad}"
    );
    let (status, src) = e.post("alex", "/api/inboxes", new(serde_json::json!({}))).await?;
    assert_eq!(status, StatusCode::CREATED, "{src}");
    assert_eq!(src["poll_seconds"], 7 * 24 * 3600);
    assert_eq!(src["profile_id"], Value::Null);
    let (_, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(body["weekly"], true);
    // One check per household: a second one would download the release twice a week and could
    // neither show in Settings nor be deleted.
    let (status, twice) = e.post("alex", "/api/inboxes", new(serde_json::json!({}))).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{twice}");
    let n: i64 = sqlx::query_scalar("select count(*) from source where kind = 'clinvar'")
        .fetch_one(&e.db.pool)
        .await?;
    assert_eq!(n, 1);

    let id = src["id"].as_str().unwrap_or_default();
    let (status, gone) = e
        .send("alex", Request::delete(format!("/api/inboxes/{id}")), Body::empty())
        .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{gone}");
    let req = Request::patch(format!("/api/inboxes/{id}")).header("content-type", "application/json");
    let (status, off) = e
        .send(
            "alex",
            req,
            Body::from(serde_json::json!({"enabled": false}).to_string()),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{off}");
    let (_, body) = e.get("alex", "/api/profiles/alex/genome/events").await?;
    assert_eq!(body["weekly"], false);
    Ok(())
}
