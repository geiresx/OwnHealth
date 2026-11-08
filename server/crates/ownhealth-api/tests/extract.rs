//! The caller extractor and the error mapping every route relies on.

use axum::body::{Body, to_bytes};
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use ownhealth_api::{ApiResult, Caller};
use ownhealth_core::Runtime;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

type R = anyhow::Result<()>;

async fn whoami(caller: Caller) -> Json<Value> {
    Json(json!({"kind": caller.0.kind, "user": caller.0.user_id}))
}

async fn profile(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:labs"), None).await?;
    tx.commit().await?;
    Ok(Json(json!({"slug": perm.profile.slug, "role": perm.role})))
}

fn app(rt: Runtime) -> Router {
    Router::new()
        .route("/whoami", get(whoami))
        .route("/change", post(whoami))
        .route("/profiles/{p}", get(profile))
        .with_state(rt)
}

async fn call(app: &Router, req: Request<Body>) -> anyhow::Result<(StatusCode, Value)> {
    let res = app.clone().oneshot(req).await?;
    let status = res.status();
    let body = to_bytes(res.into_body(), 1 << 20).await?;
    Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

#[tokio::test]
async fn callers_are_resolved_checked_and_mapped() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    let rt = Runtime::with_pool(settings.clone(), db.pool.clone())?;
    let (s, v) = call(&app(rt), Request::get("/whoami").body(Body::empty())?).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("unauthenticated"))
    );

    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    drop(c);
    settings.dev_auth = true;
    let app = app(Runtime::with_pool(settings, db.pool.clone())?);
    let (s, v) = call(&app, Request::get("/whoami").body(Body::empty())?).await?;
    assert_eq!(
        (s, v),
        (StatusCode::OK, json!({"kind": "dev", "user": fam.user("alex")}))
    );

    let cross = Request::post("/change")
        .header("origin", "https://evil.example")
        .body(Body::empty())?;
    let (s, v) = call(&app, cross).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::FORBIDDEN, json!("csrf_failed"))
    );
    let same = Request::post("/change")
        .header("host", "localhost:8080")
        .header("origin", "http://localhost:8080")
        .body(Body::empty())?;
    assert_eq!(call(&app, same).await?.0, StatusCode::OK);

    let (s, v) = call(&app, Request::get("/profiles/carmen").body(Body::empty())?).await?;
    assert_eq!((s, v), (StatusCode::OK, json!({"slug": "carmen", "role": "caregiver"})));
    let (s, v) = call(&app, Request::get("/profiles/nobody").body(Body::empty())?).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["detail"]["code"], "profile_not_granted");
    assert!(v["detail"]["allowed"].is_array());
    let (s, v) = call(
        &app,
        Request::get(format!("/profiles/{}", fam.profile("sam"))).body(Body::empty())?,
    )
    .await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::FORBIDDEN, json!("insufficient_scope"))
    );
    Ok(())
}
