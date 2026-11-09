//! v0.2 lane E: the consent screen's REST side. A pending authorization shows the client by its CIMD
//! URL and name, each requested scope with only `read:summary` pre-checked, and the caller's profiles;
//! approving needs step-up and answers the redirect with the code; denying answers access_denied; the
//! approved clients are listed and revocable without step-up. Access tokens never reach these routes.

#[path = "auth_support.rs"]
mod support;

use axum::http::{Method, StatusCode};
use ownhealth_auth::oauth::{self, AuthorizeIn};
use serde_json::{Value, json};
use support::{Env, LAN};
use uuid::Uuid;

type R = anyhow::Result<()>;

const CLIENT: &str = "https://agent.example/oauth/client.json";
const CB: &str = "https://agent.example/cb";

/// A client already in the CIMD cache and one pending request from it.
async fn pending(env: &Env, scopes: &[&str]) -> anyhow::Result<Uuid> {
    let mut c = env.pool().acquire().await?;
    sqlx::query(
        "insert into oauth_client (client_id, client_name, redirect_uris, document, expires_at) \
         values ($1, 'Agente de prueba', $2, '{}', now() + interval '1 hour') on conflict do nothing",
    )
    .bind(CLIENT)
    .bind(vec![CB])
    .execute(&mut *c)
    .await?;
    Ok(oauth::create_request(
        &mut c,
        &AuthorizeIn {
            client_id: CLIENT.into(),
            redirect_uri: CB.into(),
            state: Some("st".into()),
            code_challenge: oauth::pkce_s256(&"v".repeat(43)),
            resource: "https://testserver/mcp".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        },
    )
    .await?)
}

fn param(u: &str, key: &str) -> Option<String> {
    u.split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
}

#[tokio::test]
async fn approving_needs_step_up_and_answers_the_redirect() -> R {
    let env = Env::new(true).await?;
    let id = pending(&env, &["read:labs", "write:propose"]).await?;
    let path = format!("/api/oauth/requests/{id}");

    let mut stale = env.stale(env.user("alex")).await?;
    let r = stale.get(&path).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["client"]["client_id"], CLIENT);
    assert_eq!(r.json["client"]["client_name"], "Agente de prueba");
    assert_eq!(r.json["redirect_host"], "agent.example");
    // the flat fields the consent screen reads
    assert_eq!(r.json["client_id"], CLIENT);
    assert_eq!(r.json["client_name"], "Agente de prueba");
    assert_eq!(r.json["redirect_uri"], CB);
    let checked: Vec<(String, bool)> = r.json["scopes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    (
                        s["scope"].as_str().unwrap_or_default().to_string(),
                        s["checked"] == json!(true),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        checked,
        [
            ("read:summary".to_string(), true),
            ("read:labs".to_string(), false),
            ("write:propose".to_string(), false)
        ]
    );
    let slugs: Vec<&str> = r.json["profiles"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| p["slug"].as_str()).collect())
        .unwrap_or_default();
    assert!(slugs.contains(&"alex") && slugs.contains(&"lucia"));

    let body = json!({"scopes": ["read:summary", "read:labs"], "profile_ids": [env.profile("alex")]});
    let r = stale.post(&format!("{path}/approve"), &body).await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));

    let mut fresh = env.as_user(env.user("alex")).await?;
    // never more than the client asked for (plus read:summary), never a profile without a grant
    let r = fresh
        .post(
            &format!("{path}/approve"),
            &json!({"scopes": ["read:genome"], "profile_ids": [env.profile("alex")]}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = fresh
        .post(
            &format!("{path}/approve"),
            &json!({"scopes": ["read:summary"], "profile_ids": [Uuid::new_v4()]}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = fresh.post(&format!("{path}/approve"), &body).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let to = r.json["redirect"].as_str().unwrap_or_default().to_string();
    assert!(to.starts_with(CB), "{to}");
    assert!(param(&to, "code").is_some_and(|c| c.len() >= 43));
    assert_eq!(param(&to, "state").as_deref(), Some("st"));
    assert_eq!(param(&to, "iss").as_deref(), Some("https%3A%2F%2Ftestserver"));
    let audited: i64 = env
        .scalar_i64("select count(*) from audit_event where action = 'oauth_approve'")
        .await?;
    assert_eq!(audited, 1);

    // a request is answered once
    let r = fresh.post(&format!("{path}/approve"), &body).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = fresh.get(&path).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // the approved client is listed, and revoking it needs no step-up
    let r = stale.get("/api/oauth/consents").await?;
    let items = r.json["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["client_id"], CLIENT);
    assert_eq!(items[0]["scopes"], json!(["read:labs", "read:summary"]));
    assert_eq!(items[0]["profiles"], json!(["alex"]));
    assert_eq!(items[0]["profile_ids"], json!([env.profile("alex")]));
    assert_eq!(items[0]["revoked_at"], Value::Null);
    let cid = items[0]["id"].as_str().unwrap_or_default().to_string();
    let r = stale.delete(&format!("/api/oauth/consents/{cid}")).await?;
    assert_eq!((r.status, r.json["revoked"].clone()), (StatusCode::OK, json!(true)));
    let r = stale.delete(&format!("/api/oauth/consents/{cid}")).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // someone else cannot revoke it
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam.delete(&format!("/api/oauth/consents/{cid}")).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn denying_answers_access_denied() -> R {
    let env = Env::new(true).await?;
    let id = pending(&env, &["read:summary"]).await?;
    let mut c = env.stale(env.user("alex")).await?;
    let r = c.post0(&format!("/api/oauth/requests/{id}/deny")).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let to = r.json["redirect"].as_str().unwrap_or_default().to_string();
    assert!(to.starts_with(CB));
    assert_eq!(param(&to, "error").as_deref(), Some("access_denied"));
    assert!(param(&to, "code").is_none());
    Ok(())
}

#[tokio::test]
async fn only_a_person_in_the_browser_reaches_the_consent_routes() -> R {
    let env = Env::new(true).await?;
    let id = pending(&env, &["read:summary"]).await?;
    let path = format!("/api/oauth/requests/{id}");
    let r = env.anon().get(&path).await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // a PAT (an agent) can never approve a client for itself
    let mut conn = env.pool().acquire().await?;
    let pat = ownhealth_auth::tokens::create_token(
        &mut conn,
        &ownhealth_auth::tokens::TokenIn {
            user_id: env.user("alex"),
            label: "agent",
            profile_ids: vec![env.profile("alex")],
            scopes: vec!["read:summary".into()],
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "es",
            units: "si",
        },
    )
    .await?;
    let bearer = format!("Bearer {}", pat.plaintext);
    let mut a = env.anon();
    let body = json!({"scopes": ["read:summary"], "profile_ids": [env.profile("alex")]});
    let r = a
        .send(
            Method::POST,
            &format!("{path}/approve"),
            Some(&body),
            &[("authorization", &bearer)],
        )
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // on a plain-http install there is no authorization server
    let mut lan = env.anon_at(LAN);
    env.log_in(&mut lan, env.user("alex"), true, "password").await?;
    let r = lan.get(&path).await?;
    assert_eq!((r.status, r.code()), (StatusCode::NOT_FOUND, "oauth_disabled"));
    Ok(())
}
