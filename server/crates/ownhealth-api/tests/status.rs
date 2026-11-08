//! REST: status, dashboard, rules and drug-check, with scopes, the 404 for an unknown drug, FastAPI's
//! 422s and the audit rows (`0.1.1 tests/state/test_api.py`). The database is lane E's seeded demo;
//! Alex calls over HTTP through dev auth, and Sam (a viewer of Alex) calls the route functions with a
//! session principal, as Python's tests did by overriding `get_principal`.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth_api::routes::{drug_check, status};
use ownhealth_core::Runtime;
use ownhealth_core::access::Principal;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

struct Env {
    s: common::Seeded,
    _dir: tempfile::TempDir,
    rt: Runtime,
    app: Router,
}

async fn env() -> anyhow::Result<Env> {
    let s = common::seeded().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = s.db.settings(dir.path())?;
    settings.dev_auth = true;
    let rt = Runtime::with_pool(settings, s.pool().clone())?;
    let app = Router::new()
        .nest("/api", ownhealth_api::router())
        .with_state(rt.clone());
    Ok(Env { s, _dir: dir, rt, app })
}

impl Env {
    async fn get(&self, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        let res = self.app.clone().oneshot(Request::get(uri).body(Body::empty())?).await?;
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await?;
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    async fn session(&self, slug: &str) -> anyhow::Result<Principal> {
        let user = self.s.user(slug);
        let household: Uuid = sqlx::query_scalar("select household_id from app_user where id = $1")
            .bind(user)
            .fetch_one(self.s.pool())
            .await?;
        Ok(Principal::session(user, household))
    }

    /// (resource_ids, args_summary, src_ids) of the audit rows of one tool on one profile.
    async fn audits(
        &self,
        slug: &str,
        tool: &str,
    ) -> anyhow::Result<Vec<(Vec<String>, Option<String>, Option<Vec<String>>)>> {
        Ok(sqlx::query_as(
            "select resource_ids, args_summary, src_ids from audit_event where profile_id = $1 and tool = $2 order by id",
        )
        .bind(self.s.pid(slug))
        .bind(tool)
        .fetch_all(self.s.pool())
        .await?)
    }
}

#[tokio::test]
async fn dev_auth_is_alex() -> R {
    let e = env().await?;
    let mut c = e.s.pool().acquire().await?;
    let p = ownhealth_auth::principal::dev_principal(&mut c).await?;
    assert_eq!(p.and_then(|p| p.user_id), Some(e.s.user("alex")));
    Ok(())
}

#[tokio::test]
async fn status_is_the_rendered_state_and_is_audited() -> R {
    let e = env().await?;
    let (code, body) = e.get("/api/profiles/alex/status?as_of=2026-11-07&lang=en").await?;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body["state_hash"].as_str().is_some_and(|h| h.starts_with("sha256:")));
    assert!(body["data"]["flags"].as_array().is_some_and(|f| !f.is_empty()));
    let (_, es) = e.get("/api/profiles/alex/status?as_of=2026-11-07&lang=es").await?;
    assert_eq!(es["state_hash"], body["state_hash"]);
    assert_ne!(es["data"]["flags"][0]["text"], body["data"]["flags"][0]["text"]);
    let rows = e.audits("alex", "GET /api/profiles/{p}/status").await?;
    assert_eq!(
        rows.last().map(|r| r.0.clone()),
        Some(vec![body["state_hash"].as_str().unwrap_or_default().to_string()])
    );
    Ok(())
}

#[tokio::test]
async fn dashboard_and_rules() -> R {
    let e = env().await?;
    let (code, d) = e.get("/api/profiles/alex/dashboard?as_of=2026-11-07&lang=en").await?;
    assert_eq!(code, StatusCode::OK, "{d}");
    let cardio = d["systems"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["key"] == "cardiometabolic"))
        .cloned();
    assert_eq!(cardio.map(|c| c["status"].clone()), Some("out".into()));
    assert_eq!(e.audits("alex", "GET /api/profiles/{p}/dashboard").await?.len(), 1);
    let (code, rules) = e.get("/api/rules").await?;
    assert_eq!(code, StatusCode::OK);
    assert!(
        rules["version"]
            .as_str()
            .is_some_and(|v| v.starts_with("ownhealth-rules "))
    );
    assert!(rules["cpic"]["license"].as_str().is_some_and(|l| l.starts_with("CC0")));
    let all = rules["rules"].as_array().cloned().unwrap_or_default();
    assert!(
        !all.is_empty()
            && all
                .iter()
                .all(|r| r["cite"].as_str().is_some_and(|c| c.starts_with("https://")))
    );
    Ok(())
}

#[tokio::test]
async fn drug_check_owner_and_unknown_drug() -> R {
    let e = env().await?;
    let (code, r) = e.get("/api/profiles/alex/drug-check?drug=clopidogrel").await?;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert!(r["guidance"].as_array().is_some_and(|g| !g.is_empty()) && r["pgx"][0]["gene"] == "CYP2C19");
    let rows = e.audits("alex", "GET /api/profiles/{p}/drug-check").await?;
    let last = rows.last().cloned().unwrap_or_default();
    assert!(
        last.1
            .as_deref()
            .is_some_and(|a| !a.is_empty() && !a.contains("clopidogrel"))
    ); // hashed, not the name
    assert!(last.2.unwrap_or_default().contains(&"src_g07".to_string()));
    assert_eq!(last.0, ["RxNorm:32968"]);
    let (code, u) = e.get("/api/profiles/alex/drug-check?drug=notadrugzz").await?;
    assert!(
        code == StatusCode::NOT_FOUND && u["detail"]["code"] == "drug_unknown",
        "{code} {u}"
    );
    Ok(())
}

#[tokio::test]
async fn bad_query_parameters_answer_422() -> R {
    let e = env().await?;
    let (code, b) = e.get("/api/profiles/alex/drug-check").await?;
    assert!(
        code == StatusCode::UNPROCESSABLE_ENTITY && b["detail"][0]["type"] == "missing",
        "{b}"
    );
    let (code, b) = e
        .get(&format!("/api/profiles/alex/drug-check?drug={}", "a".repeat(81)))
        .await?;
    assert!(
        code == StatusCode::UNPROCESSABLE_ENTITY && b["detail"][0]["type"] == "string_too_long",
        "{b}"
    );
    let (code, b) = e.get("/api/profiles/alex/drug-check?drug=").await?;
    assert!(
        code == StatusCode::UNPROCESSABLE_ENTITY && b["detail"][0]["type"] == "string_too_short",
        "{b}"
    );
    let (code, b) = e.get("/api/profiles/alex/status?as_of=07/11/2026").await?;
    assert!(
        code == StatusCode::UNPROCESSABLE_ENTITY && b["detail"][0]["loc"] == serde_json::json!(["query", "as_of"]),
        "{b}"
    );
    Ok(())
}

#[tokio::test]
async fn a_viewer_gets_the_summary_but_no_drug_check() -> R {
    let e = env().await?;
    let sam = e.session("sam").await?;
    let refused = drug_check::drug_check(&e.rt, &sam, "alex", "clopidogrel", None).await;
    assert!(refused.is_err());
    let res = axum::response::IntoResponse::into_response(refused.err().ok_or_else(|| anyhow::anyhow!("allowed"))?);
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let q = status::StatusParams {
        as_of: Some(common::as_of()),
        ..Default::default()
    };
    let d = status::dashboard(&e.rt, &sam, "alex", &q)
        .await
        .map_err(|_| anyhow::anyhow!("dashboard refused"))?;
    let systems = d["systems"].as_array().cloned().unwrap_or_default();
    assert!(!systems.is_empty() && systems.iter().all(|c| c["withheld"] == true)); // read:summary only: no lab values
    let needs: std::collections::BTreeSet<String> = systems
        .iter()
        .filter_map(|c| c["needs"].as_str().map(str::to_string))
        .collect();
    assert_eq!(needs, ["read:labs".to_string(), "read:wearables".to_string()].into());
    let s = status::status(&e.rt, &sam, "alex", &q)
        .await
        .map_err(|_| anyhow::anyhow!("status refused"))?;
    assert!(s["withheld"]["by_scope"].as_array().is_some_and(|w| !w.is_empty()));
    Ok(())
}

#[tokio::test]
async fn no_grant_no_status() -> R {
    let e = env().await?;
    let sam = e.session("sam").await?;
    let out = status::status(&e.rt, &sam, "carmen", &status::StatusParams::default()).await;
    let res = axum::response::IntoResponse::into_response(out.err().ok_or_else(|| anyhow::anyhow!("allowed"))?);
    assert!([StatusCode::FORBIDDEN, StatusCode::NOT_FOUND].contains(&res.status()));
    Ok(())
}
