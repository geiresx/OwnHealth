//! REST: the genome page, a variant lookup and the consent switches, with scopes, gating and audit
//! (`tests/genome/test_genome_api.py`).
//!
//! People call over HTTP with real session cookies, so step-up is the real check. Tokens never reach
//! a REST route, so the token cases call the route functions with a token principal, as Python's
//! tests did by overriding `get_principal`.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth_api::routes::genome;
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, PrincipalKind, scope_set};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use ownhealth_genome::synthetic;
use serde_json::{Value, json};
use tower::ServiceExt;

type AuditRow = (String, Option<String>, Vec<String>, Value);
type RawAuditRow = (String, Option<String>, Option<Vec<String>>, Option<Value>);

type R = anyhow::Result<()>;

const APOE_SITE: &str = "NC_000019.10:g.44908684T>C";

struct Env {
    db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    app: Router,
    fam: Family,
}

/// The Rivera family with Alex's WGS facts and raw files, and Lucía's facts, imported.
async fn genome_family() -> anyhow::Result<Env> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    let rt = Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &rt.blobs, &raw)?;
    for (slug, facts, att) in [("alex", facts, att), ("lucia", synthetic::lucia_facts(), Vec::new())] {
        synthetic::seed_genome(
            &mut c,
            &rt.blobs,
            fam.household_id,
            fam.profile(slug),
            &facts,
            &att,
            "genome-facts.json",
            None,
        )
        .await?;
    }
    drop(c);
    let app = Router::new()
        .nest("/api", ownhealth_api::router())
        .with_state(rt.clone());
    Ok(Env {
        db,
        _dir: dir,
        rt,
        app,
        fam,
    })
}

impl Env {
    /// A session cookie for `slug`, with or without a fresh step-up.
    async fn login(&self, slug: &str, step_up: bool) -> anyhow::Result<String> {
        let mut c = self.db.pool.acquire().await?;
        let secret =
            sessions::create_session_with(&mut c, self.fam.user(slug), None, None, "passkey", Some(step_up)).await?;
        Ok(format!("{}={secret}", sessions::COOKIE))
    }

    async fn get(&self, cookie: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        self.send(Request::get(uri).header("cookie", cookie).body(Body::empty())?)
            .await
    }

    async fn post(&self, cookie: &str, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
        let req = Request::post(uri)
            .header("cookie", cookie)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?;
        self.send(req).await
    }

    async fn send(&self, req: Request<Body>) -> anyhow::Result<(StatusCode, Value)> {
        let res = self.app.clone().oneshot(req).await?;
        let status = res.status();
        let body = to_bytes(res.into_body(), 1 << 24).await?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// Alex's token with these scopes.
    fn token(&self, scopes: &[&str]) -> Principal {
        let mut p = Principal::session(self.fam.user("alex"), self.fam.household_id);
        p.kind = PrincipalKind::Token;
        p.scopes = scope_set(scopes);
        p.token_label = Some("test agent".into());
        p
    }

    /// (tool, args_summary, resource_ids, withheld) of the profile's audit rows of `action`.
    async fn audits(&self, slug: &str, action: &str) -> anyhow::Result<Vec<AuditRow>> {
        let rows: Vec<RawAuditRow> = sqlx::query_as(
            "select tool, args_summary, resource_ids, withheld from audit_event \
             where profile_id = $1 and action = $2 order by id",
        )
        .bind(self.fam.profile(slug))
        .bind(action)
        .fetch_all(&self.db.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(t, a, r, w)| (t, a, r.unwrap_or_default(), w.unwrap_or(Value::Null)))
            .collect())
    }
}

fn genes(body: &Value) -> Vec<String> {
    body["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| f["gene"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn tile<'a>(body: &'a Value, key: &str) -> &'a Value {
    body["tiles"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["key"] == key))
        .unwrap_or(&Value::Null)
}

#[tokio::test]
async fn genome_page_for_the_owner() -> R {
    let env = genome_family().await?;
    let alex = env.login("alex", true).await?;
    let (s, body) = env.get(&alex, "/api/profiles/alex/genome").await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["dataset"]["assay"], "wgs");
    assert_eq!(body["dataset"]["source_tool"], "OwnGenome 1.4.2");
    assert_eq!(body["dataset"]["db_versions"]["clinvar"], "2026-09-28");
    assert_eq!(body["dataset"]["raw_files"]["gvcf"], true);
    let g = genes(&body);
    for want in [
        "CYP2C19", "SLCO1B1", "DPYD", "TPMT", "VKORC1", "CYP2C9", "CYP2D6", "CFTR",
    ] {
        assert!(g.iter().any(|x| x == want), "{want} missing from {g:?}");
    }
    assert!(!g.iter().any(|x| x == "APOE" || x == "MTHFR"));
    // the owner sees what is locked
    assert_eq!(body["locked"], json!([{"category": "risk_marker", "gene": "APOE"}]));
    assert_eq!(body["withheld"], json!({"sensitive": 1, "minor_rule": 0}));
    assert_eq!(
        body["consent"],
        json!({"ui": false, "agents": false, "can_change": true})
    );
    assert_eq!(tile(&body, "pgx")["value"], "7");
    assert!(
        tile(&body, "risk_marker")["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("1 hidden")
    );
    // numbers go out for the UI to word; no English title words in the PRS tile
    assert_eq!(
        (&tile(&body, "pgx")["actionable"], &tile(&body, "risk_marker")["hidden"]),
        (&json!(2), &json!(1))
    );
    let prs = tile(&body, "prs")["genes"].as_str().unwrap_or_default();
    assert!(!prs.contains("percentile") && prs.contains("PGS"), "{prs}");
    assert!(
        body["coverage"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["target"] == "FMR1" && c["state"] == "not_assessed")
    );
    assert_eq!(body["notice"], ownhealth_genome::NOTICE);
    let reads = env.audits("alex", "read").await?;
    let (tool, _, ids, withheld) = reads.last().ok_or_else(|| anyhow::anyhow!("no audit row"))?;
    assert_eq!(tool, "GET /api/profiles/{p}/genome");
    assert_eq!(withheld, &json!({"sensitive": 1, "minor_rule": 0}));
    assert!(!ids.is_empty() && ids.iter().all(|i| i.starts_with("f_")), "{ids:?}");
    Ok(())
}

#[tokio::test]
async fn a_bad_min_evidence_is_400() -> R {
    let env = genome_family().await?;
    let alex = env.login("alex", true).await?;
    let (s, body) = env.get(&alex, "/api/profiles/alex/genome?min_evidence=E9").await?;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"detail": {"code": "bad_input", "message": "min_evidence is one of ['E1', 'E2', 'E3', 'E4']"}})
    );
    Ok(())
}

#[tokio::test]
async fn viewer_without_read_genome_is_refused() -> R {
    let env = genome_family().await?;
    let sam = env.login("sam", true).await?;
    let (s, body) = env.get(&sam, "/api/profiles/alex/genome").await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::FORBIDDEN, &json!("insufficient_scope"))
    );
    Ok(())
}

#[tokio::test]
async fn consent_needs_step_up_then_reveals_in_the_ui_only() -> R {
    let env = genome_family().await?;
    let alex = env.login("alex", false).await?;
    let (s, body) = env
        .post(&alex, "/api/profiles/alex/genome/consent", json!({"ui": true}))
        .await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::FORBIDDEN, &json!("step_up_required"))
    );
    let alex = env.login("alex", true).await?;
    let (s, body) = env
        .post(&alex, "/api/profiles/alex/genome/consent", json!({"ui": true}))
        .await?;
    assert_eq!((s, body), (StatusCode::OK, json!({"ui": true, "agents": false})));
    let (_, body) = env.get(&alex, "/api/profiles/alex/genome").await?;
    assert!(genes(&body).iter().any(|g| g == "APOE"));
    assert_eq!(body["locked"], json!([]));
    // the UI switch is not consent to hand it to an agent
    let agent = env.token(&["read:genome", "read:sensitive"]);
    let body = genome::overview(&env.rt, &agent, "alex", None)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert!(!genes(&body).iter().any(|g| g == "APOE"));
    assert_eq!(body["locked"], json!([])); // tokens never get the locked list
    assert_eq!(body["consent"]["can_change"], false);
    let refused = genome::consent(&env.rt, &agent, "alex", None, Some(true)).await;
    let res = axum::response::IntoResponse::into_response(refused.err().ok_or_else(|| anyhow::anyhow!("allowed"))?);
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let rows = env.audits("alex", "consent_change").await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.as_deref(), Some("ui=True agents=None"));
    assert_eq!(rows[0].2, vec![env.fam.profile("alex").to_string()]);
    Ok(())
}

#[tokio::test]
async fn only_owner_or_guardian_of_a_minor_changes_consent() -> R {
    let env = genome_family().await?;
    let sam = env.login("sam", true).await?;
    let (viewer, _) = env
        .post(&sam, "/api/profiles/alex/genome/consent", json!({"ui": true}))
        .await?;
    assert_eq!(viewer, StatusCode::FORBIDDEN);
    let (guardian, body) = env
        .post(&sam, "/api/profiles/lucia/genome/consent", json!({"ui": true}))
        .await?;
    assert_eq!(guardian, StatusCode::OK, "{body}");
    Ok(())
}

#[tokio::test]
async fn consent_body_is_validated_as_pydantic_did() -> R {
    let env = genome_family().await?;
    let alex = env.login("alex", true).await?;
    let (s, body) = env
        .post(&alex, "/api/profiles/alex/genome/consent", json!({"ui": "maybe"}))
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["detail"][0]["type"], "bool_parsing");
    assert_eq!(body["detail"][0]["loc"], json!(["body", "ui"]));
    let (s, body) = env
        .post(&alex, "/api/profiles/alex/genome/consent", json!({"agents": "yes"}))
        .await?;
    assert_eq!((s, body), (StatusCode::OK, json!({"ui": false, "agents": true})));
    Ok(())
}

#[tokio::test]
async fn minor_page_withholds_by_the_minor_rule() -> R {
    let env = genome_family().await?;
    let alex = env.login("alex", true).await?;
    let (s, _) = env
        .post(
            &alex,
            "/api/profiles/lucia/genome/consent",
            json!({"ui": true, "agents": true}),
        )
        .await?;
    assert_eq!(s, StatusCode::OK);
    let (_, body) = env.get(&alex, "/api/profiles/lucia/genome?min_evidence=E4").await?;
    assert_eq!(genes(&body), vec!["TPMT".to_string()]);
    assert_eq!(body["withheld"], json!({"sensitive": 0, "minor_rule": 3}));
    assert_eq!(body["locked"], json!([]));
    assert_eq!(body["is_minor"], true);
    Ok(())
}

async fn lookup(
    env: &Env,
    p: &Principal,
    slug: &str,
    params: genome::VariantParams,
) -> anyhow::Result<(StatusCode, Value)> {
    match genome::variant(&env.rt, p, slug, &params).await {
        Ok(v) => Ok(v),
        Err(e) => Ok((axum::response::IntoResponse::into_response(e).status(), Value::Null)),
    }
}

fn rsid(r: &str) -> genome::VariantParams {
    genome::VariantParams {
        rsid: Some(r.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn variant_lookup_scope_gating_and_audit() -> R {
    let env = genome_family().await?;
    let no_raw = env.token(&["read:genome"]);
    assert_eq!(
        lookup(&env, &no_raw, "alex", rsid("rs4244285")).await?.0,
        StatusCode::FORBIDDEN
    );
    let raw = env.token(&["read:genome_raw"]);
    let (s, body) = lookup(&env, &raw, "alex", rsid("rs4244285")).await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["calls"][0]["call_state"], "alt");
    assert_eq!(body["notice"], ownhealth_genome::NOTICE);
    // APOE rs429358 by position: a withheld finding never resolves an rsID (that would say it exists)
    let by_pos = genome::VariantParams {
        hgvs: Some(APOE_SITE.into()),
        ..Default::default()
    };
    let (_, apoe) = lookup(&env, &raw, "alex", by_pos).await?;
    assert_eq!(apoe["withheld"], "withheld_sensitive");
    assert_eq!(apoe["calls"], json!([]));
    let (s, bad) = lookup(&env, &raw, "alex", rsid("rs123456789")).await?;
    assert_eq!(
        (s, &bad["detail"]["code"]),
        (StatusCode::BAD_REQUEST, &json!("rsid_unknown"))
    );
    let (s, _) = lookup(&env, &raw, "carmen", rsid("rs4244285")).await?;
    assert!(s == StatusCode::FORBIDDEN || s == StatusCode::NOT_FOUND, "{s}");
    let rows = env.audits("alex", "lookup").await?;
    let args: Vec<_> = rows.iter().map(|r| r.1.clone().unwrap_or_default()).collect();
    assert_eq!(args, vec!["rs4244285", APOE_SITE, "rs123456789"]);
    assert!(rows.iter().all(|r| !format!("{r:?}").contains("0/1"))); // never a genotype in the log
    Ok(())
}

#[tokio::test]
async fn variant_over_http_for_a_person_and_no_genome_is_404() -> R {
    let env = genome_family().await?;
    let alex = env.login("alex", true).await?;
    let (s, body) = env
        .get(&alex, "/api/profiles/alex/genome/variant?rsid=rs4244285")
        .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["calls"][0]["call_state"], "alt");
    // Sam has no genome: the answer is a 404 and its audit row still commits
    let sam = env.login("sam", true).await?;
    let (s, body) = env.get(&sam, "/api/profiles/sam/genome/variant?rsid=rs4244285").await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::NOT_FOUND, &json!("no_genome")),
        "{body}"
    );
    let rows: Vec<(Option<String>,)> =
        sqlx::query_as("select result_kind from audit_event where profile_id = $1 and action = 'lookup'")
            .bind(env.fam.profile("sam"))
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(rows, vec![(Some("no_genome".to_string()),)]);
    Ok(())
}
