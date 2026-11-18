//! REST: `GET /api/profiles/{p}/next-draw` and `GET .../next-draw.pdf`, the next-draw planner in one
//! language and as a printable request. Alex calls over HTTP through dev auth; Sam (a viewer of Alex,
//! read:summary only) is refused.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use ownhealth_api::routes::next_draw;
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

async fn call(app: &Router, uri: &str) -> anyhow::Result<(StatusCode, axum::http::HeaderMap, Vec<u8>)> {
    let res = app.clone().oneshot(Request::get(uri).body(Body::empty())?).await?;
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = to_bytes(res.into_body(), usize::MAX).await?;
    Ok((status, headers, bytes.to_vec()))
}

async fn json(app: &Router, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
    let (st, _, bytes) = call(app, uri).await?;
    Ok((st, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

#[tokio::test]
async fn the_owner_reads_the_plan_in_both_languages_and_prints_it() -> R {
    let (s, _dir, _rt, app) = env().await?;
    let (st, en) = json(&app, "/api/profiles/alex/next-draw?as_of=2026-11-07&lang=en").await?;
    assert_eq!(st, StatusCode::OK, "{en}");
    let (_, es) = json(&app, "/api/profiles/alex/next-draw?as_of=2026-11-07&lang=es").await?;
    assert_eq!(en["state_hash"], es["state_hash"]);
    assert_eq!(en["count"], 6);
    assert_eq!(en["sections"][1]["title"], "Biotin");
    assert_eq!(es["sections"][1]["title"], "Biotina");
    assert_eq!(es["sections"][1]["items"][1]["name"], "TSH");
    assert_eq!(es["labels"]["title"], "Pruebas para la próxima analítica");

    let (st, h, pdf) = call(
        &app,
        "/api/profiles/alex/next-draw.pdf?as_of=2026-11-07&lang=es&hide_name=1",
    )
    .await?;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "application/pdf");
    assert!(pdf.starts_with(b"%PDF-"));
    let disp = h[header::CONTENT_DISPOSITION].to_str()?;
    assert_eq!(
        disp,
        "inline; filename=\"ownhealth-next-draw-unnamed-es-2026-11-07.pdf\""
    );
    let (_, again, pdf2) = call(
        &app,
        "/api/profiles/alex/next-draw.pdf?as_of=2026-11-07&lang=es&hide_name=1",
    )
    .await?;
    assert_eq!(pdf, pdf2, "the same plan prints the same bytes");
    assert_eq!(again[header::CACHE_CONTROL], "no-store");
    let (_, named, _) = call(&app, "/api/profiles/alex/next-draw.pdf?as_of=2026-11-07&lang=en").await?;
    assert!(
        named[header::CONTENT_DISPOSITION]
            .to_str()?
            .contains("ownhealth-next-draw-alex-en-")
    );

    let rows: Vec<(String, Option<Vec<String>>, Option<String>)> = sqlx::query_as(
        "select via, src_ids, args_summary from audit_event where resource_type = 'next_draw' order by id",
    )
    .fetch_all(s.pool())
    .await?;
    assert_eq!(rows.len(), 5, "{rows:?}");
    let (via, srcs, args) = &rows[2];
    assert_eq!(via, "export");
    assert!(
        srcs.as_ref().is_some_and(|s| s.contains(&"src_a05".to_string())),
        "{srcs:?}"
    );
    assert_eq!(args.as_deref(), Some("lang=es;hide_name=1;pages=1"));

    let (bad, _) = json(&app, "/api/profiles/alex/next-draw.pdf?hide_name=maybe").await?;
    assert_eq!(bad, StatusCode::UNPROCESSABLE_ENTITY);
    let (bad, _) = json(&app, "/api/profiles/alex/next-draw?as_of=yesterday").await?;
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
    let refused = next_draw::plan(&rt, &sam, "alex", None, None).await;
    let res = axum::response::IntoResponse::into_response(refused.err().ok_or_else(|| anyhow::anyhow!("allowed"))?);
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    Ok(())
}
