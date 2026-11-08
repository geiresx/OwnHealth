//! Lane F (`tests/mcp/test_mcp_limits.py::test_usage_route`): `GET /tokens/{id}/usage` shows the
//! owner what one MCP token used, and answers 404 to anyone else.

use axum::body::{Body, to_bytes};
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_auth::tokens::{TokenIn, create_token};
use ownhealth_core::Runtime;
use ownhealth_core::access::all_scopes;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

type R = anyhow::Result<()>;

async fn body(res: axum::response::Response) -> anyhow::Result<(StatusCode, Value)> {
    let status = res.status();
    let b = to_bytes(res.into_body(), 1 << 20).await?;
    Ok((status, serde_json::from_slice(&b).unwrap_or(Value::Null)))
}

#[tokio::test]
async fn usage_route() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    let tok = create_token(
        &mut c,
        &TokenIn {
            user_id: fam.user("alex"),
            label: "Claude Code · test",
            profile_ids: vec![fam.profile("alex")],
            scopes: all_scopes().into_iter().collect(),
            default_profile: None,
            days: 90,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?;
    // one real MCP call (2026-07-28 stateless request)
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "get_health_status", "arguments": {"profile": "alex"},
                   "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                             "io.modelcontextprotocol/clientCapabilities": {}}}});
    let req = Request::post("/mcp")
        .header("host", "127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::AUTHORIZATION, format!("Bearer {}", tok.plaintext))
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", "get_health_status")
        .body(Body::from(call.to_string()))?;
    let (s, r) = body(ownhealth_mcp::router(rt.clone()).oneshot(req).await?).await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["result"]["isError"], json!(false), "{r}");

    let api = ownhealth_api::router().with_state(rt.clone());
    let get = |who: &str, cookie: String| {
        let _ = who;
        Request::get(format!("/tokens/{}/usage", tok.id))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
    };
    let alex = format!(
        "{COOKIE}={}",
        create_session(&mut c, fam.user("alex"), None, None, "passkey").await?
    );
    let (s, b) = body(api.clone().oneshot(get("alex", alex.clone())?).await?).await?;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["today"]["calls"], 1);
    assert_eq!(b["limits"]["rate_per_min"], 60);
    assert!(b["history"].as_array().is_some_and(|h| !h.is_empty()));
    let sam = format!(
        "{COOKIE}={}",
        create_session(&mut c, fam.user("sam"), None, None, "passkey").await?
    );
    let (s, b) = body(api.clone().oneshot(get("sam", sam)?).await?).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b, json!({"detail": {"code": "unknown", "message": "No such token."}}));
    // a path that is not a UUID is FastAPI's 422
    let req = Request::get("/tokens/nope/usage")
        .header(header::COOKIE, alex)
        .body(Body::empty())?;
    let (s, b) = body(api.oneshot(req).await?).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["detail"][0]["type"], "uuid_parsing");
    Ok(())
}
