//! v0.2 foundation: the push inboxes authenticate with their source token before anything else, and
//! the session routes answer only a caller they have resolved.

#[path = "auth_support.rs"]
mod support;

use axum::http::{Method, StatusCode};
use ownhealth_core::sources::{self, NewSource, SourcePatch};
use serde_json::json;
use support::Env;

type R = anyhow::Result<()>;

#[tokio::test]
async fn push_routes_take_a_source_token_and_nothing_else() -> R {
    let env = Env::new(true).await?;
    let mut conn = env.pool().acquire().await?;
    let id = sources::create_source(
        &mut conn,
        &NewSource {
            profile_id: Some(env.profile("alex")),
            ..NewSource::new(env.fam().household_id, "health_auto_export", "iPhone de Alex")
        },
    )
    .await?;
    let token = sources::issue_token(&mut conn, id).await?;
    let mut c = env.anon();
    let body = json!({"data": {"metrics": []}});
    let bearer = format!("Bearer {token}");
    let push = "/api/ingest/health-auto-export";

    let r = c
        .send(Method::POST, push, Some(&body), &[("authorization", &bearer)])
        .await?;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["seen"], 0);
    let r = c
        .send(
            Method::POST,
            "/api/ingest/paperless",
            Some(&body),
            &[("x-ownhealth-secret", &token)],
        )
        .await?;
    // lane B: the Paperless route takes only a Paperless source's token
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "wrong_source"));

    let mut wrong = token.clone();
    wrong.pop();
    wrong.push(if token.ends_with('A') { 'B' } else { 'A' });
    for headers in [
        vec![],
        vec![("authorization", format!("Bearer {wrong}"))],
        vec![("authorization", "Bearer oh_pat_abcdefgh_x".to_string())],
        vec![("authorization", token.clone())],
    ] {
        let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = c.send(Method::POST, push, Some(&body), &h).await?;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{headers:?}");
    }

    // a disabled source's token stops working
    sources::update_source(
        &mut conn,
        id,
        &SourcePatch {
            enabled: Some(false),
            ..SourcePatch::default()
        },
    )
    .await?;
    let r = c
        .send(Method::POST, push, Some(&body), &[("authorization", &bearer)])
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn session_routes_answer_only_after_the_login() -> R {
    let env = Env::new(true).await?;
    for path in ["/api/inboxes", "/api/profiles/alex/cgm/summary"] {
        let r = env.anon().get(path).await?;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{path}");
    }
    let mut c = env.as_user(env.user("alex")).await?;
    let r = c.get("/api/inboxes").await?;
    assert_eq!(r.status, StatusCode::OK);
    let r = c.get("/api/profiles/alex/cgm/summary").await?;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["valid"], false);
    let r = c.get("/api/profiles/carmen/cgm/summary").await?;
    assert!(
        matches!(r.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
        "{}",
        r.status
    );
    Ok(())
}
