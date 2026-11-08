//! Generic OIDC login against a fake provider on 127.0.0.1: code flow + PKCE, a signed ID token,
//! state, nonce and the browser binder (`tests/security/test_oidc.py`).

#[path = "auth_support.rs"]
mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ownhealth_auth::oidc::OidcConfig;
use ownhealth_auth::testing::{EcKey as SigningKey, es256_jwt, jwk, s256, signing_key};
use serde_json::{Map, Value, json};
use support::{Client, Env};

type R = anyhow::Result<()>;

struct Grant {
    nonce: String,
    challenge: String,
    sub: String,
    name: String,
}

struct Idp {
    issuer: String,
    key: SigningKey,
    sign_with: Option<SigningKey>,
    codes: HashMap<String, Grant>,
    over: Map<String, Value>,
}

type Shared = Arc<Mutex<Idp>>;

async fn discovery(State(s): State<Shared>) -> Json<Value> {
    let issuer = s.lock().map(|i| i.issuer.clone()).unwrap_or_default();
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
    }))
}

async fn jwks(State(s): State<Shared>) -> Json<Value> {
    let keys = s.lock().map(|i| vec![jwk(&i.key, "k1")]).unwrap_or_default();
    Json(json!({"keys": keys}))
}

async fn token(State(s): State<Shared>, Form(form): Form<HashMap<String, String>>) -> Response {
    let Ok(mut idp) = s.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(grant) = form.get("code").and_then(|c| idp.codes.remove(c)) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if s256(&verifier) != grant.challenge {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "error_description": "pkce"})),
        )
            .into_response();
    }
    let now = chrono::Utc::now().timestamp();
    let mut claims = json!({
        "iss": idp.issuer, "aud": "ownhealth", "sub": grant.sub, "exp": now + 300, "iat": now,
        "auth_time": now, "nonce": grant.nonce, "name": grant.name,
    });
    for (k, v) in &idp.over {
        claims[k] = v.clone();
    }
    let key = idp.sign_with.clone().unwrap_or_else(|| idp.key.clone());
    let id_token = es256_jwt(&key, "k1", &claims);
    Json(json!({"id_token": id_token, "access_token": "at", "token_type": "Bearer"})).into_response()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(url: &str) -> HashMap<String, String> {
    url.split_once('?')
        .map(|(_, q)| q)
        .unwrap_or_default()
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

struct Fake {
    idp: Shared,
    issuer: String,
}

impl Fake {
    async fn start() -> anyhow::Result<Fake> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let issuer = format!("http://{}", listener.local_addr()?);
        let idp = Arc::new(Mutex::new(Idp {
            issuer: issuer.clone(),
            key: signing_key(None),
            sign_with: None,
            codes: HashMap::new(),
            over: Map::new(),
        }));
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(idp.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Fake { idp, issuer })
    }

    fn config(&self) -> OidcConfig {
        let mut cfg = OidcConfig::new(&self.issuer, "ownhealth");
        cfg.client_secret = Some("s".into());
        cfg.label = "Pocket ID".into();
        cfg
    }

    fn set(&self, sign_with: Option<SigningKey>, over: Value) {
        if let Ok(mut i) = self.idp.lock() {
            i.sign_with = sign_with;
            i.over = over.as_object().cloned().unwrap_or_default();
        }
    }

    /// What the provider does after the person logs in: the callback query (and the request's).
    fn authorize(&self, location: &str, sub: &str) -> (String, HashMap<String, String>) {
        let q = query(location);
        assert_eq!(
            (q["code_challenge_method"].as_str(), q["client_id"].as_str()),
            ("S256", "ownhealth")
        );
        let mut idp = match self.idp.lock() {
            Ok(i) => i,
            Err(e) => e.into_inner(),
        };
        let code = format!("code-{}-{}", idp.codes.len(), &q["state"][..6]);
        idp.codes.insert(
            code.clone(),
            Grant {
                nonce: q["nonce"].clone(),
                challenge: q["code_challenge"].clone(),
                sub: sub.into(),
                name: "Sam Rivera".into(),
            },
        );
        (format!("state={}&code={code}", q["state"]), q)
    }

    fn wire(&self, env: &Env, mut client: Client) -> Client {
        let cookies = std::mem::take(&mut client.cookies);
        let mut c = env.client_with(&env.settings.base_url, Some(self.config()));
        c.cookies = cookies;
        c
    }

    async fn go(
        &self,
        c: &mut Client,
        sub: &str,
        params: &str,
    ) -> anyhow::Result<(support::Resp, HashMap<String, String>)> {
        let r = c.get(&format!("/api/auth/oidc/login?{params}")).await?;
        assert_eq!(r.status, StatusCode::FOUND, "{:?}", r.json);
        let (cb, q) = self.authorize(&r.location(), sub);
        Ok((c.get(&format!("/api/auth/oidc/callback?{cb}")).await?, q))
    }
}

#[tokio::test]
async fn first_run_then_login_and_rejections() -> R {
    let env = Env::new(false).await?;
    let idp = Fake::start().await?;
    let mut c = idp.wire(&env, env.anon());
    assert_eq!(
        c.get("/api/auth/state").await?.json["oidc"],
        json!({"enabled": true, "label": "Pocket ID"})
    );
    let (r, q) = idp.go(&mut c, "sub-sam", "mode=login").await?;
    assert_eq!((r.status, r.location()), (StatusCode::FOUND, "/".to_string()));
    assert_eq!(q["redirect_uri"], "https://testserver/api/auth/oidc/callback");
    let me = c.me().await?;
    assert_eq!(me["display_name"], "Sam Rivera");
    let subject: String = sqlx::query_scalar("select oidc_subject from app_user")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(subject, format!("{}#sub-sam", idp.issuer));
    let role: String = sqlx::query_scalar("select role from profile_grant")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(role, "owner");

    // the same person again: a new session; a stranger: not linked
    let mut again = idp.wire(&env, env.anon());
    assert_eq!(idp.go(&mut again, "sub-sam", "mode=login").await?.0.location(), "/");
    assert_ne!(again.me().await?, Value::Null);
    let mut stranger = idp.wire(&env, env.anon());
    let r = idp.go(&mut stranger, "sub-mallory", "mode=login").await?.0;
    assert_eq!(r.location(), "/login?auth_error=oidc_not_linked");
    assert_eq!(stranger.me().await?, Value::Null);

    // a forged signature, a token for another client, a wrong nonce: all refused
    for tamper in ["key", "aud", "nonce"] {
        let mut bad = idp.wire(&env, env.anon());
        match tamper {
            "key" => idp.set(Some(signing_key(None)), json!({})),
            "aud" => idp.set(None, json!({"aud": "another-app"})),
            _ => idp.set(None, json!({"nonce": "replayed"})),
        }
        let r = idp.go(&mut bad, "sub-sam", "mode=login").await?.0;
        assert_eq!(r.location(), "/login?auth_error=oidc_failed", "{tamper}");
        assert_eq!(bad.me().await?, Value::Null, "{tamper}");
    }
    idp.set(None, json!({}));

    // a state is single use
    let r = c.get("/api/auth/oidc/login").await?;
    let (cb, _) = idp.authorize(&r.location(), "sub-sam");
    assert_eq!(c.get(&format!("/api/auth/oidc/callback?{cb}")).await?.location(), "/");
    let replay = c.get(&format!("/api/auth/oidc/callback?{cb}")).await?;
    assert!(
        replay.location().contains("auth_error=challenge_invalid"),
        "{}",
        replay.location()
    );
    // the provider said no
    let r = c.get("/api/auth/oidc/callback?error=access_denied").await?;
    assert_eq!(r.location(), "/login?auth_error=oidc_access_denied");
    Ok(())
}

#[tokio::test]
async fn enroll_by_invite_and_step_up() -> R {
    let env = Env::new(true).await?;
    let idp = Fake::start().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let inv = alex
        .post(
            "/api/household/invites",
            &json!({"display_name": "Carmen Ortega", "claim_profile": "carmen"}),
        )
        .await?;
    let code = inv.json["code"].as_str().unwrap_or_default().to_string();
    let sub = format!("sub-carmen-{}", &code[..6]);
    let mut carmen = idp.wire(&env, env.anon());
    let (r, _) = idp.go(&mut carmen, &sub, &format!("mode=enroll&invite={code}")).await?;
    assert_eq!(r.location(), "/");
    let profiles = carmen.get("/api/household").await?.json["profiles"].clone();
    assert_eq!(
        profiles.as_array().map(|p| p
            .iter()
            .map(|x| (x["slug"].clone(), x["role"].clone()))
            .collect::<Vec<_>>()),
        Some(vec![(json!("carmen"), json!("owner"))])
    );

    // step-up through the provider: only the same subject counts
    let uid: uuid::Uuid = sqlx::query_scalar("select user_id from profile where id = $1")
        .bind(env.profile("carmen"))
        .fetch_one(env.pool())
        .await?;
    let mut stale = idp.wire(&env, env.login(uid, false, "oidc").await?);
    let begin = stale.post0("/api/auth/step-up/begin").await?.json;
    assert_eq!(
        begin,
        json!({"method": "oidc", "url": "/api/auth/oidc/login?mode=step_up"})
    );
    let (r, q) = idp.go(&mut stale, "sub-someone-else", "mode=step_up").await?;
    assert_eq!((q["prompt"].as_str(), q["max_age"].as_str()), ("login", "0"));
    assert!(r.location().contains("oidc_wrong_user"), "{}", r.location());
    assert!(!stale.step_up_fresh().await?);
    let (r, _) = idp.go(&mut stale, &sub, "mode=step_up").await?;
    assert_eq!(r.location(), "/");
    assert!(stale.step_up_fresh().await?);
    Ok(())
}

#[tokio::test]
async fn the_callback_only_counts_in_the_browser_that_started_it() -> R {
    let env = Env::new(false).await?;
    let idp = Fake::start().await?;
    let mut owner = idp.wire(&env, env.anon());
    assert_eq!(idp.go(&mut owner, "sub-sam", "mode=login").await?.0.location(), "/");
    let uid: uuid::Uuid = sqlx::query_scalar("select id from app_user")
        .fetch_one(env.pool())
        .await?;

    // link CSRF: the attacker (logged in as the owner here) starts a link and sends the URL to a victim
    let mut attacker = idp.wire(&env, env.login(uid, true, "oidc").await?);
    let r = attacker.get("/api/auth/oidc/login?mode=link").await?;
    let (cb, _) = idp.authorize(&r.location(), "sub-victim");
    let mut victim = idp.wire(&env, env.anon());
    let r = victim.get(&format!("/api/auth/oidc/callback?{cb}")).await?;
    assert!(r.location().contains("auth_error=oidc_failed"), "{}", r.location());
    let subject: String = sqlx::query_scalar("select oidc_subject from app_user where id = $1")
        .bind(uid)
        .fetch_one(env.pool())
        .await?;
    assert!(subject.ends_with("#sub-sam"));

    // login CSRF: an unused callback URL for the attacker's account does not log a victim in
    let r = attacker.get("/api/auth/oidc/login").await?;
    let (cb, _) = idp.authorize(&r.location(), "sub-sam");
    let mut victim = idp.wire(&env, env.anon());
    let r = victim.get(&format!("/api/auth/oidc/callback?{cb}")).await?;
    assert!(r.location().contains("auth_error=oidc_failed"), "{}", r.location());
    assert_eq!(victim.me().await?, Value::Null);

    // a step-up started by one session does not mark another session of the same browser
    let mut stale = idp.wire(&env, env.login(uid, false, "oidc").await?);
    let r = stale.get("/api/auth/oidc/login?mode=step_up").await?;
    let (cb, _) = idp.authorize(&r.location(), "sub-sam");
    let mut other = env.login(uid, false, "oidc").await?;
    if let Some(s) = other.cookies.get("oh_session").cloned() {
        stale.cookies.insert("oh_session".into(), s);
    }
    let r = stale.get(&format!("/api/auth/oidc/callback?{cb}")).await?;
    assert!(r.location().contains("auth_error=oidc_wrong_user"), "{}", r.location());
    assert!(!other.step_up_fresh().await?);
    Ok(())
}

#[tokio::test]
async fn step_up_needs_proof_of_a_fresh_provider_login() -> R {
    let env = Env::new(false).await?;
    let idp = Fake::start().await?;
    let mut owner = idp.wire(&env, env.anon());
    assert_eq!(idp.go(&mut owner, "sub-sam", "mode=login").await?.0.location(), "/");
    let uid: uuid::Uuid = sqlx::query_scalar("select id from app_user")
        .fetch_one(env.pool())
        .await?;
    let mut stale = idp.wire(&env, env.login(uid, false, "oidc").await?);
    idp.set(None, json!({"auth_time": null})); // a provider that reused an old SSO session and says nothing
    let (r, _) = idp.go(&mut stale, "sub-sam", "mode=step_up").await?;
    assert!(r.location().contains("auth_error=oidc_not_fresh"), "{}", r.location());
    assert!(!stale.step_up_fresh().await?);
    Ok(())
}

#[tokio::test]
async fn linking_a_provider_needs_a_fresh_login() -> R {
    let env = Env::new(true).await?;
    let idp = Fake::start().await?;
    let mut stale = idp.wire(&env, env.stale(env.user("alex")).await?);
    let r = stale.get("/api/auth/oidc/login?mode=link").await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));
    let mut fresh = idp.wire(&env, env.as_user(env.user("alex")).await?);
    let r = fresh.get("/api/auth/oidc/login?mode=link").await?;
    assert_eq!(r.status, StatusCode::FOUND);
    let binder = r.set_cookies().join("\n");
    assert!(
        binder.starts_with("oh_oidc=")
            && binder.ends_with("; HttpOnly; Max-Age=600; Path=/api/auth/oidc; SameSite=lax; Secure"),
        "{binder}"
    );
    // without a provider configured the routes answer 404
    let r = env.anon().get("/api/auth/oidc/login").await?;
    assert_eq!((r.status, r.code()), (StatusCode::NOT_FOUND, "oidc_disabled"));
    Ok(())
}
