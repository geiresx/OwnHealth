//! Session lifetime, step-up, cookie flags, CSRF, PATs, and REST refusing PATs
//! (`tests/security/test_sessions.py`, `test_tokens_api.py`, `test_rest_refuses_tokens.py`,
//! `tests/test_tokens.py`).

#[path = "auth_support.rs"]
mod support;

use axum::http::{Method, StatusCode};
use ownhealth_auth::sessions::{self, create_session, require_step_up, resolve_session, revoke_session};
use ownhealth_auth::tokens::{self, TokenIn, create_token, revoke_token, verify_token};
use ownhealth_core::Error;
use ownhealth_core::access::{Principal, PrincipalKind, all_scopes, scope_set};
use serde_json::{Value, json};
use support::Env;

type R = anyhow::Result<()>;

#[tokio::test]
async fn session_lifetime() -> R {
    let env = Env::new(true).await?;
    let alex = env.user("alex");
    let mut c = env.pool().acquire().await?;
    let secret = create_session(&mut c, alex, Some("192.0.2.1"), Some("cargo test"), "passkey").await?;
    let p = resolve_session(&mut c, &secret)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no session"))?;
    assert_eq!(
        (p.kind, p.user_id, p.household_id),
        (PrincipalKind::Session, Some(alex), Some(env.fam().household_id))
    );
    let stored: Vec<u8> = sqlx::query_scalar("select secret_sha256 from user_session where user_id = $1")
        .bind(alex)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(stored.len(), 32, "only the hash is kept");
    assert!(resolve_session(&mut c, &format!("{secret}x")).await?.is_none());
    // idle for 31 minutes
    sqlx::query("update user_session set last_seen_at = now() - interval '31 minutes' where user_id = $1")
        .bind(alex)
        .execute(&mut *c)
        .await?;
    assert!(resolve_session(&mut c, &secret).await?.is_none());
    // absolute limit
    let s2 = create_session(&mut c, alex, None, None, "passkey").await?;
    sqlx::query("update user_session set expires_at = now() - interval '1 second' where secret_sha256 = sha256($1)")
        .bind(s2.as_bytes())
        .execute(&mut *c)
        .await?;
    assert!(resolve_session(&mut c, &s2).await?.is_none());
    // revoked
    let s3 = create_session(&mut c, alex, None, None, "passkey").await?;
    assert!(revoke_session(&mut c, &s3).await? && resolve_session(&mut c, &s3).await?.is_none());
    // disabled user
    let s4 = create_session(&mut c, alex, None, None, "passkey").await?;
    sqlx::query("update app_user set disabled_at = now() where id = $1")
        .bind(alex)
        .execute(&mut *c)
        .await?;
    assert!(resolve_session(&mut c, &s4).await?.is_none());
    Ok(())
}

fn step_up_refused(r: Result<(), Error>) -> bool {
    matches!(r, Err(Error::AccessDenied { code, .. }) if code == "step_up_required")
}

#[tokio::test]
async fn step_up_window() -> R {
    let env = Env::new(true).await?;
    let alex = env.user("alex");
    let mut c = env.pool().acquire().await?;
    let s = create_session(&mut c, alex, None, None, "passkey").await?;
    let fresh = resolve_session(&mut c, &s)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no session"))?;
    require_step_up(&mut c, &fresh, sessions::STEP_UP_SECONDS).await?;
    sqlx::query("update user_session set step_up_at = now() - interval '301 seconds' where id = $1")
        .bind(fresh.session_id)
        .execute(&mut *c)
        .await?;
    assert!(step_up_refused(
        require_step_up(&mut c, &fresh, sessions::STEP_UP_SECONDS).await
    ));
    let s = create_session(&mut c, alex, None, None, "recovery").await?;
    let recovered = resolve_session(&mut c, &s)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no session"))?;
    assert!(step_up_refused(
        require_step_up(&mut c, &recovered, sessions::STEP_UP_SECONDS).await
    ));
    let token = Principal {
        kind: PrincipalKind::Token,
        user_id: Some(alex),
        household_id: Some(env.fam().household_id),
        ..fresh.clone()
    };
    assert!(
        require_step_up(&mut c, &token, sessions::STEP_UP_SECONDS)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn cookie_flags() -> R {
    let env = Env::new(true).await?;
    let mut c = env.as_user(env.user("alex")).await?;
    let r = c.post0("/api/auth/logout").await?;
    let cookie = r.set_cookies().join("\n");
    assert!(cookie.starts_with("oh_session=\"\"; expires=") && cookie.ends_with("; Max-Age=0; Path=/; SameSite=lax"));
    // a login on https sets HttpOnly, Secure and SameSite=Lax
    let mut owner = env.anon();
    let codes: Vec<String> = {
        let mut tx = env.pool().begin().await?;
        let codes = ownhealth_auth::recovery::new_codes(&mut tx, env.user("alex")).await?;
        tx.commit().await?;
        codes
    };
    let r = owner.post("/api/auth/recovery", &json!({"code": codes[0]})).await?;
    let header = r.set_cookies().join("\n");
    assert!(
        header.ends_with("; HttpOnly; Max-Age=43200; Path=/; SameSite=lax; Secure"),
        "{header}"
    );
    Ok(())
}

#[tokio::test]
async fn csrf_refuses_cross_site_writes() -> R {
    let env = Env::new(true).await?;
    let mut c = env.as_user(env.user("alex")).await?;
    let body = json!({"locks": ["read:genome"]});
    let put = |h: &'static str, v: &'static str| (h, v);
    for (h, v) in [
        put("origin", "https://evil.example"),
        put("sec-fetch-site", "cross-site"),
        put("referer", "https://evil.example/page"),
    ] {
        let r = c
            .send(Method::PUT, "/api/profiles/alex/locks", Some(&body), &[(h, v)])
            .await?;
        assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "csrf_failed"), "{h}");
    }
    let r = c
        .send(
            Method::PUT,
            "/api/profiles/alex/locks",
            Some(&body),
            &[("origin", "https://testserver")],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // reads are not state-changing; the public login routes check the origin too
    let r = c
        .send(
            Method::GET,
            "/api/household",
            None,
            &[("origin", "https://evil.example")],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK);
    let r = c
        .send(
            Method::POST,
            "/api/auth/login/begin",
            None,
            &[("origin", "https://evil.example")],
        )
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    Ok(())
}

fn token_in(user: uuid::Uuid, profiles: Vec<uuid::Uuid>) -> TokenIn<'static> {
    TokenIn {
        user_id: user,
        label: "Claude Code · laptop",
        profile_ids: profiles,
        scopes: ownhealth_core::access::PAT_DEFAULT_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        default_profile: None,
        days: tokens::DEFAULT_DAYS,
        identity_mode: "full",
        lang: "es",
        units: "si",
    }
}

#[tokio::test]
async fn pat_round_trip_and_rules() -> R {
    let env = Env::new(true).await?;
    let (alex, sam) = (env.user("alex"), env.user("sam"));
    let mut c = env.pool().acquire().await?;
    let new = create_token(&mut c, &token_in(alex, vec![env.profile("alex")])).await?;
    assert!(new.plaintext.starts_with(&format!("{}_", new.prefix)) && new.plaintext.len() > 50);
    let (digest, scopes): (Vec<u8>, Vec<String>) =
        sqlx::query_as("select secret_sha256, scopes from api_token where id = $1")
            .bind(new.id)
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(digest.len(), 32);
    assert_eq!(scopes, ["read:labs", "read:summary", "read:vitals"]);
    let p = verify_token(&mut c, &new.plaintext)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no token"))?;
    assert_eq!(p.kind, PrincipalKind::Token);
    assert_eq!(p.user_id, Some(alex));
    assert_eq!(p.scopes, scope_set(["read:labs", "read:summary", "read:vitals"]));
    assert_eq!(p.default_profile, Some(env.profile("alex")));
    let last = new.plaintext.chars().last().unwrap_or('A');
    let tampered = format!(
        "{}{}",
        &new.plaintext[..new.plaintext.len() - 1],
        if last == 'A' { 'B' } else { 'A' }
    );
    assert!(verify_token(&mut c, &tampered).await?.is_none());
    assert!(verify_token(&mut c, "oh_pat_nope").await?.is_none());
    assert!(revoke_token(&mut c, alex, new.id).await?);
    assert!(verify_token(&mut c, &new.plaintext).await?.is_none());

    let refused = create_token(&mut c, &token_in(sam, vec![env.profile("carmen")])).await;
    assert!(matches!(refused, Err(Error::AccessDenied { .. })), "no grant");
    let bad = TokenIn {
        scopes: vec!["read:everything".into()],
        ..token_in(sam, vec![env.profile("sam")])
    };
    assert!(matches!(create_token(&mut c, &bad).await, Err(Error::Invalid(_))));
    let long = TokenIn {
        days: 400,
        ..token_in(sam, vec![env.profile("sam")])
    };
    assert!(matches!(create_token(&mut c, &long).await, Err(Error::Invalid(_))));
    let t = create_token(
        &mut c,
        &TokenIn {
            days: 1,
            ..token_in(sam, vec![env.profile("sam")])
        },
    )
    .await?;
    sqlx::query("update api_token set expires_at = now() - interval '1 second' where id = $1")
        .bind(t.id)
        .execute(&mut *c)
        .await?;
    assert!(verify_token(&mut c, &t.plaintext).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn tokens_api_create_list_revoke() -> R {
    let env = Env::new(true).await?;
    let alex = env.user("alex");
    let body = json!({"label": "Claude Code · laptop", "profiles": ["alex", "lucia"],
                      "scopes": ["read:summary", "read:labs"]});
    let r = env.stale(alex).await?.post("/api/tokens", &body).await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));

    let mut c = env.as_user(alex).await?;
    let r = c.post("/api/tokens", &body).await?;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    let (tok, plaintext) = (
        r.json["token"].clone(),
        r.json["plaintext"].as_str().unwrap_or_default().to_string(),
    );
    assert!(plaintext.starts_with(&format!("{}_", tok["prefix"].as_str().unwrap_or("?"))));
    assert_eq!(
        (&tok["profiles"], &tok["default_profile"]),
        (&json!(["alex", "lucia"]), &json!("alex"))
    );
    assert_eq!(
        (&tok["scopes"], &tok["identity_mode"]),
        (&json!(["read:labs", "read:summary"]), &json!("full"))
    );
    assert_eq!(
        (&tok["lang"], &tok["limits"]["rate_per_min"]),
        (&json!("es"), &json!(60))
    );

    let listed = c.get("/api/tokens").await?.json["items"].clone();
    assert_eq!(
        listed
            .as_array()
            .map(|a| a.iter().map(|t| t["id"].clone()).collect::<Vec<_>>()),
        Some(vec![tok["id"].clone()])
    );
    assert!(!listed.to_string().contains(&plaintext) && !listed.to_string().contains("secret"));
    let mut conn = env.pool().acquire().await?;
    assert!(verify_token(&mut conn, &plaintext).await?.is_some());
    let (action, args): (String, Option<String>) =
        sqlx::query_as("select action, args_summary from audit_event where token_id = $1::text::uuid")
            .bind(tok["id"].as_str())
            .fetch_one(&mut *conn)
            .await?;
    assert_eq!(action, "token_create");
    assert_eq!(
        args.as_deref(),
        Some("scopes=read:labs,read:summary;profiles=2;days=90")
    );

    // the token itself may not manage tokens, or read the audit log, or the household
    let mut as_token = env.anon();
    let bearer = format!("Bearer {plaintext}");
    for (m, path, b) in [
        (Method::GET, "/api/tokens", None),
        (Method::POST, "/api/tokens", Some(&body)),
        (Method::GET, "/api/audit", None),
        (Method::GET, "/api/household", None),
        (Method::PUT, "/api/profiles/alex/locks", Some(&json!({"locks": []}))),
        (Method::POST, "/api/profiles/alex/export", Some(&json!({"fmt": "zip"}))),
        (Method::DELETE, "/api/account?confirm=Alex%20Rivera", None),
        (Method::POST, "/api/auth/step-up/begin", None),
    ] {
        let r = as_token.send(m, path, b, &[("authorization", &bearer)]).await?;
        assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "forbidden"), "{path}");
    }

    // a profile without a grant cannot go on a token
    let mut bad = body.clone();
    bad["profiles"] = json!(["nobody"]);
    assert_eq!(c.post("/api/tokens", &bad).await?.status, StatusCode::NOT_FOUND);
    let mut sam = env.as_user(env.user("sam")).await?;
    bad["profiles"] = json!(["sam", "carmen"]);
    assert_eq!(sam.post("/api/tokens", &bad).await?.status, StatusCode::NOT_FOUND);
    let mut bad = body.clone();
    bad["scopes"] = json!(["read:everything"]);
    let r = c.post("/api/tokens", &bad).await?;
    assert_eq!((r.status, r.code()), (StatusCode::UNPROCESSABLE_ENTITY, "bad_input"));
    let mut bad = body.clone();
    bad["days"] = json!(400);
    let r = c.post("/api/tokens", &bad).await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json["detail"][0]["type"], "less_than_equal");

    // Sam cannot revoke Alex's token; Alex can, once
    let id = tok["id"].as_str().unwrap_or_default();
    assert_eq!(
        sam.delete(&format!("/api/tokens/{id}")).await?.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        c.delete(&format!("/api/tokens/{id}")).await?.json,
        json!({"id": id, "revoked": true})
    );
    assert_eq!(
        c.delete(&format!("/api/tokens/{id}")).await?.status,
        StatusCode::NOT_FOUND
    );
    assert!(verify_token(&mut conn, &plaintext).await?.is_none());
    assert_eq!(
        c.delete("/api/tokens/not-a-uuid").await?.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    Ok(())
}

#[tokio::test]
async fn a_full_scope_pat_reaches_no_lane_g_route() -> R {
    let env = Env::new(true).await?;
    let mut c = env.pool().acquire().await?;
    let tok = create_token(
        &mut c,
        &TokenIn {
            scopes: all_scopes().into_iter().collect(),
            ..token_in(env.user("alex"), vec![env.profile("alex")])
        },
    )
    .await?;
    let before = env.scalar_i64("select count(*) from audit_event").await?;
    let bearer = format!("Bearer {}", tok.plaintext);
    let mut anon = env.anon();
    for (m, path) in [
        (Method::GET, "/api/household"),
        (Method::PUT, "/api/grants"),
        (
            Method::DELETE,
            "/api/grants?user_id=00000000-0000-0000-0000-000000000000&profile=alex",
        ),
        (Method::POST, "/api/household/invites"),
        (Method::PUT, "/api/profiles/alex/sensitive"),
        (Method::DELETE, "/api/profiles/alex?confirm=alex"),
    ] {
        let body = (m != Method::GET && m != Method::DELETE).then(|| json!({}));
        let r = anon.send(m, path, body.as_ref(), &[("authorization", &bearer)]).await?;
        assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "forbidden"), "{path}");
    }
    assert_eq!(env.scalar_i64("select count(*) from audit_event").await?, before);
    let _ = Value::Null;
    Ok(())
}
