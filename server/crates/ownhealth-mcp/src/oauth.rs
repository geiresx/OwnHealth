//! Lane E (v0.2): the HTTP side of the built-in OAuth 2.1 authorization server (mcp.md 2.2,
//! security-privacy.md 5). The domain logic (requests, consents, codes, tokens, the CIMD fetch) is
//! `ownhealth_auth::oauth`; these handlers speak the protocol:
//!
//! - `GET /.well-known/oauth-authorization-server` (RFC 8414): issuer = OWNHEALTH_BASE_URL,
//!   `code_challenge_methods_supported: ["S256"]`, `client_id_metadata_document_supported: true`,
//!   `authorization_response_iss_parameter_supported: true`; no `registration_endpoint` (no DCR).
//! - `GET /oauth/authorize`: checks the client (CIMD) and `redirect_uri` first (a failure there is
//!   shown, never redirected), then `response_type=code`, PKCE S256 (plain or missing is refused),
//!   `resource` = our canonical `/mcp` URI (RFC 8707) and the scopes; then sends the browser to the
//!   consent screen, `/oauth/consent?request=<id>`, where the person logs in and approves with step-up
//!   (`ownhealth_api::routes::oauth`).
//! - `POST /oauth/token` (form): `authorization_code` (PKCE verifier, same `resource`) and
//!   `refresh_token`; public clients only (`client_id` in the body, no secret).
//!
//! All of it runs only behind https (`oauth::enabled`); on plain http these routes answer 404 and
//! the protected-resource metadata names no authorization server.

use axum::extract::{Query, RawForm, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ownhealth_auth::oauth::{self, AuthorizeIn, OAuthError};
use ownhealth_core::access::SCOPES;
use ownhealth_core::{Error, Runtime};
use serde_json::{Value, json};
use sqlx::Connection;

fn base(rt: &Runtime) -> &str {
    rt.settings.base_url.trim_end_matches('/')
}

fn no_store(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    r
}

fn error_json(status: StatusCode, e: &OAuthError) -> Response {
    no_store(
        (
            status,
            Json(json!({"error": e.code, "error_description": e.description})),
        )
            .into_response(),
    )
}

fn disabled() -> Response {
    error_json(
        StatusCode::NOT_FOUND,
        &OAuthError::new(
            "unsupported",
            "OAuth needs OWNHEALTH_BASE_URL to be an https address; use a personal access token.",
        ),
    )
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "oauth request failed");
    error_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        &OAuthError::new("server_error", "OwnHealth could not answer this request."),
    )
}

/// RFC 8414 metadata.
pub fn metadata(base: &str) -> Value {
    let mut scopes: Vec<&str> = SCOPES.to_vec();
    scopes.sort_unstable();
    json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": scopes,
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
    })
}

async fn as_metadata(State(rt): State<Runtime>) -> Response {
    if !oauth::enabled(base(&rt)) {
        return disabled();
    }
    Json(metadata(base(&rt))).into_response()
}

/// The single value of a parameter; a repeated parameter is a bad request (RFC 6749 3.1).
fn single<'a>(q: &'a [(String, String)], key: &str) -> Result<Option<&'a str>, OAuthError> {
    let mut it = q.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let first = it.next();
    if it.next().is_some() {
        return Err(OAuthError::new("invalid_request", format!("'{key}' is repeated.")));
    }
    Ok(first.filter(|v| !v.is_empty()))
}

/// What `/oauth/authorize` decides, before anything is stored.
#[derive(Debug, PartialEq)]
pub enum Authorize {
    /// Shown to the person; the client is not trusted with a redirect.
    Show(OAuthError),
    /// Sent back to the client's redirect URI.
    Back {
        redirect_uri: String,
        state: Option<String>,
        error: OAuthError,
    },
    Ok(AuthorizeIn),
}

/// The checks after the client and its redirect URI are known good.
pub fn check_authorize(
    q: &[(String, String)],
    client_id: &str,
    redirect_uri: &str,
    canonical: &str,
) -> Result<AuthorizeIn, OAuthError> {
    if single(q, "response_type")? != Some("code") {
        return Err(OAuthError::new(
            "unsupported_response_type",
            "Only response_type=code is supported.",
        ));
    }
    match single(q, "code_challenge_method")? {
        Some("S256") => {}
        Some(_) => {
            return Err(OAuthError::new(
                "invalid_request",
                "Only the S256 PKCE method is supported.",
            ));
        }
        None => {
            return Err(OAuthError::new(
                "invalid_request",
                "PKCE is required: send code_challenge_method=S256.",
            ));
        }
    }
    let challenge = single(q, "code_challenge")?
        .filter(|c| oauth::valid_challenge(c))
        .ok_or_else(|| OAuthError::new("invalid_request", "code_challenge must be an S256 challenge."))?;
    let resources: Vec<&str> = q
        .iter()
        .filter(|(k, _)| k == "resource")
        .map(|(_, v)| v.as_str())
        .collect();
    let resource = match resources.as_slice() {
        [] => canonical,
        [r] if *r == canonical => canonical,
        _ => {
            return Err(OAuthError::new(
                "invalid_target",
                "This server issues tokens only for its own /mcp resource.",
            ));
        }
    };
    let scopes = oauth::parse_scopes(single(q, "scope")?)?;
    Ok(AuthorizeIn {
        client_id: client_id.to_string(),
        redirect_uri: redirect_uri.to_string(),
        state: single(q, "state")?.map(str::to_string),
        code_challenge: challenge.to_string(),
        resource: resource.to_string(),
        scopes,
    })
}

async fn decide(rt: &Runtime, q: &[(String, String)]) -> Result<Authorize, Error> {
    let client_id = match single(q, "client_id") {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Ok(Authorize::Show(OAuthError::new(
                "invalid_request",
                "client_id is missing.",
            )));
        }
        Err(e) => return Ok(Authorize::Show(e)),
    };
    let mut conn = rt.pool.acquire().await?;
    let client = match oauth::fetch_client(&mut conn, client_id).await {
        Ok(c) => c,
        Err(Error::Invalid(m)) => return Ok(Authorize::Show(OAuthError::new("invalid_client", m))),
        Err(e) => return Err(e),
    };
    let redirect_uri = match single(q, "redirect_uri") {
        Ok(Some(r)) if oauth::redirect_allowed(&client.redirect_uris, r) => r,
        Ok(_) => {
            return Ok(Authorize::Show(OAuthError::new(
                "invalid_request",
                "redirect_uri is missing or not listed in the client's metadata document.",
            )));
        }
        Err(e) => return Ok(Authorize::Show(e)),
    };
    let state = single(q, "state").ok().flatten().map(str::to_string);
    let canonical = oauth::canonical_resource(base(rt));
    Ok(match check_authorize(q, client_id, redirect_uri, &canonical) {
        Ok(a) => Authorize::Ok(a),
        Err(error) => Authorize::Back {
            redirect_uri: redirect_uri.to_string(),
            state,
            error,
        },
    })
}

async fn authorize(State(rt): State<Runtime>, Query(q): Query<Vec<(String, String)>>) -> Response {
    if !oauth::enabled(base(&rt)) {
        return disabled();
    }
    let issuer = base(&rt).to_string();
    match decide(&rt, &q).await {
        Err(e) => internal(e),
        Ok(Authorize::Show(e)) => error_json(StatusCode::BAD_REQUEST, &e),
        Ok(Authorize::Back {
            redirect_uri,
            state,
            error,
        }) => {
            let mut p = vec![("error", error.code), ("error_description", error.description.as_str())];
            if let Some(s) = &state {
                p.push(("state", s));
            }
            p.push(("iss", issuer.as_str()));
            match oauth::redirect_with(&redirect_uri, &p) {
                Ok(to) => no_store(Redirect::to(&to).into_response()),
                Err(e) => internal(e),
            }
        }
        Ok(Authorize::Ok(a)) => {
            let stored = async {
                let mut conn = rt.pool.acquire().await?;
                oauth::create_request(&mut conn, &a).await
            }
            .await;
            match stored {
                Ok(id) => no_store(Redirect::to(&format!("{issuer}/oauth/consent?request={id}")).into_response()),
                Err(e) => internal(e),
            }
        }
    }
}

async fn token(State(rt): State<Runtime>, RawForm(raw): RawForm) -> Response {
    if !oauth::enabled(base(&rt)) {
        return disabled();
    }
    let form: Vec<(String, String)> = url_decode(&raw);
    let p = |k: &str| single(&form, k);
    let answer = async {
        let grant_type = p("grant_type")?;
        let client_id = p("client_id")?
            .ok_or_else(|| OAuthError::new("invalid_client", "client_id is required (public clients)."))?;
        if p("client_secret")?.is_some() {
            return Err(OAuthError::new(
                "invalid_client",
                "Only public clients are supported; send no client_secret.",
            ));
        }
        let resource = p("resource")?;
        Ok((
            grant_type.map(str::to_string),
            client_id.to_string(),
            resource.map(str::to_string),
        ))
    }
    .await;
    let (grant_type, client_id, resource) = match answer {
        Ok(x) => x,
        Err(e) => {
            let status = if e.code == "invalid_client" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            return error_json(status, &e);
        }
    };
    let canonical = oauth::canonical_resource(base(&rt));
    let result = async {
        let mut conn = rt.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let r = match grant_type.as_deref() {
            Some("authorization_code") => {
                let (Ok(Some(code)), Ok(Some(redirect_uri)), Ok(verifier)) =
                    (p("code"), p("redirect_uri"), p("code_verifier"))
                else {
                    return Ok(Err(OAuthError::new(
                        "invalid_request",
                        "code and redirect_uri are required.",
                    )));
                };
                let Some(verifier) = verifier else {
                    return Ok(Err(OAuthError::new(
                        "invalid_request",
                        "code_verifier is required (PKCE).",
                    )));
                };
                oauth::exchange_code(
                    &mut tx,
                    code,
                    &client_id,
                    redirect_uri,
                    verifier,
                    resource.as_deref(),
                    &canonical,
                )
                .await?
            }
            Some("refresh_token") => {
                let Ok(Some(rt_token)) = p("refresh_token") else {
                    return Ok(Err(OAuthError::new("invalid_request", "refresh_token is required.")));
                };
                oauth::refresh(&mut tx, rt_token, &client_id, resource.as_deref(), &canonical).await?
            }
            _ => Err(OAuthError::new(
                "unsupported_grant_type",
                "Use authorization_code or refresh_token.",
            )),
        };
        // a replayed code or refresh token revokes the consent even though the answer is an error
        tx.commit().await?;
        Ok::<_, Error>(r)
    }
    .await;
    match result {
        Err(e) => internal(e),
        Ok(Err(e)) => error_json(StatusCode::BAD_REQUEST, &e),
        Ok(Ok(pair)) => no_store(Json(pair).into_response()),
    }
}

/// `application/x-www-form-urlencoded` pairs.
fn url_decode(raw: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(raw).into_owned().collect()
}

/// The authorization server routes (outside `/api`).
pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(as_metadata))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/token", post(token))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const CH: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const MCP: &str = "https://h.example/mcp";

    #[test]
    fn plain_pkce_and_a_foreign_resource_are_refused() {
        let ok = check_authorize(
            &q(&[
                ("response_type", "code"),
                ("code_challenge", CH),
                ("code_challenge_method", "S256"),
                ("resource", MCP),
                ("scope", "read:labs"),
                ("state", "xyz"),
            ]),
            "https://c.example/c.json",
            "https://c.example/cb",
            MCP,
        )
        .unwrap();
        assert_eq!(ok.resource, MCP);
        assert_eq!(ok.state.as_deref(), Some("xyz"));
        assert_eq!(ok.scopes, vec!["read:labs"]);

        let base = [("response_type", "code"), ("code_challenge", CH)];
        let cases: Vec<(Vec<(&str, &str)>, &str)> = vec![
            (vec![("code_challenge_method", "plain")], "invalid_request"),
            (vec![], "invalid_request"),
            (
                vec![
                    ("code_challenge_method", "S256"),
                    ("resource", "https://other.example/mcp"),
                ],
                "invalid_target",
            ),
            (
                vec![
                    ("code_challenge_method", "S256"),
                    ("resource", MCP),
                    ("resource", "https://x/mcp"),
                ],
                "invalid_target",
            ),
            (
                vec![("code_challenge_method", "S256"), ("scope", "root")],
                "invalid_scope",
            ),
        ];
        for (extra, code) in cases {
            let mut pairs: Vec<(&str, &str)> = base.to_vec();
            pairs.extend(extra.iter().copied());
            let e = check_authorize(&q(&pairs), "https://c.example/c.json", "https://c.example/cb", MCP).unwrap_err();
            assert_eq!(e.code, code, "{extra:?}");
        }
        let e = check_authorize(
            &q(&[
                ("response_type", "token"),
                ("code_challenge", CH),
                ("code_challenge_method", "S256"),
            ]),
            "c",
            "r",
            MCP,
        )
        .unwrap_err();
        assert_eq!(e.code, "unsupported_response_type");
    }

    #[test]
    fn metadata_names_s256_cimd_and_iss_and_no_registration() {
        let m = metadata("https://h.example");
        assert_eq!(m["code_challenge_methods_supported"], json!(["S256"]));
        assert_eq!(m["client_id_metadata_document_supported"], json!(true));
        assert_eq!(m["authorization_response_iss_parameter_supported"], json!(true));
        assert!(m.get("registration_endpoint").is_none());
    }
}
