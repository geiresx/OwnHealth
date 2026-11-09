//! 0.1.1 to 0.2.0: login rows written by the Python release keep working. `auth_python_rows.json`
//! holds the rows OwnHealth 0.1.1 wrote at first-run setup (a py_webauthn passkey, recovery codes),
//! plus a password, a session and a PAT, with the plaintexts and the passkey's private key
//! (its generator ran on the Python backend and left with it in 0.2.0; see git history). Synthetic data only.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use ownhealth_auth::testing::{SoftKey, signing_key, unb64u};
use ownhealth_auth::tokens::verify_token;
use ownhealth_core::access::{PrincipalKind, scope_set};
use serde_json::{Value, json};
use support::{BASE, Env, LAN};
use uuid::Uuid;

type R = anyhow::Result<()>;

const TABLES: [&str; 8] = [
    "household",
    "app_user",
    "profile",
    "profile_grant",
    "passkey",
    "recovery_code",
    "user_session",
    "api_token",
];

fn fixture() -> anyhow::Result<Value> {
    Ok(serde_json::from_str(include_str!("auth_python_rows.json"))?)
}

/// The fixture's rows in an empty Rust-migrated database; times moved to now, so the session and
/// the token have not expired since the fixture was made.
async fn load(env: &Env, fx: &Value) -> anyhow::Result<()> {
    let mut tx = env.pool().begin().await?;
    for table in TABLES {
        for row in fx["rows"][table].as_array().into_iter().flatten() {
            // Only the columns Python wrote; columns added since then take their defaults, as on an upgrade.
            let cols = row
                .as_object()
                .map(|o| o.keys().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "insert into {table} ({cols}) overriding system value select {cols} from jsonb_populate_record(null::{table}, $1)"
            )))
            .bind(row)
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query(
        "update user_session set created_at = now(), last_seen_at = now(), step_up_at = now(), \
         expires_at = now() + interval '12 hours'",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("update api_token set expires_at = now() + interval '90 days'")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn rows_written_by_python_authenticate() -> R {
    let fx = fixture()?;
    let env = Env::new(false).await?;
    load(&env, &fx).await?;
    let user: Uuid = serde_json::from_value(fx["user_id"].clone())?;

    // the session cookie 0.1.1 set
    let mut browser = env.anon();
    browser.cookies.insert("oh_session".into(), text(&fx["session_secret"]));
    let state = browser.get("/api/auth/state").await?.json;
    assert_eq!(state["setup_needed"], false);
    assert_eq!(
        (
            &state["user"]["display_name"],
            &state["user"]["passkeys"],
            &state["user"]["recovery_codes_left"]
        ),
        (&json!("Carmen Ortega"), &json!(1), &json!(10))
    );
    assert_eq!(state["method"], "passkey");

    // the PAT
    let mut c = env.pool().acquire().await?;
    let p = verify_token(&mut c, &text(&fx["token_plaintext"]))
        .await?
        .ok_or_else(|| anyhow::anyhow!("the 0.1.1 token does not verify"))?;
    assert_eq!((p.kind, p.user_id), (PrincipalKind::Token, Some(user)));
    assert_eq!(p.scopes, scope_set(["read:labs", "read:summary"]));
    drop(c);

    // the passkey py_webauthn stored: a login with the same authenticator
    let pk = &fx["passkey"];
    let scalar = hex_bytes(&text(&pk["private_scalar_hex"]));
    let mut key = SoftKey {
        key: signing_key(Some(&scalar)),
        cred_id: unb64u(&text(&pk["credential_id"])),
        count: u32::try_from(pk["sign_count"].as_u64().unwrap_or(0))?,
        user_handle: Some(unb64u(&text(&pk["user_handle"]))),
        ..SoftKey::new("testserver", BASE)
    };
    let stored: Vec<u8> = sqlx::query_scalar("select public_key from passkey")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(stored, key.cose(), "the fixture key is the stored key");
    let mut fresh = env.anon();
    let opts = fresh.post0("/api/auth/login/begin").await?.json;
    let r = fresh
        .post(
            "/api/auth/login/finish",
            &json!({"credential": key.get(&opts, None, None)}),
        )
        .await?;
    assert_eq!(
        (r.status, &r.json["user_id"]),
        (StatusCode::OK, &json!(user)),
        "{:?}",
        r.json
    );
    let count: i64 = sqlx::query_scalar("select sign_count from passkey")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(count, i64::from(key.count));
    // a step-up with it too
    let mut stale = env.login(user, false, "passkey").await?;
    let begin = stale.post0("/api/auth/step-up/begin").await?.json;
    let r = stale
        .post(
            "/api/auth/step-up/finish",
            &json!({"credential": key.get(&begin["options"], None, None)}),
        )
        .await?;
    assert_eq!(r.json["ok"], true, "{:?}", r.json);

    // a recovery code 0.1.1 printed
    let codes: Vec<String> = serde_json::from_value(fx["recovery_codes"].clone())?;
    let r = env
        .anon()
        .post("/api/auth/recovery", &json!({"code": codes[0]}))
        .await?;
    assert_eq!((r.status, &r.json["recovery_codes_left"]), (StatusCode::OK, &json!(9)));

    // the scrypt password, on a plain-HTTP LAN address where passwords are allowed
    let r = env
        .anon_at(LAN)
        .post(
            "/api/auth/password/login",
            &json!({"name": "Carmen Ortega", "password": text(&fx["password"])}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = env
        .anon_at(LAN)
        .post(
            "/api/auth/password/login",
            &json!({"name": "Carmen Ortega", "password": "not her password"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}
