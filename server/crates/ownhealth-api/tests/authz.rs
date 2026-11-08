//! REST authorization on every route (`tests/security/test_authz.py`): who resolves the caller, and
//! what a viewer can and cannot do. The routes are read from the route modules' source, so a new
//! handler is checked the day it lands.
//!
//! Family (demo): Sam owns sam, is viewer on alex, guardian on lucia, and has no grant on carmen.

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeSet;

use axum::http::{Method, StatusCode};
use regex::Regex;
use serde_json::json;
use support::Env;

type R = anyhow::Result<()>;

/// Routes reachable without a login: they are the login itself.
const PUBLIC: [&str; 15] = [
    "/auth/state",
    "/auth/setup/begin",
    "/auth/setup/finish",
    "/auth/setup/password",
    "/auth/login/begin",
    "/auth/login/finish",
    "/auth/recovery",
    "/auth/password/login",
    "/auth/logout",
    "/auth/invite/{code}",
    "/auth/enroll/begin",
    "/auth/enroll/finish",
    "/auth/enroll/password",
    "/auth/oidc/login",
    "/auth/oidc/callback",
];

const METHODS: [Method; 5] = [Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE];

/// Every path a route module registers under `/api`.
fn route_paths() -> anyhow::Result<BTreeSet<String>> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
    let re = Regex::new(r#"\.route\(\s*"([^"]+)""#)?;
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(dir)? {
        let src = std::fs::read_to_string(entry?.path())?;
        out.extend(re.captures_iter(&src).map(|c| c[1].to_string()));
    }
    anyhow::ensure!(out.len() > 50, "the route scan found only {} paths", out.len());
    Ok(out)
}

fn head(body: &[u8]) -> String {
    String::from_utf8_lossy(body).chars().take(200).collect()
}

fn fill(path: &str, slug: &str) -> String {
    let zero = "00000000-0000-0000-0000-000000000000";
    let re = Regex::new(r"\{([a-z_]+)\}").unwrap_or_else(|_| unreachable!());
    re.replace_all(path, |c: &regex::Captures| match &c[1] {
        "p" => slug.to_string(),
        "kind" => "condition".into(),
        "metric" => "steps".into(),
        "page" => "1.png".into(),
        "page_no" => "1".into(),
        "code" | "credential_id" | "src_id" => "x".into(),
        _ => zero.into(),
    })
    .into_owned()
}

#[tokio::test]
async fn every_route_resolves_the_caller() -> R {
    let env = Env::new(true).await?;
    let mut anon = env.anon();
    let mut leaks = Vec::new();
    for path in route_paths()? {
        if PUBLIC.contains(&path.as_str()) {
            continue;
        }
        let mut routed = false;
        for m in METHODS {
            let body = (m != Method::GET && m != Method::DELETE).then(|| json!({}));
            let r = anon
                .send(m.clone(), &format!("/api{}", fill(&path, "alex")), body.as_ref(), &[])
                .await?;
            match r.status {
                StatusCode::METHOD_NOT_ALLOWED => {}
                StatusCode::UNAUTHORIZED => routed = true,
                s => leaks.push(format!("{m} {path}: {s} {}", head(&r.bytes))),
            }
        }
        if !routed {
            leaks.push(format!("{path}: no method answered 401"));
        }
    }
    assert!(
        leaks.is_empty(),
        "routes answering a caller nobody resolved:\n{}",
        leaks.join("\n")
    );
    Ok(())
}

#[tokio::test]
async fn public_list_is_exact() -> R {
    let paths = route_paths()?;
    let stale: Vec<&str> = PUBLIC.iter().copied().filter(|p| !paths.contains(*p)).collect();
    assert!(stale.is_empty(), "stale entries: {stale:?}");
    let env = Env::new(false).await?;
    let mut anon = env.anon();
    // a public route answers someone nobody knows (a 401 here means it moved behind the login)
    for path in PUBLIC {
        for m in METHODS {
            let body = (m != Method::GET && m != Method::DELETE).then(|| json!({}));
            let r = anon
                .send(m.clone(), &format!("/api{}", fill(path, "alex")), body.as_ref(), &[])
                .await?;
            assert_ne!(r.status, StatusCode::UNAUTHORIZED, "{m} {path}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn profile_routes_refuse_ungranted_profiles() -> R {
    let env = Env::new(true).await?;
    // required query parameters get a dummy value, so validation (422) cannot hide a missing check
    let query = "?code=x&drug=x&rsid=x&q=x&query=x";
    for anonymous in [true, false] {
        let mut c = if anonymous {
            env.anon()
        } else {
            env.as_user(env.user("sam")).await?
        };
        let mut leaks = Vec::new();
        for path in route_paths()?.into_iter().filter(|p| p.contains("{p}")) {
            let r = c.get(&format!("/api{}{query}", fill(&path, "carmen"))).await?;
            let ok = match r.status {
                StatusCode::METHOD_NOT_ALLOWED => true,
                s if anonymous => s == StatusCode::UNAUTHORIZED,
                s => s == StatusCode::FORBIDDEN || s == StatusCode::NOT_FOUND,
            };
            if !ok {
                leaks.push(format!("GET {path}: {} {}", r.status, head(&r.bytes)));
            }
        }
        assert!(leaks.is_empty(), "anonymous={anonymous}:\n{}", leaks.join("\n"));
    }
    Ok(())
}

#[tokio::test]
async fn viewer_cannot_write_or_manage() -> R {
    let env = Env::new(true).await?;
    let mut sam = env.as_user(env.user("sam")).await?;
    assert_eq!(sam.get("/api/profiles").await?.status, StatusCode::OK);
    let alex = env.user("alex").to_string();
    let attempts = [
        (
            Method::PUT,
            "/api/profiles/alex/locks".to_string(),
            Some(json!({"locks": []})),
        ),
        (
            Method::PUT,
            "/api/profiles/alex/sensitive".into(),
            Some(json!({"agents": true})),
        ),
        (
            Method::PUT,
            "/api/grants".into(),
            Some(json!({"user_id": alex, "profile": "alex", "role": "viewer"})),
        ),
        (
            Method::POST,
            "/api/profiles/alex/export".into(),
            Some(json!({"fmt": "zip"})),
        ),
        (Method::DELETE, "/api/profiles/alex?confirm=alex".into(), None),
        (Method::DELETE, format!("/api/grants?user_id={alex}&profile=alex"), None),
    ];
    for (m, url, body) in attempts {
        let r = sam.send(m.clone(), &url, body.as_ref(), &[]).await?;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{m} {url}: {:?}", r.json);
    }
    let boundary = "ownhealthboundary";
    let form = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"profile\"\r\n\r\nalex\r\n--{boundary}\r\n\
         Content-Disposition: form-data; name=\"files\"; filename=\"x.pdf\"\r\nContent-Type: application/pdf\r\n\r\n\
         %PDF-1.4 synthetic\r\n--{boundary}--\r\n"
    );
    let r = sam
        .send_raw(
            Method::POST,
            "/api/imports",
            form.into_bytes(),
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    // no grant at all: 404, same as a profile that does not exist
    let r = sam.put("/api/profiles/carmen/locks", &json!({"locks": []})).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = sam.post("/api/profiles/carmen/export", &json!({"fmt": "zip"})).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    Ok(())
}
