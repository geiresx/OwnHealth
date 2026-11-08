//! The HTTP door of /mcp (`mcp/auth.py`, mcp.md 1, 2.1): Origin check and personal access token.
//!
//! A request whose `Origin` is not the configured public URL is 403. A missing or bad token is 401
//! with `WWW-Authenticate: Bearer resource_metadata=...`. A valid PAT becomes a Principal in the
//! request extensions, where the MCP handlers read it (rmcp hands them `http::request::Parts`).
//! Browser sessions and the dev user never reach /mcp: an agent always holds its own token.

use std::net::SocketAddr;

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, SCOPES};
use serde_json::{Value, json};
use sqlx::Connection;

/// `scheme://netloc` of a URL, lowercased (Python `urlsplit`).
pub fn origin_of(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{netloc}").to_lowercase()
}

pub fn resource_metadata_url(base_url: &str) -> String {
    format!(
        "{}/.well-known/oauth-protected-resource",
        base_url.trim_end_matches('/')
    )
}

/// RFC 9728 metadata the 401 points at. PATs only in v0.1, so no authorization server is named.
pub async fn protected_resource_metadata(State(rt): State<Runtime>) -> Json<Value> {
    let base = rt.settings.base_url.trim_end_matches('/');
    let mut scopes: Vec<&str> = SCOPES.to_vec();
    scopes.sort_unstable();
    Json(json!({
        "resource": format!("{base}/mcp"),
        "resource_name": "OwnHealth",
        "bearer_methods_supported": ["header"],
        "scopes_supported": scopes,
    }))
}

/// Python `json.dumps(body)` with its default separators and ASCII escapes.
fn py_dumps_default(pairs: &[(&str, &str)]) -> String {
    let esc = |s: &str| {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                    let mut buf = [0u16; 2];
                    for u in c.encode_utf16(&mut buf) {
                        out.push_str(&format!("\\u{u:04x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out
    };
    let body: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("\"{}\": \"{}\"", esc(k), esc(v)))
        .collect();
    format!("{{{}}}", body.join(", "))
}

fn json_response(status: StatusCode, pairs: &[(&str, &str)], challenge: Option<String>) -> Response {
    let mut r = (status, py_dumps_default(pairs)).into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(c) = challenge
        && let Ok(v) = HeaderValue::from_str(&c)
    {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

/// The client address of a request, when the server recorded one.
pub fn client_ip(req: &http::Extensions) -> Option<String> {
    let ip = req.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string())?;
    // 'testclient' and similar are not addresses
    ip.chars().any(|c| c.is_ascii_digit()).then_some(ip)
}

/// The guard in front of /mcp.
pub async fn pat_guard(State(rt): State<Runtime>, mut req: Request<Body>, next: Next) -> Response {
    let settings = &rt.settings;
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let o = String::from_utf8_lossy(origin.as_bytes())
            .trim_end_matches('/')
            .to_lowercase();
        if o != origin_of(&settings.base_url) {
            return json_response(
                StatusCode::FORBIDDEN,
                &[("error", "origin_not_allowed"), ("message", "Origin not allowed.")],
                None,
            );
        }
    }
    let mut challenge = format!(
        "Bearer resource_metadata=\"{}\"",
        resource_metadata_url(&settings.base_url)
    );
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    let (scheme, value) = auth.split_once(' ').unwrap_or((auth.as_str(), ""));
    let value = value.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || value.is_empty() {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &[("error", "invalid_token"), ("message", "A bearer token is required.")],
            Some(challenge),
        );
    }
    let principal = match verify(&rt, value).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "token check failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };
    let Some(principal) = principal else {
        challenge.push_str(", error=\"invalid_token\"");
        return json_response(
            StatusCode::UNAUTHORIZED,
            &[
                ("error", "invalid_token"),
                ("message", "Unknown, expired or revoked token."),
            ],
            Some(challenge),
        );
    };
    req.extensions_mut().insert(principal);
    next.run(req).await
}

async fn verify(rt: &Runtime, plaintext: &str) -> ownhealth_core::Result<Option<Principal>> {
    let mut conn = rt.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let p = ownhealth_auth::tokens::verify_token(&mut tx, plaintext).await?;
    tx.commit().await?;
    Ok(p)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn python_shapes() {
        assert_eq!(origin_of("http://127.0.0.1:8080/x"), "http://127.0.0.1:8080");
        assert_eq!(origin_of("HTTPS://Health.Example.org"), "https://health.example.org");
        assert_eq!(
            py_dumps_default(&[("error", "origin_not_allowed"), ("message", "Origin not allowed.")]),
            r#"{"error": "origin_not_allowed", "message": "Origin not allowed."}"#
        );
    }
}
