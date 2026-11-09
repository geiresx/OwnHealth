//! v0.2 the source registry over REST (`/api/inboxes`): a manager adds, changes and removes sources;
//! secrets are sealed and never come back; a push token needs step-up; another household sees nothing.

#[path = "auth_support.rs"]
mod support;

use axum::http::{Method, StatusCode};
use ownhealth_core::Runtime;
use ownhealth_core::demo::create_rivera_family;
use serde_json::{Value, json};
use support::Env;

type R = anyhow::Result<()>;

const IMAP_PASS: &str = "app-pass-registry-7Qx";
/// 32 bytes of 0x09, base64.
const KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

async fn env() -> anyhow::Result<Env> {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(KEY.into());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    Ok(env)
}

fn mailbox() -> Value {
    json!({"kind": "email", "label": "Correo de salud", "secret": IMAP_PASS,
           "config": {"host": "imap.rivera.example", "port": 993, "tls": "implicit", "username": "salud.rivera"}})
}

#[tokio::test]
async fn a_manager_adds_changes_and_removes_a_mailbox() -> R {
    let env = env().await?;
    let mut c = env.as_user(env.user("alex")).await?;

    let r = c.post("/api/inboxes", &mailbox()).await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = r.json["id"].as_str().unwrap_or_default().to_string();
    // no poll interval in the body: the kind's default, which the 2-minute email target needs
    assert_eq!(r.json["poll_seconds"], 60);
    assert_eq!(r.json["has_secret"], true);
    assert_eq!(r.json["quarantined"], 0);
    for hidden in ["secret_id", "cursor", "token_sha256", "secret"] {
        assert!(r.json.get(hidden).is_none(), "{hidden} leaked: {}", r.json);
    }
    assert!(!String::from_utf8_lossy(&r.bytes).contains(IMAP_PASS));
    let sealed = env
        .scalar_i64("select count(*) from secret where purpose = 'imap_password'")
        .await?;
    assert_eq!(sealed, 1);
    let plain: i64 = sqlx::query_scalar("select count(*) from secret where position($1::bytea in ciphertext) > 0")
        .bind(IMAP_PASS.as_bytes())
        .fetch_one(env.pool())
        .await?;
    assert_eq!(plain, 0, "the password is stored in the clear");

    let list = c.get("/api/inboxes").await?;
    assert_eq!(list.status, StatusCode::OK);
    let row = list
        .json
        .as_array()
        .and_then(|a| a.iter().find(|s| s["id"] == id.as_str()));
    assert_eq!(row.map(|s| s["kind"].clone()), Some(json!("email")));

    // a new password replaces the sealed one in place; an absent one keeps it
    let r = c
        .send(
            Method::PATCH,
            &format!("/api/inboxes/{id}"),
            Some(&json!({"label": "Buzón", "secret": "otra-clave", "poll_seconds": 120})),
            &[],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        (r.json["label"].as_str(), r.json["poll_seconds"].as_i64()),
        (Some("Buzón"), Some(120))
    );
    assert_eq!(env.scalar_i64("select count(*) from secret").await?, 1);

    // "sync now" queues one poll job
    let r = c.post0(&format!("/api/inboxes/{id}/sync")).await?;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.json);
    assert!(r.json["job_id"].is_i64());
    let r = c.get(&format!("/api/inboxes/{id}/runs?limit=5")).await?;
    assert_eq!((r.status, r.json.as_array().map(Vec::len)), (StatusCode::OK, Some(0)));

    let r = c.delete(&format!("/api/inboxes/{id}")).await?;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        env.scalar_i64("select count(*) from secret").await?,
        0,
        "the sealed password stays"
    );
    assert_eq!(env.scalar_i64("select count(*) from source").await?, 0);
    let r = c.get(&format!("/api/inboxes/{id}")).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn a_broken_source_is_refused_when_saved() -> R {
    let env = env().await?;
    let mut c = env.as_user(env.user("alex")).await?;
    let alex = env.profile("alex").to_string();
    for (body, why) in [
        (
            json!({"kind": "email", "label": "x", "config": {"host": "h", "port": 993, "tls": true, "username": "u"}}),
            "tls",
        ),
        (
            json!({"kind": "email", "label": "x", "config": {"host": "h", "port": 993, "tls": "implicit"}}),
            "no user",
        ),
        (
            json!({"kind": "folder", "label": "x", "config": {"subdir": "../etc"}}),
            "climbs out",
        ),
        (
            json!({"kind": "paperless", "label": "x", "profile_id": alex, "config": {"base_url": "ftp://p", "health_tag": "health"}}),
            "scheme",
        ),
        (
            json!({"kind": "nightscout", "label": "x", "config": {"base_url": "https://ns.rivera.example"}}),
            "no profile",
        ),
        (
            json!({"kind": "nightscout", "label": "x", "profile_id": alex, "config": {"url": "nope"}}),
            "url",
        ),
        (
            json!({"kind": "health_auto_export", "label": "x", "profile_id": alex, "secret": "s"}),
            "no secret kind",
        ),
        (json!({"kind": "teletype", "label": "x"}), "kind"),
        (json!({"kind": "folder", "label": " "}), "label"),
        (json!({"kind": "folder", "label": "x", "poll_seconds": 5}), "poll"),
    ] {
        let r = c.post("/api/inboxes", &body).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{why}: {}", r.json);
    }
    assert_eq!(env.scalar_i64("select count(*) from source").await?, 0);

    // the keys each lane reads are accepted, with the kind's default interval
    let r = c
        .post(
            "/api/inboxes",
            &json!({"kind": "nightscout", "label": "Sensor de Lucía", "profile_id": env.profile("lucia"),
                    "secret": "ns-token", "config": {"base_url": "https://ns.rivera.example"}}),
        )
        .await?;
    assert_eq!(
        (r.status, r.json["poll_seconds"].as_i64()),
        (StatusCode::CREATED, Some(300))
    );
    let r = c
        .post(
            "/api/inboxes",
            &json!({"kind": "folder", "label": "Escáner", "config": {"subdir": "scanner"}}),
        )
        .await?;
    assert_eq!(
        (r.status, r.json["poll_seconds"].as_i64()),
        (StatusCode::CREATED, Some(60))
    );
    // a push kind is never polled
    let r = c
        .post(
            "/api/inboxes",
            &json!({"kind": "shortcut", "label": "Atajo", "profile_id": alex}),
        )
        .await?;
    assert_eq!(
        (r.status, r.json["poll_seconds"].clone()),
        (StatusCode::CREATED, Value::Null)
    );
    let id = r.json["id"].as_str().unwrap_or_default().to_string();
    let r = c.post0(&format!("/api/inboxes/{id}/sync")).await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[tokio::test]
async fn a_push_token_needs_step_up_and_then_works() -> R {
    let env = env().await?;
    let mut c = env.as_user(env.user("alex")).await?;
    let r = c
        .post(
            "/api/inboxes",
            &json!({"kind": "health_auto_export", "label": "iPhone de Alex", "profile_id": env.profile("alex")}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = r.json["id"].as_str().unwrap_or_default().to_string();

    let mut stale = env.stale(env.user("alex")).await?;
    let r = stale.post0(&format!("/api/inboxes/{id}/token")).await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));

    let r = c.post0(&format!("/api/inboxes/{id}/token")).await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let token = r.json["token"].as_str().unwrap_or_default().to_string();
    let prefix = r.json["prefix"].as_str().unwrap_or_default();
    assert!(
        token.starts_with(prefix) && prefix.starts_with("oh_ing_") && prefix.len() == 15,
        "{prefix}"
    );
    let listed = c.get(&format!("/api/inboxes/{id}")).await?;
    assert_eq!(listed.json["token_prefix"], prefix);
    assert!(!String::from_utf8_lossy(&listed.bytes).contains(&token));

    let bearer = format!("Bearer {token}");
    let r = env
        .anon()
        .send(
            Method::POST,
            "/api/ingest/health-auto-export",
            Some(&json!({"data": {"metrics": []}})),
            &[("authorization", &bearer)],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    Ok(())
}

#[tokio::test]
async fn only_managers_of_the_household_reach_its_sources() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post(
            "/api/inboxes",
            &json!({"kind": "shortcut", "label": "Atajo", "profile_id": env.profile("alex")}),
        )
        .await?;
    let id = r.json["id"].as_str().unwrap_or_default().to_string();

    // alex is only a caregiver of carmen: he cannot point a source at her profile
    let r = alex
        .post(
            "/api/inboxes",
            &json!({"kind": "shortcut", "label": "Atajo", "profile_id": env.profile("carmen")}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);

    // a person of another household gets 404 for every route of this source
    let other = {
        let mut conn = env.pool().acquire().await?;
        create_rivera_family(&mut conn, "Otra casa").await?
    };
    let mut o = env.as_user(other.user("alex")).await?;
    let list = o.get("/api/inboxes").await?;
    assert_eq!(list.json, json!([]));
    for (method, path) in [
        (Method::GET, format!("/api/inboxes/{id}")),
        (Method::PATCH, format!("/api/inboxes/{id}")),
        (Method::DELETE, format!("/api/inboxes/{id}")),
        (Method::GET, format!("/api/inboxes/{id}/runs")),
        (Method::POST, format!("/api/inboxes/{id}/token")),
        (Method::POST, format!("/api/inboxes/{id}/sync")),
        (Method::GET, "/api/inboxes/not-a-uuid".to_string()),
    ] {
        let body = (method == Method::PATCH).then(|| json!({"label": "mía"}));
        let r = o.send(method.clone(), &path, body.as_ref(), &[]).await?;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{method} {path}: {}", r.json);
    }
    assert_eq!(
        env.scalar_i64("select count(*) from source where label = 'Atajo'")
            .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn changing_where_a_secret_goes_needs_step_up_and_the_secret_again() -> R {
    let env = env().await?;
    let mut c = env.as_user(env.user("alex")).await?;
    let r = c.post("/api/inboxes", &mailbox()).await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = r.json["id"].as_str().unwrap_or_default().to_string();
    let path = format!("/api/inboxes/{id}");
    let cfg = |host: &str, authserv: Option<&str>| {
        json!({"host": host, "port": 993, "tls": "implicit", "username": "salud.rivera",
               "trusted_authserv_id": authserv})
    };

    // a session past its step-up window cannot point the password at another host, nor trust
    // another receiving server's Authentication-Results
    let mut stale = env.stale(env.user("alex")).await?;
    for (config, why) in [
        (cfg("imap.evil.example", None), "host"),
        (cfg("imap.rivera.example", Some("evil.example")), "authserv-id"),
    ] {
        let r = stale
            .send(
                Method::PATCH,
                &path,
                Some(&json!({"config": config, "secret": "nueva"})),
                &[],
            )
            .await?;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::FORBIDDEN, "step_up_required"),
            "{why}: {}",
            r.json
        );
    }
    // a label change needs no step-up
    let r = stale
        .send(Method::PATCH, &path, Some(&json!({"label": "Buzón"})), &[])
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    // with step-up, a new host still needs the password typed again
    let r = c
        .send(
            Method::PATCH,
            &path,
            Some(&json!({"config": cfg("imap.evil.example", None)})),
            &[],
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.json);
    let host: Option<String> = sqlx::query_scalar("select config->>'host' from source")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(host.as_deref(), Some("imap.rivera.example"));
    // the same host with a new authserv-id keeps the stored password
    let r = c
        .send(
            Method::PATCH,
            &path,
            Some(&json!({"config": cfg("imap.rivera.example", Some("mx.rivera.example"))})),
            &[],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let r = c
        .send(
            Method::PATCH,
            &path,
            Some(&json!({"config": cfg("imap2.rivera.example", None), "secret": "otra-clave"})),
            &[],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    // the same for a Paperless token
    let r = c
        .post(
            "/api/inboxes",
            &json!({"kind": "paperless", "label": "Paperless", "secret": "pl-token",
                    "config": {"base_url": "https://paperless.rivera.example", "health_tag": "health"}}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let pid = r.json["id"].as_str().unwrap_or_default().to_string();
    let moved = json!({"config": {"base_url": "https://paperless.evil.example", "health_tag": "health"}});
    let r = stale
        .send(Method::PATCH, &format!("/api/inboxes/{pid}"), Some(&moved), &[])
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));
    let r = c
        .send(Method::PATCH, &format!("/api/inboxes/{pid}"), Some(&moved), &[])
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.json);
    Ok(())
}

#[tokio::test]
async fn a_paperless_tag_cannot_route_into_a_profile_the_caller_does_not_manage() -> R {
    let env = env().await?;
    // alex owns his profile and guards lucía's, but is only a caregiver of carmen's
    let mut alex = env.as_user(env.user("alex")).await?;
    let config = |profile: uuid::Uuid| {
        json!({"base_url": "https://paperless.rivera.example", "health_tag": "health",
               "profile_by_tag": {"7": profile}})
    };
    let r = alex
        .post(
            "/api/inboxes",
            &json!({"kind": "paperless", "label": "Paperless", "secret": "pl-token",
                    "config": config(env.profile("carmen"))}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    assert_eq!(env.scalar_i64("select count(*) from source").await?, 0);

    let r = alex
        .post(
            "/api/inboxes",
            &json!({"kind": "paperless", "label": "Paperless", "secret": "pl-token",
                    "config": config(env.profile("lucia"))}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = r.json["id"].as_str().unwrap_or_default().to_string();
    let r = alex
        .send(
            Method::PATCH,
            &format!("/api/inboxes/{id}"),
            Some(&json!({"config": config(env.profile("carmen"))})),
            &[],
        )
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    let mapped: Option<String> = sqlx::query_scalar("select config->'profile_by_tag'->>'7' from source")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(mapped, Some(env.profile("lucia").to_string()));
    Ok(())
}
