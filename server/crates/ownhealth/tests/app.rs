//! The assembled app: probes, the SPA fallback, /api and /mcp routing, startup steps.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ownhealth::app::{build_app, check_secret_key, start};
use ownhealth_core::Runtime;
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

type R = anyhow::Result<()>;

async fn call(app: &axum::Router, req: Request<Body>) -> anyhow::Result<(StatusCode, Vec<u8>, Option<String>)> {
    let res = app.clone().oneshot(req).await?;
    let status = res.status();
    let cache = res
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = to_bytes(res.into_body(), 1 << 20).await?.to_vec();
    Ok((status, body, cache))
}

fn get(path: &str) -> anyhow::Result<Request<Body>> {
    Ok(Request::get(path).body(Body::empty())?)
}

fn json_of(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

async fn runtime(db: &TestDb, dir: &std::path::Path, ui: bool) -> anyhow::Result<Runtime> {
    let mut s = db.settings(dir)?;
    let static_dir = dir.join("static");
    std::fs::create_dir_all(&static_dir)?;
    if ui {
        std::fs::write(static_dir.join("index.html"), "<!doctype html><title>OwnHealth</title>")?;
        std::fs::create_dir_all(static_dir.join("_app"))?;
        std::fs::write(static_dir.join("_app/app.js"), "console.log(1)")?;
    }
    s.static_dir = Some(static_dir);
    Runtime::with_pool(s, db.pool.clone())
}

#[tokio::test]
async fn probes_answer_like_python() -> R {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let rt = runtime(&db, dir.path(), true).await?;
    let app = build_app(rt.clone());
    let (s, b, _) = call(&app, get("/healthz")?).await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        json_of(&b),
        json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")})
    );
    let (s, b, _) = call(&app, get("/readyz")?).await?;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_of(&b)["detail"]["code"], "migrations_pending");
    ownhealth_core::migrate::migrate_embedded(&db.pool).await?;
    let (s, b, _) = call(&app, get("/readyz")?).await?;
    assert_eq!((s, json_of(&b)), (StatusCode::OK, json!({"status": "ready"})));
    std::fs::remove_dir_all(rt.blobs.root())?;
    let (s, b, _) = call(&app, get("/readyz")?).await?;
    assert_eq!(
        (s, json_of(&b)["detail"]["code"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!("blob_dir_missing"))
    );
    Ok(())
}

#[tokio::test]
async fn the_ui_falls_back_to_index_but_api_and_mcp_paths_do_not() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let app = build_app(runtime(&db, dir.path(), true).await?);
    let (s, b, cache) = call(&app, get("/biomarkers/ldl")?).await?;
    assert!(s == StatusCode::OK && b.starts_with(b"<!doctype html>") && cache.as_deref() == Some("no-cache"));
    let (s, b, _) = call(&app, get("/_app/app.js")?).await?;
    assert!(s == StatusCode::OK && b == b"console.log(1)");
    let (s, b, _) = call(&app, get("/../../etc/passwd")?).await?;
    assert!(
        s == StatusCode::OK && b.starts_with(b"<!doctype html>"),
        "no path traversal"
    );
    for path in ["/api", "/api/nope", "/mcp/x"] {
        let (s, b, _) = call(&app, get(path)?).await?;
        assert_eq!(
            (s, json_of(&b)["detail"]["code"].clone()),
            (StatusCode::NOT_FOUND, json!("not_found")),
            "{path}"
        );
    }
    // 0.1.1 has no /metrics: the SPA answers it like any other non-API path
    let (s, _, _) = call(&app, get("/metrics")?).await?;
    assert_eq!(s, StatusCode::OK, "/metrics is the SPA");

    let bare = build_app(runtime(&db, tempfile::tempdir()?.path(), false).await?);
    let (s, b, _) = call(&bare, get("/")?).await?;
    assert_eq!(
        (s, json_of(&b)["detail"]["code"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!("ui_not_built"))
    );
    Ok(())
}

#[tokio::test]
async fn mcp_answers_initialize_at_slash_mcp() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let app = build_app(runtime(&db, dir.path(), true).await?);
    // every /mcp request, initialize included, needs a PAT (test_mcp_protocol.py)
    let mut conn = db.pool.acquire().await?;
    let fam = ownhealth_core::demo::create_rivera_family(&mut conn, "Rivera").await?;
    let tok = ownhealth_auth::tokens::create_token(
        &mut conn,
        &ownhealth_auth::tokens::TokenIn {
            user_id: fam.user("alex"),
            label: "t",
            profile_ids: vec![fam.profile("alex")],
            scopes: vec!["read:summary".into()],
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?;
    drop(conn);
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});
    let req = Request::post("/mcp")
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {}", tok.plaintext))
        .body(Body::from(init.to_string()))?;
    let (s, b, _) = call(&app, req).await?;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let v = json_of(&b);
    assert_eq!(v["result"]["serverInfo"]["name"], "OwnHealth");
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25");
    Ok(())
}

#[tokio::test]
async fn startup_migrates_and_seeds_the_demo_once() -> R {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let mut s = db.settings(dir.path())?;
    s.run_migrations = true;
    s.demo = true;
    let rt = Runtime::with_pool(s, db.pool.clone())?;
    start(&rt).await?.stop().await;
    start(&rt).await?.stop().await;
    let n: i64 = sqlx::query_scalar("select count(*) from household where name = 'Rivera (demo)'")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(n, 1);
    let profiles: i64 = sqlx::query_scalar("select count(*) from profile")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(profiles, 4);
    Ok(())
}

#[test]
fn the_dev_secret_key_is_refused_outside_dev_and_demo() -> R {
    let mut env = std::collections::HashMap::new();
    env.insert(
        "OWNHEALTH_DATABASE_URL".to_string(),
        "postgresql://x@localhost/x".to_string(),
    );
    let s = ownhealth_core::config::Settings::from(&env)?;
    assert!(check_secret_key(&s).is_err());
    assert!(
        check_secret_key(&ownhealth_core::config::Settings {
            dev_auth: true,
            ..s.clone()
        })
        .is_ok()
    );
    assert!(
        check_secret_key(&ownhealth_core::config::Settings {
            secret_key: "k".repeat(64),
            ..s
        })
        .is_ok()
    );
    Ok(())
}

/// A writer the access-log test reads back.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut v) = self.0.lock() {
            v.extend_from_slice(b);
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn one_access_line_per_request_without_the_query() -> R {
    let db = TestDb::empty().await?;
    let dir = tempfile::tempdir()?;
    let rt = runtime(&db, dir.path(), true).await?;
    let app = build_app(rt);
    let out = Captured::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let (s, _, _) = call(&app, get("/healthz?x=secret")?).await?;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(out.0.lock().map(|v| v.clone()).unwrap_or_default())?;
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("ownhealth::access")).collect();
    assert_eq!(lines.len(), 1, "{text}");
    for want in ["GET", "/healthz", "status=200", "latency_ms="] {
        assert!(lines[0].contains(want), "{want} missing: {}", lines[0]);
    }
    assert!(!text.contains("secret"), "{text}");
    Ok(())
}
