//! REST: `GET /api/profiles/{p}/calculators`, the ownhealth.calculators/v1 document in one language.
//! Alex calls over HTTP through dev auth; Sam (a viewer of Alex, read:summary only) is refused.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth_api::routes::calculators;
use ownhealth_core::Runtime;
use ownhealth_core::access::Principal;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

async fn env() -> anyhow::Result<(common::Seeded, tempfile::TempDir, Runtime, Router)> {
    common::install_fake_genome();
    let s = common::seeded().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = s.db.settings(dir.path())?;
    settings.dev_auth = true;
    let rt = Runtime::with_pool(settings, s.pool().clone())?;
    let app = Router::new()
        .nest("/api", ownhealth_api::router())
        .with_state(rt.clone());
    Ok((s, dir, rt, app))
}

async fn get(app: &Router, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
    let res = app.clone().oneshot(Request::get(uri).body(Body::empty())?).await?;
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

#[tokio::test]
async fn the_owner_reads_results_with_their_inputs_in_both_languages() -> R {
    let (s, _dir, _rt, app) = env().await?;
    let (st, en) = get(&app, "/api/profiles/alex/calculators?as_of=2026-11-07&lang=en").await?;
    assert_eq!(st, StatusCode::OK, "{en}");
    let (_, es) = get(&app, "/api/profiles/alex/calculators?as_of=2026-11-07&lang=es").await?;
    assert_eq!(en["calculators_hash"], es["calculators_hash"]);
    let egfr: Vec<&Value> = en["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["id"] == "ckd_epi_2021_cr")
        .collect();
    let last = egfr.last().copied().cloned().unwrap_or_default();
    assert_eq!(last["status"], "ok");
    assert_eq!(last["name"], "eGFR (CKD-EPI 2021, creatinine)");
    assert_eq!(last["inputs"][0]["group"], "creatinine");
    assert!(last["inputs"][0]["src"].as_str().is_some_and(|s| s.starts_with("src_")));
    let es_cys = es["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["id"] == "ckd_epi_2021_cr_cys")
        .cloned()
        .unwrap_or_default();
    assert_eq!(es_cys["text"], "Sin calcular: el registro no tiene cistatina C.");
    let audits: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where tool = 'GET /api/profiles/{p}/calculators' and resource_type = 'calculators' \
         and resource_ids[1] = $1",
    )
    .bind(en["calculators_hash"].as_str().unwrap_or_default())
    .fetch_one(s.pool())
    .await?;
    assert_eq!(audits, 2);
    let (bad, _) = get(&app, "/api/profiles/alex/calculators?as_of=yesterday").await?;
    assert_eq!(bad, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[tokio::test]
async fn a_viewer_without_read_labs_is_refused() -> R {
    let (s, _dir, rt, _app) = env().await?;
    let user = s.user("sam");
    let household: Uuid = sqlx::query_scalar("select household_id from app_user where id = $1")
        .bind(user)
        .fetch_one(s.pool())
        .await?;
    let sam = Principal::session(user, household);
    let refused = calculators::calculators(&rt, &sam, "alex", None, None).await;
    let res = axum::response::IntoResponse::into_response(refused.err().ok_or_else(|| anyhow::anyhow!("allowed"))?);
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// SCORE2 and PREVENT read conditions, medicines and vitals: a token with read:labs and read:vitals
/// but no read:records sees each as not assessed for its scope, and no statin, condition or src of
/// them anywhere in the body. The same call with read:records shows the statin, so the check can fail.
#[tokio::test]
async fn a_token_without_read_records_sees_no_condition_or_medicine_in_a_risk_result() -> R {
    let (s, _dir, rt, _app) = env().await?;
    let user = s.user("alex");
    let household: Uuid = sqlx::query_scalar("select household_id from app_user where id = $1")
        .bind(user)
        .fetch_one(s.pool())
        .await?;
    let token = |scopes: &[&str]| {
        let mut p = Principal::session(user, household);
        p.kind = ownhealth_core::access::PrincipalKind::Token;
        p.scopes = ownhealth_core::access::scope_set(scopes);
        p
    };
    let read = |p: Principal| {
        let rt = rt.clone();
        async move {
            calculators::calculators(&rt, &p, "alex", None, Some("en"))
                .await
                .map_err(|_| anyhow::anyhow!("refused"))
        }
    };
    let all = read(token(&["read:summary", "read:labs", "read:vitals", "read:records"])).await?;
    assert!(all.to_string().contains("src_m02"), "the control sees the statin");
    let doc = read(token(&["read:summary", "read:labs", "read:vitals"])).await?;
    let body = doc.to_string();
    for hidden in [
        "src_m02",
        "\"statin\"",
        "antihypertensive",
        "\"diabetes\"",
        "smoking_status",
    ] {
        assert!(!body.contains(hidden), "{hidden} in {body}");
    }
    let risk: Vec<&Value> = doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| {
            ["score2", "score2_op", "prevent_cvd", "prevent_ascvd"].contains(&i["id"].as_str().unwrap_or_default())
        })
        .collect();
    assert!(!risk.is_empty());
    for it in risk {
        assert_eq!(
            (it["status"].as_str(), it["reason_code"].as_str()),
            (Some("not_assessed"), Some("scope_missing")),
            "{it}"
        );
        assert_eq!(it["reason"]["scopes"], serde_json::json!(["read:records"]));
        assert_eq!(it["inputs"], serde_json::json!([]));
        assert_eq!(
            it["text"],
            "Not computed: this access does not include read:records, and the risk equation reads conditions, medicines and vital signs."
        );
    }
    // the lab calculators are unchanged
    assert!(
        doc["items"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|i| i["id"] == "ckd_epi_2021_cr" && i["status"] == "ok")
    );
    Ok(())
}
