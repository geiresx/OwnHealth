//! First-run setup, passkey login, recovery codes, step-up and the password fallback, through the
//! REST API (`tests/security/test_passkeys.py`, `test_password.py`).

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use ownhealth_auth::passkeys::{self, Rp};
use ownhealth_auth::testing::SoftKey;
use serde_json::{Value, json};
use support::{BASE, Client, Env, LAN};

type R = anyhow::Result<()>;

async fn setup_owner(client: &mut Client, key: &mut SoftKey) -> anyhow::Result<Value> {
    let opts = client
        .post(
            "/api/auth/setup/begin",
            &json!({"household_name": "Rivera", "display_name": "Alex Rivera"}),
        )
        .await?
        .json;
    assert_eq!(opts["authenticatorSelection"]["residentKey"], "required");
    let r = client
        .post(
            "/api/auth/setup/finish",
            &json!({"credential": key.create(&opts, None), "label": "laptop"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    Ok(r.json)
}

#[tokio::test]
async fn first_run_setup_then_login() -> R {
    let env = Env::new(false).await?;
    let mut anon = env.anon();
    assert_eq!(anon.get("/api/auth/state").await?.json["setup_needed"], true);
    let mut key = SoftKey::default();
    let out = setup_owner(&mut anon, &mut key).await?;
    assert_eq!(out["profile"], "alex");
    assert_eq!(out["recovery_codes"].as_array().map(Vec::len), Some(10));
    // the setup response set the session cookie
    let state = anon.get("/api/auth/state").await?.json;
    assert_eq!(state["user"]["display_name"], "Alex Rivera");
    assert_eq!(
        (
            &state["setup_needed"],
            &state["user"]["passkeys"],
            &state["step_up_fresh"]
        ),
        (&json!(false), &json!(1), &json!(true))
    );
    let grants: Vec<(String, String)> =
        sqlx::query_as("select p.slug, g.role from profile_grant g join profile p on p.id = g.profile_id")
            .fetch_all(env.pool())
            .await?;
    assert_eq!(grants, vec![("alex".to_string(), "owner".to_string())]);

    // a second setup is refused: the instance has an owner
    let mut again = env.anon();
    let r = again
        .post("/api/auth/setup/begin", &json!({"display_name": "Mallory"}))
        .await?;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // log out, then log back in with the passkey
    let r = anon.post0("/api/auth/logout").await?;
    assert_eq!(r.json, json!({"ok": true}));
    assert!(r.set_cookies()[0].starts_with("oh_session=\"\"; expires="));
    assert_eq!(anon.me().await?, Value::Null);
    let opts = anon.post0("/api/auth/login/begin").await?.json;
    let assertion = key.get(&opts, None, None);
    let r = anon
        .post("/api/auth/login/finish", &json!({"credential": assertion}))
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(anon.me().await?["display_name"], "Alex Rivera");
    // the same assertion again, or a new signature over the same challenge: the challenge is used up
    let mut again = env.anon();
    let r = again
        .post("/api/auth/login/finish", &json!({"credential": assertion}))
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let resigned = key.get(&opts, None, None); // higher sign count
    let r = again
        .post("/api/auth/login/finish", &json!({"credential": resigned}))
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, "challenge_invalid"));
    // the re-signed attempt was refused
    assert_eq!(
        env.scalar_i64("select sign_count from passkey").await?,
        i64::from(key.count - 1)
    );
    assert_eq!(
        env.scalar_i64("select count(*) from audit_event where action = 'login'")
            .await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn login_rejects_wrong_origin_and_unknown_key() -> R {
    let env = Env::new(false).await?;
    let mut key = SoftKey::default();
    setup_owner(&mut env.anon(), &mut key).await?;
    let mut c = env.anon();
    let opts = c.post0("/api/auth/login/begin").await?.json;
    let bad_origin = key.get(&opts, Some("https://evil.example"), None);
    let r = c
        .post("/api/auth/login/finish", &json!({"credential": bad_origin}))
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let mut stranger = SoftKey {
        user_handle: Some(vec![b'x'; 16]),
        ..Default::default()
    };
    let opts = c.post0("/api/auth/login/begin").await?.json;
    let r = c
        .post(
            "/api/auth/login/finish",
            &json!({"credential": stranger.get(&opts, None, None)}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::UNAUTHORIZED, "passkey_unknown"));
    let opts = c.post0("/api/auth/login/begin").await?.json;
    let mut forged = key.get(&opts, None, None);
    let sig = forged["response"]["signature"].as_str().unwrap_or_default().to_string();
    forged["response"]["signature"] = json!(format!("{}AAAA", &sig[..sig.len() - 4]));
    let r = c.post("/api/auth/login/finish", &json!({"credential": forged})).await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // the wrong RP id
    let opts = c.post0("/api/auth/login/begin").await?.json;
    let other_rp = key.get(&opts, None, Some("evil.example"));
    let r = c
        .post("/api/auth/login/finish", &json!({"credential": other_rp}))
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(c.me().await?, Value::Null);
    Ok(())
}

#[tokio::test]
async fn recovery_code_logs_in_once_without_step_up() -> R {
    let env = Env::new(false).await?;
    let mut key = SoftKey::default();
    let out = setup_owner(&mut env.anon(), &mut key).await?;
    let codes: Vec<String> = serde_json::from_value(out["recovery_codes"].clone())?;
    let mut c = env.anon();
    let r = c.post("/api/auth/recovery", &json!({"code": "0000-0000-0000"})).await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // case and dashes do not matter
    let r = c
        .post("/api/auth/recovery", &json!({"code": codes[3].to_lowercase()}))
        .await?;
    assert_eq!((r.status, &r.json["recovery_codes_left"]), (StatusCode::OK, &json!(9)));
    let state = c.get("/api/auth/state").await?.json;
    assert_eq!(
        (&state["method"], &state["step_up_fresh"]),
        (&json!("recovery"), &json!(false))
    );
    // step-up-gated: creating a token
    let body = json!({"label": "x", "profiles": ["alex"]});
    assert_eq!(c.post("/api/tokens", &body).await?.code(), "step_up_required");
    // the code is used up
    let r = env
        .anon()
        .post("/api/auth/recovery", &json!({"code": codes[3]}))
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // after a recovery login the person adds a new passkey, and step-up works with it
    let mut new_key = SoftKey::default();
    let opts = c.post0("/api/auth/passkeys/begin").await?.json;
    assert_eq!(opts["excludeCredentials"].as_array().map(Vec::len), Some(1));
    let r = c
        .post(
            "/api/auth/passkeys/finish",
            &json!({"credential": new_key.create(&opts, None)}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let begin = c.post0("/api/auth/step-up/begin").await?.json;
    assert_eq!(begin["method"], "passkey");
    let r = c
        .post(
            "/api/auth/step-up/finish",
            &json!({"credential": new_key.get(&begin["options"], None, None)}),
        )
        .await?;
    assert_eq!(r.json["ok"], true, "{:?}", r.json);
    assert_eq!(c.post("/api/tokens", &body).await?.status, StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
async fn step_up_rejects_another_users_passkey() -> R {
    let env = Env::new(false).await?;
    let mut key = SoftKey::default();
    setup_owner(&mut env.anon(), &mut key).await?;
    let (hh, alex): (uuid::Uuid, uuid::Uuid) = sqlx::query_as("select household_id, id from app_user")
        .fetch_one(env.pool())
        .await?;
    let sam: uuid::Uuid =
        sqlx::query_scalar("insert into app_user (household_id, display_name) values ($1, 'Sam Rivera') returning id")
            .bind(hh)
            .fetch_one(env.pool())
            .await?;
    // added from a fresh session (adding a passkey needs one), then used from a stale one
    let mut sam_key = SoftKey::default();
    let mut sam_fresh = env.as_user(sam).await?;
    let opts = sam_fresh.post0("/api/auth/passkeys/begin").await?.json;
    let r = sam_fresh
        .post(
            "/api/auth/passkeys/finish",
            &json!({"credential": sam_key.create(&opts, None)}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK);
    let mut sam_client = env.stale(sam).await?;
    let begin = sam_client.post0("/api/auth/step-up/begin").await?.json;
    // Alex's key answering Sam's step-up challenge: refused, and the session stays not fresh
    let r = sam_client
        .post(
            "/api/auth/step-up/finish",
            &json!({"credential": key.get(&begin["options"], None, None)}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(!sam_client.step_up_fresh().await?);
    // Sam's own challenge and passkey, handed to Alex's stale session: refused too
    let mut alex_client = env.stale(alex).await?;
    let begin = sam_client.post0("/api/auth/step-up/begin").await?.json;
    let r = alex_client
        .post(
            "/api/auth/step-up/finish",
            &json!({"credential": sam_key.get(&begin["options"], None, None)}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(!alex_client.step_up_fresh().await?);

    // finish_login itself refuses a step-up challenge answered with another user's passkey
    let rp = Rp::from_base_url(BASE);
    let mut tx = env.pool().begin().await?;
    let opts = passkeys::begin_login(&mut tx, &rp, Some(sam), None).await?;
    let res = passkeys::finish_login(&mut tx, &key.get(&opts, None, None), &rp, "step_up").await;
    assert!(res.is_err(), "a step-up challenge for Sam is never answered by Alex");
    let _ = alex;
    Ok(())
}

#[tokio::test]
async fn adding_a_passkey_needs_a_fresh_login() -> R {
    let env = Env::new(true).await?;
    let alex = env.user("alex");
    // a session without a fresh login cannot plant a passkey it could later step up with
    let r = env.stale(alex).await?.post0("/api/auth/passkeys/begin").await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));
    let r = env.as_user(alex).await?.post0("/api/auth/passkeys/begin").await?;
    assert_eq!(r.status, StatusCode::OK);
    // a recovery-code login may add one right after it logs in, not for the rest of the session
    let mut recovered = env.login(alex, false, "recovery").await?;
    assert_eq!(
        recovered.post0("/api/auth/passkeys/begin").await?.status,
        StatusCode::OK
    );
    sqlx::query(
        "update user_session set created_at = now() - interval '6 minutes' where user_id = $1 and method = 'recovery'",
    )
    .bind(alex)
    .execute(env.pool())
    .await?;
    let r = recovered.post0("/api/auth/passkeys/begin").await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));
    Ok(())
}

#[tokio::test]
async fn removing_the_last_way_in_is_refused() -> R {
    let env = Env::new(false).await?;
    let mut key = SoftKey::default();
    let mut owner = env.anon();
    setup_owner(&mut owner, &mut key).await?;
    let listed = owner.get("/api/auth/passkeys").await?.json;
    let id = listed["items"][0]["id"].as_str().unwrap_or_default().to_string();
    let r = owner.delete(&format!("/api/auth/passkeys/{id}")).await?;
    assert_eq!((r.status, r.code()), (StatusCode::CONFLICT, "last_passkey"));
    let codes = owner.post0("/api/auth/recovery-codes").await?.json;
    assert_eq!(codes["recovery_codes"].as_array().map(Vec::len), Some(10));
    Ok(())
}

#[tokio::test]
async fn lan_install_uses_password() -> R {
    let env = Env::new(false).await?;
    let mut c = env.anon_at(LAN);
    let state = c.get("/api/auth/state").await?.json;
    assert_eq!(
        (&state["passkeys_possible"], &state["password_allowed"]),
        (&json!(false), &json!(true))
    );
    let r = c
        .post("/api/auth/setup/begin", &json!({"display_name": "Alex Rivera"}))
        .await?;
    assert_eq!(r.code(), "passkeys_unavailable");
    let r = c
        .post(
            "/api/auth/setup/password",
            &json!({"display_name": "Alex Rivera", "password": "short"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json["detail"][0]["loc"], json!(["body", "password"]));
    let r = c
        .post(
            "/api/auth/setup/password",
            &json!({"display_name": "Alex Rivera", "password": "a long synthetic pass"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // plain HTTP: a Secure cookie would never come back
    let cookie = r.set_cookies().join("\n");
    assert!(cookie.starts_with("oh_session=") && !cookie.to_lowercase().contains("secure"));
    assert!(
        cookie.ends_with("; HttpOnly; Max-Age=43200; Path=/; SameSite=lax"),
        "{cookie}"
    );
    assert_eq!(c.me().await?["display_name"], "Alex Rivera");

    let mut other = env.anon_at(LAN);
    let r = other
        .post(
            "/api/auth/password/login",
            &json!({"name": "alex rivera", "password": "wrong one"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = other
        .post(
            "/api/auth/password/login",
            &json!({"name": "Alex Rivera", "password": "a long synthetic pass"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK);
    // step-up with the password
    assert_eq!(
        other.post0("/api/auth/step-up/begin").await?.json,
        json!({"method": "password"})
    );
    let r = other
        .post("/api/auth/step-up/finish", &json!({"password": "nope"}))
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = other
        .post(
            "/api/auth/step-up/finish",
            &json!({"password": "a long synthetic pass"}),
        )
        .await?;
    assert_eq!(r.json["ok"], true);

    // behind HTTPS the same instance refuses passwords: passkeys work there
    let r = env
        .anon()
        .post(
            "/api/auth/password/login",
            &json!({"name": "Alex Rivera", "password": "a long synthetic pass"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "passkeys_available"));
    Ok(())
}

#[tokio::test]
async fn bodies_are_validated_like_fastapi() -> R {
    let env = Env::new(false).await?;
    let mut c = env.anon();
    let r = c.post0("/api/auth/setup/begin").await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json["detail"][0]["type"], "missing");
    let r = c
        .post("/api/auth/setup/begin", &json!({"household_name": "Rivera"}))
        .await?;
    assert_eq!(r.json["detail"][0]["loc"], json!(["body", "display_name"]));
    let r = c.post("/api/auth/setup/begin", &json!({"display_name": ""})).await?;
    assert_eq!(
        r.json["detail"][0],
        json!({"type": "string_too_short", "loc": ["body", "display_name"],
               "msg": "String should have at least 1 character", "input": "", "ctx": {"min_length": 1}})
    );
    Ok(())
}
