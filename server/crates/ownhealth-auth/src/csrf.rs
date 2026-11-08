//! CSRF protection for state-changing REST calls made with the session cookie (`auth/csrf.py`).
//!
//! The cookie is SameSite=Lax, and every unsafe request that rides on a cookie (or on dev auth)
//! must come from our own origin: `Origin` (or `Sec-Fetch-Site`, or the `Referer` origin) must
//! match OWNHEALTH_BASE_URL or the request's own host. Bearer tokens skip the check.

use http::{HeaderMap, Method};
use ownhealth_core::access::{Principal, PrincipalKind};

/// The 403 body `{"detail": {"code": "csrf_failed", ...}}` is built by the API layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrfFailed;

impl std::fmt::Display for CsrfFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cross-site request refused.")
    }
}

impl std::error::Error for CsrfFailed {}

/// `scheme://netloc`, lowercased (Python `urlsplit`).
fn origin_of(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{netloc}").to_lowercase()
}

fn header<'h>(h: &'h HeaderMap, name: &str) -> Option<&'h str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

fn allowed_origins(headers: &HeaderMap, base_url: &str) -> Vec<String> {
    let mut out = vec![origin_of(base_url)];
    if let Some(host) = header(headers, "host") {
        out.push(format!("https://{host}").to_lowercase());
        out.push(format!("http://{host}").to_lowercase());
    }
    out
}

/// Refuse a cross-site unsafe request carried by a cookie or dev auth.
pub fn check(method: &Method, headers: &HeaderMap, principal: &Principal, base_url: &str) -> Result<(), CsrfFailed> {
    if principal.kind == PrincipalKind::Token {
        return Ok(());
    }
    check_origin(method, headers, base_url)
}

/// The same-origin check alone, for the public login routes (no principal yet).
pub fn check_origin(method: &Method, headers: &HeaderMap, base_url: &str) -> Result<(), CsrfFailed> {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Ok(());
    }
    if header(headers, "sec-fetch-site") == Some("cross-site") {
        return Err(CsrfFailed);
    }
    let origin = header(headers, "origin")
        .map(str::to_string)
        .or_else(|| header(headers, "referer").map(origin_of));
    match origin {
        Some(o) if !allowed_origins(headers, base_url).contains(&o.to_lowercase()) => Err(CsrfFailed),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            if let Ok(v) = v.parse() {
                m.insert(*k, v);
            }
        }
        m
    }

    #[test]
    fn unsafe_cross_site_requests_are_refused() {
        let p = Principal::session(Uuid::nil(), Uuid::nil());
        let base = "https://health.example";
        assert!(check(&Method::GET, &h(&[("origin", "https://evil.example")]), &p, base).is_ok());
        assert!(check(&Method::POST, &h(&[("origin", "https://evil.example")]), &p, base).is_err());
        assert!(check(&Method::POST, &h(&[("origin", "https://HEALTH.example")]), &p, base).is_ok());
        assert!(
            check(
                &Method::POST,
                &h(&[("host", "box:8080"), ("origin", "http://box:8080")]),
                &p,
                base
            )
            .is_ok()
        );
        assert!(check(&Method::POST, &h(&[("sec-fetch-site", "cross-site")]), &p, base).is_err());
        assert!(
            check(
                &Method::DELETE,
                &h(&[("referer", "https://evil.example/page?x=1")]),
                &p,
                base
            )
            .is_err()
        );
        assert!(
            check(&Method::POST, &h(&[]), &p, base).is_ok(),
            "no Origin and no Referer: SameSite=Lax covers it"
        );
        let token = Principal {
            kind: PrincipalKind::Token,
            ..p
        };
        assert!(check(&Method::POST, &h(&[("origin", "https://evil.example")]), &token, base).is_ok());
    }
}
