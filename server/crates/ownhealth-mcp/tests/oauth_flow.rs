//! v0.2 OAuth 2.1 (mcp.md 2.2, security-privacy.md 5 and 16): an MCP client identified by a Client ID
//! Metadata Document runs the whole flow against the built-in authorization server (discovery,
//! authorize with PKCE S256 and the resource, consent, code, token, refresh) and then calls tools on
//! `/mcp`. Tokens for another audience, plain PKCE, foreign resources, replays and CIMD documents on
//! private addresses are refused. The consent screen's REST side is tested in ownhealth-api.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use common::*;
use http::Request;
use ownhealth_auth::oauth::{self, GrantIn, pkce_s256};
use ownhealth_core::Runtime;
use serde_json::{Value, json};
use uuid::Uuid;

const BASE: &str = "https://health.example.test";
const MCP: &str = "https://health.example.test/mcp";
const CB: &str = "https://agent.example/oauth/callback";
const VERIFIER: &str = "Pq7wLr2Zt9Xk4Nb8Vc3Md6Fh1Js5Gy0Ae-Tu_Wi.Oo~Kl";

/// The MCP router served at `base`.
fn app_at(w: &World, base: &str) -> Router {
    let mut settings = (*w.rt.settings).clone();
    settings.base_url = base.into();
    let rt = Runtime {
        settings: Arc::new(settings),
        pool: w.rt.pool.clone(),
        blobs: w.rt.blobs.clone(),
    };
    ownhealth_mcp::router(rt)
}

/// A client's metadata document, served on a loopback port for an https client id nobody resolves
/// (the test route of `oauth::route_test_cimd_origin`); returns the client_id.
async fn serve_cimd(redirects: &[&str]) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let origin = format!("https://agent-{}.test", &Uuid::new_v4().simple().to_string()[..8]);
    let client_id = format!("{origin}/oauth/client.json");
    let doc = json!({
        "client_id": client_id,
        "client_name": "Agente de prueba",
        "client_uri": "https://agent.example",
        "redirect_uris": redirects,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let app = Router::new().route("/oauth/client.json", get(move || async move { axum::Json(doc) }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    oauth::route_test_cimd_origin(&origin, &local);
    Ok(client_id)
}

fn query(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

async fn authorize(app: &Router, pairs: &[(&str, &str)]) -> anyhow::Result<Http> {
    send(
        app,
        Request::get(format!("/oauth/authorize?{}", query(pairs))).body(Body::empty())?,
    )
    .await
}

async fn token(app: &Router, pairs: &[(&str, &str)]) -> anyhow::Result<Http> {
    send(
        app,
        Request::post("/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(query(pairs)))?,
    )
    .await
}

fn location(r: &Http) -> String {
    r.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn param(u: &str, key: &str) -> Option<String> {
    url::Url::parse(u)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Authorize, approve as Alex (what the consent screen does after step-up), and return the code.
async fn approved_code(w: &World, app: &Router, client_id: &str, scopes: &[&str]) -> anyhow::Result<String> {
    let challenge = pkce_s256(VERIFIER);
    let r = authorize(
        app,
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", CB),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", "st-1"),
            ("scope", "read:summary read:labs write:propose"),
            ("resource", MCP),
        ],
    )
    .await?;
    assert!(
        r.status.is_redirection(),
        "{} {}",
        r.status,
        String::from_utf8_lossy(&r.body)
    );
    let consent = location(&r);
    assert!(
        consent.starts_with(&format!("{BASE}/oauth/consent?request=")),
        "{consent}"
    );
    let id = Uuid::parse_str(&param(&consent, "request").unwrap_or_default())?;
    let mut c = w.conn().await?;
    let pending = oauth::get_request(&mut c, id).await?;
    assert_eq!(pending.client.client_name.as_deref(), Some("Agente de prueba"));
    let (_, back) = oauth::approve_request(
        &mut c,
        w.user("alex"),
        id,
        &GrantIn {
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            profile_ids: vec![w.pid("alex")],
            default_profile: None,
            identity_mode: "full".into(),
            days: None,
        },
        BASE,
    )
    .await?;
    assert!(back.starts_with(CB));
    assert_eq!(param(&back, "state").as_deref(), Some("st-1"));
    // RFC 9207: the response names its issuer
    assert_eq!(param(&back, "iss").as_deref(), Some(BASE));
    Ok(param(&back, "code").unwrap_or_default())
}

async fn exchange(app: &Router, client_id: &str, code: &str) -> anyhow::Result<Http> {
    token(
        app,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", CB),
            ("client_id", client_id),
            ("code_verifier", VERIFIER),
            ("resource", MCP),
        ],
    )
    .await
}

async fn mcp_status(app: &Router, access: &str) -> anyhow::Result<u16> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let auth = format!("Bearer {access}");
    let r = send(
        app,
        post(
            &body,
            &[("mcp-protocol-version", "2025-11-25"), ("authorization", &auth)],
        )?,
    )
    .await?;
    Ok(r.status.as_u16())
}

#[tokio::test]
async fn a_cimd_client_runs_the_whole_flow_and_calls_tools() -> R {
    let w = world().await?;
    let app = app_at(&w, BASE);
    let client_id = serve_cimd(&[CB]).await?;

    // discovery: the 401 points at the resource metadata, which names our authorization server
    let r = send(
        &app,
        post(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}), &[])?,
    )
    .await?;
    assert_eq!(r.status, 401);
    let prm = send(
        &app,
        Request::get("/.well-known/oauth-protected-resource").body(Body::empty())?,
    )
    .await?
    .json();
    assert_eq!(prm["resource"], MCP);
    assert_eq!(prm["authorization_servers"], json!([BASE]));
    let prm2 = send(
        &app,
        Request::get("/.well-known/oauth-protected-resource/mcp").body(Body::empty())?,
    )
    .await?
    .json();
    assert_eq!(prm2, prm);
    let asm = send(
        &app,
        Request::get("/.well-known/oauth-authorization-server").body(Body::empty())?,
    )
    .await?
    .json();
    assert_eq!(asm["issuer"], BASE);
    assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(asm["client_id_metadata_document_supported"], json!(true));
    assert!(asm.get("registration_endpoint").is_none(), "no DCR");

    let code = approved_code(&w, &app, &client_id, &["read:summary", "read:labs"]).await?;
    let r = exchange(&app, &client_id, &code).await?;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.headers["cache-control"], "no-store");
    let t = r.json();
    assert_eq!(t["token_type"], "Bearer");
    assert_eq!(t["scope"], "read:labs read:summary");
    assert!(t["expires_in"].as_i64().is_some_and(|s| s > 0 && s <= 3600));
    let access = t["access_token"].as_str().unwrap_or_default().to_string();
    let refresh = t["refresh_token"].as_str().unwrap_or_default().to_string();
    assert!(access.starts_with("oh_oat_"));

    // the client calls tools with what the person granted, and nothing else
    let c = Client::connect(app.clone(), Some(&access), "2026-07-28").await?;
    let names = c.tool_names().await?;
    assert!(names.contains("get_health_status") && names.contains("search_observations"));
    assert!(
        !names.contains("propose"),
        "write:propose was asked for but not granted"
    );
    let r = c.call("get_health_status", json!({"profile": "alex"})).await?;
    assert!(!r.is_error, "{}", r.raw);
    let r = c.call_tool("get_health_status", json!({"profile": "lucia"})).await?;
    assert_eq!(r.sc["code"], "profile_not_granted");
    // the audit log names the client by its CIMD URL
    let client: Option<String> = sqlx::query_scalar(
        "select client from audit_event where tool = 'get_health_status' and result_kind = 'ok' order by id desc limit 1",
    )
    .fetch_one(&w.db.pool)
    .await?;
    assert_eq!(client.as_deref(), Some(client_id.as_str()));
    // OAuth tokens are for /mcp only; the PAT routes never list them
    let mut conn = w.conn().await?;
    let pats = ownhealth_auth::tokens::list_tokens(&mut conn, w.user("alex")).await?;
    assert!(pats.iter().all(|p| p.prefix.starts_with("oh_pat_")));

    // refresh rotates both tokens; the old access token stops at once
    let r = token(
        &app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
            ("resource", MCP),
        ],
    )
    .await?;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let access2 = r.json()["access_token"].as_str().unwrap_or_default().to_string();
    assert_ne!(access2, access);
    assert_eq!(mcp_status(&app, &access).await?, 401);
    assert_eq!(mcp_status(&app, &access2).await?, 200);

    // a refresh token used twice is a theft signal: the consent and its tokens go
    let r = token(
        &app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
        ],
    )
    .await?;
    assert_eq!(
        (r.status.as_u16(), r.json()["error"].clone()),
        (400, json!("invalid_grant"))
    );
    assert_eq!(mcp_status(&app, &access2).await?, 401);

    // a replayed code fails too
    let r = exchange(&app, &client_id, &code).await?;
    assert_eq!(r.json()["error"], "invalid_grant");
    Ok(())
}

/// Someone who intercepted a code but holds no PKCE verifier (the case PKCE exists for) can neither
/// burn the code before the real client redeems it nor revoke the consent after it did.
#[tokio::test]
async fn a_code_thief_without_the_verifier_can_neither_burn_the_code_nor_revoke_the_consent() -> R {
    let w = world().await?;
    let app = app_at(&w, BASE);
    let client_id = serve_cimd(&[CB]).await?;
    let code = approved_code(&w, &app, &client_id, &["read:summary"]).await?;
    let thief = |verifier: &'static str, client: String, redirect: &'static str| {
        let (app, code) = (app.clone(), code.clone());
        async move {
            let r = token(
                &app,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", redirect),
                    ("client_id", &client),
                    ("code_verifier", verifier),
                    ("resource", MCP),
                ],
            )
            .await?;
            anyhow::Ok((r.status.as_u16(), r.json()["error"].clone()))
        }
    };
    let garbage = "Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9Zz9";
    let refused = (400, json!("invalid_grant"));
    // before the real client: a wrong verifier, a wrong redirect and a wrong client are refused ...
    assert_eq!(thief(garbage, client_id.clone(), CB).await?, refused);
    assert_eq!(
        thief(VERIFIER, client_id.clone(), "https://evil.example/cb").await?,
        refused
    );
    assert_eq!(
        thief(VERIFIER, "https://evil.example/client.json".into(), CB).await?,
        refused
    );
    // ... and the code still works for the client that holds the verifier
    let r = exchange(&app, &client_id, &code).await?;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let access = r.json()["access_token"].as_str().unwrap_or_default().to_string();
    assert_eq!(mcp_status(&app, &access).await?, 200);
    // after it: the thief's replay is refused and the person's consent stays
    assert_eq!(thief(garbage, client_id.clone(), CB).await?, refused);
    assert_eq!(mcp_status(&app, &access).await?, 200);
    // a replay that passes every check (the verifier leaked too) still revokes what the code issued
    assert_eq!(
        exchange(&app, &client_id, &code).await?.json()["error"],
        "invalid_grant"
    );
    assert_eq!(mcp_status(&app, &access).await?, 401);
    Ok(())
}

#[tokio::test]
async fn tokens_for_another_audience_are_rejected() -> R {
    let w = world().await?;
    let app = app_at(&w, BASE);
    let client_id = serve_cimd(&[CB]).await?;
    let code = approved_code(&w, &app, &client_id, &["read:summary"]).await?;
    let access = exchange(&app, &client_id, &code).await?.json()["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(mcp_status(&app, &access).await?, 200);

    // the same token presented to a server whose canonical /mcp is another URI
    let other = app_at(&w, "https://other.example.test");
    assert_eq!(mcp_status(&other, &access).await?, 401);

    // a token row minted for another resource
    sqlx::query("update api_token set audience = 'https://other.example.test/mcp' where prefix = $1")
        .bind(&access[..15])
        .execute(&w.db.pool)
        .await?;
    assert_eq!(mcp_status(&app, &access).await?, 401);
    let mut c = w.conn().await?;
    assert!(
        oauth::verify_access_token(&mut c, &access, MCP).await?.is_none(),
        "audience must equal the canonical /mcp URI"
    );
    sqlx::query("update api_token set audience = $2 where prefix = $1")
        .bind(&access[..15])
        .bind(MCP)
        .execute(&w.db.pool)
        .await?;
    assert_eq!(mcp_status(&app, &access).await?, 200);

    // revoking the consent ends the token
    let consent: Uuid = sqlx::query_scalar("select consent_id from api_token where prefix = $1")
        .bind(&access[..15])
        .fetch_one(&w.db.pool)
        .await?;
    assert!(oauth::revoke_consent(&mut c, w.user("alex"), consent).await?);
    assert_eq!(mcp_status(&app, &access).await?, 401);

    // and a token endpoint request for a foreign resource gets no token
    let code = approved_code(&w, &app, &client_id, &["read:summary"]).await?;
    let r = token(
        &app,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", CB),
            ("client_id", &client_id),
            ("code_verifier", VERIFIER),
            ("resource", "https://other.example.test/mcp"),
        ],
    )
    .await?;
    assert_eq!(r.json()["error"], "invalid_target");
    Ok(())
}

#[tokio::test]
async fn plain_pkce_foreign_resources_and_bad_redirects_are_refused() -> R {
    let w = world().await?;
    let app = app_at(&w, BASE);
    let client_id = serve_cimd(&[CB]).await?;
    let challenge = pkce_s256(VERIFIER);
    let base = [
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", CB),
        ("state", "s"),
    ];
    let cases: [(&[(&str, &str)], &str); 4] = [
        (
            &[("code_challenge", VERIFIER), ("code_challenge_method", "plain")],
            "invalid_request",
        ),
        (&[("code_challenge", VERIFIER)], "invalid_request"),
        (
            &[
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("resource", "https://other.example.test/mcp"),
            ],
            "invalid_target",
        ),
        (
            &[
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("scope", "read:everything"),
            ],
            "invalid_scope",
        ),
    ];
    for (extra, code) in cases {
        let mut pairs = base.to_vec();
        pairs.extend_from_slice(extra);
        let r = authorize(&app, &pairs).await?;
        assert!(r.status.is_redirection(), "{extra:?}");
        let to = location(&r);
        assert!(to.starts_with(CB), "{to}");
        assert_eq!(param(&to, "error").as_deref(), Some(code), "{extra:?}");
        assert_eq!(param(&to, "iss").as_deref(), Some(BASE));
    }
    let open: i64 = sqlx::query_scalar("select count(*) from oauth_request")
        .fetch_one(&w.db.pool)
        .await?;
    assert_eq!(open, 0, "nothing is stored for a refused request");

    // a redirect URI the document does not list is shown, never followed
    let r = authorize(
        &app,
        &[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", "https://attacker.example/cb"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    .await?;
    assert_eq!(r.status, 400);
    assert!(location(&r).is_empty());

    // a wrong verifier at the token endpoint
    let code = approved_code(&w, &app, &client_id, &["read:summary"]).await?;
    let r = token(
        &app,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", CB),
            ("client_id", &client_id),
            ("code_verifier", &"x".repeat(43)),
        ],
    )
    .await?;
    assert_eq!(r.json()["error"], "invalid_grant");
    Ok(())
}

#[tokio::test]
async fn cimd_documents_on_private_addresses_are_never_fetched() -> R {
    let w = world().await?;
    let app = app_at(&w, BASE);
    // a real listener on loopback, which must never be contacted
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let h = hit.clone();
    tokio::spawn(async move {
        if listener.accept().await.is_ok() {
            h.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let challenge = pkce_s256(VERIFIER);
    for client_id in [
        format!("https://127.0.0.1:{port}/client.json"),
        "https://10.1.2.3/client.json".to_string(),
        "https://[::1]/client.json".to_string(),
        "https://169.254.169.254/latest/client.json".to_string(),
        "https://localhost/client.json".to_string(),
    ] {
        let r = authorize(
            &app,
            &[
                ("response_type", "code"),
                ("client_id", &client_id),
                ("redirect_uri", CB),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
        )
        .await?;
        assert_eq!(r.status, 400, "{client_id}");
        let body = r.json();
        assert_eq!(body["error"], "invalid_client", "{client_id}");
        assert!(
            body["error_description"]
                .as_str()
                .is_some_and(|d| d.contains("private")),
            "{body}"
        );
        assert!(location(&r).is_empty());
    }
    // plain http is refused before any lookup
    let r = authorize(
        &app,
        &[
            ("response_type", "code"),
            ("client_id", "http://agent.example/client.json"),
        ],
    )
    .await?;
    assert_eq!(r.json()["error"], "invalid_client");
    assert!(
        !hit.load(std::sync::atomic::Ordering::SeqCst),
        "the private address was contacted"
    );
    let cached: i64 = sqlx::query_scalar("select count(*) from oauth_client")
        .fetch_one(&w.db.pool)
        .await?;
    assert_eq!(cached, 0);
    Ok(())
}

#[tokio::test]
async fn on_plain_http_there_is_no_authorization_server() -> R {
    let w = world().await?;
    let app = w.real_app();
    for path in ["/.well-known/oauth-authorization-server", "/oauth/authorize"] {
        let r = send(&app, Request::get(path).body(Body::empty())?).await?;
        assert_eq!(r.status, 404, "{path}");
    }
    let prm: Value = send(
        &app,
        Request::get("/.well-known/oauth-protected-resource").body(Body::empty())?,
    )
    .await?
    .json();
    assert!(prm.get("authorization_servers").is_none());
    // PATs keep working exactly as before
    let tok = w.tok().await?;
    assert_eq!(mcp_status(&app, &tok).await?, 200);
    Ok(())
}
