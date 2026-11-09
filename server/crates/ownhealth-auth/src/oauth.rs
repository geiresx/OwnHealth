//! Lane E (v0.2): the authorization server's domain logic (mcp.md 2.2, security-privacy.md 5, 16).
//! The HTTP handlers are `ownhealth_mcp::oauth` (protocol) and `ownhealth_api::routes::oauth` (the
//! consent screen and the consents list); tables are `oauth_client`, `oauth_request`, `oauth_consent`,
//! `oauth_code`, `oauth_refresh_token` and `api_token` rows of kind `oauth` (1001, 1050).
//!
//! - Clients are Client ID Metadata Documents: the `client_id` is an https URL; [`fetch_client`] gets
//!   it with `core::net::get(url, &Egress::cimd())` (64 KiB, 5 s, private addresses refused, no
//!   redirects), checks that its `client_id` equals the URL, that it is a public client and that it
//!   lists redirect URIs, and caches it for its `max-age` (5 minutes to a day). No DCR.
//! - PKCE S256 only ([`pkce_s256`]); `resource` must be our canonical `/mcp` URI ([`canonical_resource`]);
//!   the code is single use, ten minutes; a second use revokes the consent.
//! - Approving a client needs step-up (the route checks it); the consent names each profile and scope,
//!   expires after 90 days by default (365 at most) and is revocable from the UI. PATs are untouched.
//! - One consent has one access token row (`api_token`, kind `oauth`, audience = the resource), so the
//!   MCP rate limits, daily budgets and audit count per consent. The access token lives an hour; a
//!   refresh rotates its secret in place and rotates the refresh token, until the consent expires. A
//!   refresh token used twice revokes the consent.
//! - [`verify_access_token`] is what the `/mcp` guard calls for an `oh_oat_` bearer: a token whose
//!   audience is not our canonical URI is refused (RFC 8707).

use std::collections::BTreeSet;

use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use ownhealth_core::access::{Principal, PrincipalKind, is_scope};
use ownhealth_core::net::{self, Egress};
use ownhealth_core::{Error, Result};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{sha256, token_urlsafe};

pub const ACCESS_TOKEN_PREFIX: &str = "oh_oat_";
pub const REFRESH_TOKEN_PREFIX: &str = "oh_ort_";
pub const CODE_SECONDS: i64 = 600;
pub const REQUEST_SECONDS: i64 = 600;
pub const ACCESS_SECONDS: i64 = 3600;
pub const DEFAULT_TOKEN_DAYS: i64 = 90;
pub const MAX_TOKEN_DAYS: i64 = 365;
/// How long a fetched metadata document is trusted, whatever its `Cache-Control` says.
pub const CIMD_MIN_SECONDS: i64 = 300;
pub const CIMD_MAX_SECONDS: i64 = 86_400;
/// The scope a consent screen pre-checks, and what a request without `scope` asks for.
pub const DEFAULT_SCOPE: &str = "read:summary";
const NAME_MAX_CHARS: usize = 120;
const ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// `https://<host>/mcp` for a base URL.
pub fn canonical_resource(base_url: &str) -> String {
    format!("{}/mcp", base_url.trim_end_matches('/'))
}

/// The authorization server runs only behind https (OAuth 2.1 section 1.5; passkeys need it too).
/// On a plain-http install `/mcp` keeps taking PATs and advertises no authorization server.
pub fn enabled(base_url: &str) -> bool {
    base_url.to_ascii_lowercase().starts_with("https://")
}

/// base64url(sha256(verifier)) without padding (RFC 7636 4.2).
pub fn pkce_s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// RFC 7636 4.1: 43 to 128 characters of `[A-Za-z0-9-._~]`.
pub fn valid_verifier(v: &str) -> bool {
    (43..=128).contains(&v.len())
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// An S256 challenge is base64url of 32 bytes: 43 characters, no padding.
pub fn valid_challenge(c: &str) -> bool {
    c.len() == 43 && c.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// An OAuth protocol error (RFC 6749 4.1.2.1 and 5.2): the code and a description safe to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub code: &'static str,
    pub description: String,
}

impl OAuthError {
    pub fn new(code: &'static str, description: impl Into<String>) -> OAuthError {
        OAuthError {
            code,
            description: description.into(),
        }
    }
}

/// The scopes of a `scope` parameter: space separated, known, deduplicated and sorted. Empty asks
/// for [`DEFAULT_SCOPE`].
pub fn parse_scopes(raw: Option<&str>) -> std::result::Result<Vec<String>, OAuthError> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for s in raw.unwrap_or_default().split_ascii_whitespace() {
        if !is_scope(s) {
            return Err(OAuthError::new("invalid_scope", format!("Unknown scope '{s}'.")));
        }
        out.insert(s.to_string());
    }
    if out.is_empty() {
        out.insert(DEFAULT_SCOPE.into());
    }
    Ok(out.into_iter().collect())
}

// ------------------------------------------------------------------ clients ----

/// A client as its metadata document describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientMetadata {
    pub client_id: String,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub client_uri: Option<String>,
    #[serde(default)]
    pub logo_uri: Option<String>,
    pub redirect_uris: Vec<String>,
}

#[cfg(feature = "testing")]
static TEST_ORIGINS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Tests only: documents of client ids under `https_origin` (a name nobody resolves, such as
/// `https://agent.test`) are fetched from `local` instead (`http://127.0.0.1:<port>`), so a test can
/// serve one without TLS. The client id, its checks and the cache are exactly as in production.
#[cfg(feature = "testing")]
pub fn route_test_cimd_origin(https_origin: &str, local: &str) {
    if let Ok(mut g) = TEST_ORIGINS.lock() {
        g.push((
            https_origin.trim_end_matches('/').to_string(),
            local.trim_end_matches('/').to_string(),
        ));
    }
}

/// Where a test serves this client's document, if it does.
fn test_route(url: &url::Url) -> Option<String> {
    #[cfg(feature = "testing")]
    {
        let origin = url.origin().ascii_serialization();
        if let Ok(g) = TEST_ORIGINS.lock()
            && let Some((_, local)) = g.iter().find(|(o, _)| *o == origin)
        {
            let mut tail = url.path().to_string();
            if let Some(q) = url.query() {
                tail.push('?');
                tail.push_str(q);
            }
            return Some(format!("{local}{tail}"));
        }
    }
    let _ = url;
    None
}

/// A `client_id` must be an https URL with a path, no fragment, no credentials and no dot segments
/// (CIMD section 3).
pub fn check_client_id(client_id: &str) -> Result<url::Url> {
    let bad = |why: &str| Error::Invalid(format!("client_id {why}"));
    let u = url::Url::parse(client_id).map_err(|_| bad("is not a URL"))?;
    if u.scheme() != "https" {
        return Err(bad("must be an https URL"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(bad("has no host"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("must not carry credentials"));
    }
    if u.fragment().is_some() || client_id.contains('#') {
        return Err(bad("must not have a fragment"));
    }
    if u.path() == "/" || u.path().is_empty() {
        return Err(bad("must have a path"));
    }
    let raw_path = client_id
        .split_once("://")
        .map(|(_, r)| r)
        .and_then(|r| r.find('/').map(|i| &r[i..]))
        .unwrap_or_default();
    if raw_path
        .split(['/', '?'])
        .any(|seg| seg == "." || seg == ".." || seg.eq_ignore_ascii_case("%2e") || seg.eq_ignore_ascii_case("%2e%2e"))
    {
        return Err(bad("must not have dot segments"));
    }
    // url normalises; the document must name exactly the string the client sent
    if u.as_str() != client_id {
        return Err(bad("is not in canonical form"));
    }
    Ok(u)
}

/// `max-age` from a `Cache-Control` header, clamped to the CIMD limits.
pub fn cache_seconds(cache_control: Option<&str>) -> i64 {
    let mut age = None;
    for part in cache_control.unwrap_or_default().split(',') {
        let p = part.trim().to_ascii_lowercase();
        if p == "no-store" || p == "no-cache" {
            age = Some(0);
        } else if let Some(v) = p.strip_prefix("max-age=")
            && age.is_none()
        {
            age = v.trim().parse::<i64>().ok();
        }
    }
    age.unwrap_or(CIMD_MIN_SECONDS)
        .clamp(CIMD_MIN_SECONDS, CIMD_MAX_SECONDS)
}

/// A display string from the document: control characters dropped, capped. Still untrusted text.
fn clean(v: Option<&Value>) -> Option<String> {
    let s: String = v?
        .as_str()?
        .chars()
        .filter(|c| !c.is_control())
        .take(NAME_MAX_CHARS)
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Check a fetched document against the URL it came from.
pub fn parse_document(client_id: &str, body: &[u8]) -> Result<(ClientMetadata, Value)> {
    let bad = |why: &str| Error::Invalid(format!("client metadata document {why}"));
    let doc: Value = serde_json::from_slice(body).map_err(|_| bad("is not JSON"))?;
    let obj = doc.as_object().ok_or_else(|| bad("is not a JSON object"))?;
    if obj.get("client_id").and_then(Value::as_str) != Some(client_id) {
        return Err(bad("names another client_id"));
    }
    if obj.keys().any(|k| k.starts_with("client_secret")) {
        return Err(bad("carries a client secret"));
    }
    match obj.get("token_endpoint_auth_method").and_then(Value::as_str) {
        None | Some("none") => {}
        Some(_) => return Err(bad("asks for client authentication; only public clients are supported")),
    }
    if let Some(g) = obj.get("grant_types")
        && !g
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == "authorization_code"))
    {
        return Err(bad("does not use the authorization_code grant"));
    }
    let uris: Vec<String> = obj
        .get("redirect_uris")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if uris.is_empty() {
        return Err(bad("lists no redirect_uris"));
    }
    for u in &uris {
        let ok = url::Url::parse(u).is_ok_and(|p| p.fragment().is_none() && p.scheme() != "javascript");
        if !ok {
            return Err(bad("has an invalid redirect URI"));
        }
    }
    Ok((
        ClientMetadata {
            client_id: client_id.to_string(),
            client_name: clean(obj.get("client_name")),
            client_uri: clean(obj.get("client_uri")),
            logo_uri: clean(obj.get("logo_uri")),
            redirect_uris: uris,
        },
        doc,
    ))
}

type ClientRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<String>,
    DateTime<Utc>,
    String,
);

async fn cached_client(conn: &mut PgConnection, client_id: &str) -> Result<Option<(ClientMetadata, bool)>> {
    let row: Option<ClientRow> = sqlx::query_as(
        "select client_name, client_uri, logo_uri, redirect_uris, expires_at, status from oauth_client \
         where client_id = $1",
    )
    .bind(client_id)
    .fetch_optional(conn)
    .await?;
    let Some((name, uri, logo, redirect_uris, expires_at, status)) = row else {
        return Ok(None);
    };
    if status == "blocked" {
        return Err(Error::Invalid("this client is blocked on this server".into()));
    }
    Ok(Some((
        ClientMetadata {
            client_id: client_id.to_string(),
            client_name: name,
            client_uri: uri,
            logo_uri: logo,
            redirect_uris,
        },
        expires_at > Utc::now(),
    )))
}

/// Fetch (or read from the cache) and check a client's metadata document: https, public addresses
/// only, 64 KiB, 5 s, no redirects (`Egress::cimd`).
pub async fn fetch_client(conn: &mut PgConnection, client_id: &str) -> Result<ClientMetadata> {
    let u = check_client_id(client_id)?;
    let (from, egress) = match test_route(&u) {
        Some(local) => (
            local,
            Egress {
                allow_private: true,
                https_only: false,
                ..Egress::cimd()
            },
        ),
        None => (client_id.to_string(), Egress::cimd()),
    };
    if let Some((client, fresh)) = cached_client(&mut *conn, client_id).await?
        && fresh
    {
        return Ok(client);
    }
    let fetched = net::get(&from, &egress, &[("accept", "application/json")])
        .await
        .map_err(|e| Error::Invalid(format!("client metadata document could not be fetched: {e}")))?;
    if fetched.status != 200 {
        return Err(Error::Invalid(format!(
            "client metadata document answered HTTP {}",
            fetched.status
        )));
    }
    let (client, doc) = parse_document(client_id, &fetched.body)?;
    let ttl = cache_seconds(fetched.cache_control.as_deref());
    sqlx::query(
        "insert into oauth_client (client_id, client_name, client_uri, logo_uri, redirect_uris, document, \
         fetched_at, expires_at) values ($1, $2, $3, $4, $5, $6, now(), now() + make_interval(secs => $7)) \
         on conflict (client_id) do update set client_name = excluded.client_name, client_uri = \
         excluded.client_uri, logo_uri = excluded.logo_uri, redirect_uris = excluded.redirect_uris, document = \
         excluded.document, fetched_at = excluded.fetched_at, expires_at = excluded.expires_at",
    )
    .bind(&client.client_id)
    .bind(&client.client_name)
    .bind(&client.client_uri)
    .bind(&client.logo_uri)
    .bind(&client.redirect_uris)
    .bind(&doc)
    .bind(ttl as f64)
    .execute(conn)
    .await?;
    Ok(client)
}

// ----------------------------------------------------------------- requests ----

/// What `/oauth/authorize` asked for, once its parameters passed.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorizeIn {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: String,
    pub resource: String,
    pub scopes: Vec<String>,
}

pub async fn create_request(conn: &mut PgConnection, r: &AuthorizeIn) -> Result<Uuid> {
    Ok(sqlx::query_scalar(
        "insert into oauth_request (client_id, redirect_uri, state, code_challenge, resource, scopes, expires_at) \
         values ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7)) returning id",
    )
    .bind(&r.client_id)
    .bind(&r.redirect_uri)
    .bind(&r.state)
    .bind(&r.code_challenge)
    .bind(&r.resource)
    .bind(&r.scopes)
    .bind(REQUEST_SECONDS as f64)
    .fetch_one(conn)
    .await?)
}

/// A pending authorization as the consent screen shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    pub id: Uuid,
    pub client: ClientMetadata,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub resource: String,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
}

type RequestRow = (
    String,
    String,
    Option<String>,
    String,
    Vec<String>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

/// An open request: not decided, not expired. `lock` takes the row for update (approve, deny).
async fn open_request(conn: &mut PgConnection, id: Uuid, lock: bool) -> Result<PendingRequest> {
    let sql = if lock {
        "select client_id, redirect_uri, state, resource, scopes, expires_at, decided_at from oauth_request \
         where id = $1 for update"
    } else {
        "select client_id, redirect_uri, state, resource, scopes, expires_at, decided_at from oauth_request \
         where id = $1"
    };
    let row: Option<RequestRow> = sqlx::query_as(sql).bind(id).fetch_optional(&mut *conn).await?;
    let Some((client_id, redirect_uri, state, resource, scopes, expires_at, decided_at)) = row else {
        return Err(Error::NotFound("Unknown authorization request.".into()));
    };
    if decided_at.is_some() || expires_at <= Utc::now() {
        return Err(Error::NotFound(
            "This authorization request has expired or was already answered. Start again from the app.".into(),
        ));
    }
    let Some((client, _)) = cached_client(&mut *conn, &client_id).await? else {
        return Err(Error::NotFound("Unknown client.".into()));
    };
    Ok(PendingRequest {
        id,
        client,
        redirect_uri,
        state,
        resource,
        scopes,
        expires_at,
    })
}

pub async fn get_request(conn: &mut PgConnection, id: Uuid) -> Result<PendingRequest> {
    open_request(conn, id, false).await
}

/// The host of a URL (the consent screen names where the browser goes back to).
pub fn url_host(u: &str) -> Option<String> {
    url::Url::parse(u).ok()?.host_str().map(str::to_string)
}

/// Is `given` one of the client's registered redirect URIs? Exact match, except that a native app's
/// loopback URI (`http://localhost/...`, `http://127.0.0.1/...`, `http://[::1]/...`) matches on any
/// port (RFC 8252 7.3): the app listens on a port it picks at run time, and its metadata document
/// lists the URI without one (Claude Code's does).
pub fn redirect_allowed(registered: &[String], given: &str) -> bool {
    let loopback = |u: &url::Url| {
        u.scheme() == "http"
            && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
            && u.username().is_empty()
            && u.password().is_none()
    };
    let Ok(g) = url::Url::parse(given) else { return false };
    registered.iter().any(|r| {
        r == given
            || url::Url::parse(r).is_ok_and(|r| {
                loopback(&r)
                    && loopback(&g)
                    && r.host_str() == g.host_str()
                    && r.path() == g.path()
                    && r.query() == g.query()
                    && g.fragment().is_none()
            })
    })
}

/// `redirect_uri` with the response parameters added (RFC 9207: always `iss`).
pub fn redirect_with(redirect_uri: &str, params: &[(&str, &str)]) -> Result<String> {
    let mut u =
        url::Url::parse(redirect_uri).map_err(|_| Error::Invalid("the client's redirect URI is not a URL".into()))?;
    {
        let mut q = u.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Ok(u.into())
}

fn response_params<'a>(state: &'a Option<String>, issuer: &'a str) -> Vec<(&'static str, &'a str)> {
    let mut p = Vec::new();
    if let Some(s) = state {
        p.push(("state", s.as_str()));
    }
    p.push(("iss", issuer));
    p
}

/// The person refused: the client gets `error=access_denied`.
pub async fn deny_request(conn: &mut PgConnection, user_id: Uuid, id: Uuid, issuer: &str) -> Result<String> {
    let r = open_request(&mut *conn, id, true).await?;
    sqlx::query("update oauth_request set decided_at = now(), decided_by = $2, decision = 'denied' where id = $1")
        .bind(id)
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    let mut p = vec![
        ("error", "access_denied"),
        ("error_description", "The person declined."),
    ];
    p.extend(response_params(&r.state, issuer));
    redirect_with(&r.redirect_uri, &p)
}

/// What the person ticked on the consent screen.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GrantIn {
    pub scopes: Vec<String>,
    pub profile_ids: Vec<Uuid>,
    #[serde(default)]
    pub default_profile: Option<Uuid>,
    #[serde(default = "full")]
    pub identity_mode: String,
    #[serde(default)]
    pub days: Option<i64>,
}

fn full() -> String {
    "full".into()
}

/// Approve a request (the caller checked step-up): record the consent, issue the code, and answer
/// the redirect back to the client. The person may grant fewer scopes than asked, plus
/// `read:summary`, never more.
pub async fn approve_request(
    conn: &mut PgConnection,
    user_id: Uuid,
    id: Uuid,
    grant: &GrantIn,
    issuer: &str,
) -> Result<(Uuid, String)> {
    let r = open_request(&mut *conn, id, true).await?;
    if let Some(s) = grant
        .scopes
        .iter()
        .find(|s| !r.scopes.contains(s) && s.as_str() != DEFAULT_SCOPE)
    {
        return Err(Error::Invalid(format!("the client did not ask for '{s}'")));
    }
    let consent_id = approve(
        &mut *conn,
        user_id,
        &ConsentIn {
            client_id: r.client.client_id.clone(),
            scopes: grant.scopes.clone(),
            profile_ids: grant.profile_ids.clone(),
            default_profile: grant.default_profile,
            identity_mode: grant.identity_mode.clone(),
            days: grant.days,
        },
    )
    .await?;
    let code = token_urlsafe(32);
    let scopes: Vec<String> = grant
        .scopes
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    sqlx::query(
        "insert into oauth_code (code_sha256, consent_id, client_id, redirect_uri, code_challenge, resource, scopes, \
         expires_at) select $1, $2, client_id, redirect_uri, code_challenge, resource, $3, \
         now() + make_interval(secs => $4) from oauth_request where id = $5",
    )
    .bind(sha256(&code))
    .bind(consent_id)
    .bind(&scopes)
    .bind(CODE_SECONDS as f64)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("update oauth_request set decided_at = now(), decided_by = $2, decision = 'approved' where id = $1")
        .bind(id)
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    let mut p = vec![("code", code.as_str())];
    p.extend(response_params(&r.state, issuer));
    Ok((consent_id, redirect_with(&r.redirect_uri, &p)?))
}

// ----------------------------------------------------------------- consents ----

/// What the person approves on the consent screen.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConsentIn {
    pub client_id: String,
    pub scopes: Vec<String>,
    pub profile_ids: Vec<Uuid>,
    pub default_profile: Option<Uuid>,
    pub identity_mode: String,
    pub days: Option<i64>,
}

/// Record an approved consent (the caller checked step-up) and return its id. Profiles must be ones
/// the user holds a grant on (else `AccessDenied` `profile_not_granted`); scopes known and at least
/// one; the expiry 1 to 365 days (90 by default).
pub async fn approve(conn: &mut PgConnection, user_id: Uuid, consent: &ConsentIn) -> Result<Uuid> {
    if consent.profile_ids.is_empty() {
        return Err(Error::Invalid("choose at least one profile".into()));
    }
    if consent.scopes.is_empty() {
        return Err(Error::Invalid("choose at least one scope".into()));
    }
    if let Some(s) = consent.scopes.iter().find(|s| !is_scope(s)) {
        return Err(Error::Invalid(format!("unknown scope '{s}'")));
    }
    if !["full", "pseudonymous", "minimal"].contains(&consent.identity_mode.as_str()) {
        return Err(Error::Invalid(
            "identity_mode must be full, pseudonymous or minimal".into(),
        ));
    }
    let days = consent.days.unwrap_or(DEFAULT_TOKEN_DAYS);
    if !(1..=MAX_TOKEN_DAYS).contains(&days) {
        return Err(Error::Invalid(format!("expiry must be 1 to {MAX_TOKEN_DAYS} days")));
    }
    let mut profiles: Vec<Uuid> = Vec::new();
    for p in &consent.profile_ids {
        if !profiles.contains(p) {
            profiles.push(*p);
        }
    }
    let granted: Vec<Uuid> =
        sqlx::query_scalar("select profile_id from profile_grant where user_id = $1 and profile_id = any($2)")
            .bind(user_id)
            .bind(&profiles)
            .fetch_all(&mut *conn)
            .await?;
    if profiles.iter().any(|p| !granted.contains(p)) {
        return Err(Error::access_denied(
            "profile_not_granted",
            "The consent names a profile you have no grant on.",
        ));
    }
    let default = match consent.default_profile {
        Some(d) if !profiles.contains(&d) => {
            return Err(Error::Invalid(
                "default_profile must be one of the chosen profiles".into(),
            ));
        }
        Some(d) => d,
        None => profiles[0],
    };
    let scopes: Vec<&String> = consent.scopes.iter().collect::<BTreeSet<_>>().into_iter().collect();
    Ok(sqlx::query_scalar(
        "insert into oauth_consent (user_id, client_id, scopes, profile_ids, default_profile, identity_mode, \
         expires_at) values ($1, $2, $3, $4, $5, $6, now() + make_interval(days => $7)) returning id",
    )
    .bind(user_id)
    .bind(&consent.client_id)
    .bind(&scopes)
    .bind(&profiles)
    .bind(default)
    .bind(&consent.identity_mode)
    .bind(i32::try_from(days).unwrap_or(DEFAULT_TOKEN_DAYS as i32))
    .fetch_one(conn)
    .await?)
}

/// Revoke a consent and every token issued under it. Revoking only narrows access: no step-up.
pub async fn revoke_consent(conn: &mut PgConnection, user_id: Uuid, consent_id: Uuid) -> Result<bool> {
    let n = sqlx::query(
        "update oauth_consent set revoked_at = now() where id = $1 and user_id = $2 and revoked_at is null",
    )
    .bind(consent_id)
    .bind(user_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n > 0 {
        revoke_tokens(conn, consent_id).await?;
    }
    Ok(n > 0)
}

async fn revoke_tokens(conn: &mut PgConnection, consent_id: Uuid) -> Result<()> {
    sqlx::query("update api_token set revoked_at = now() where consent_id = $1 and revoked_at is null")
        .bind(consent_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("update oauth_refresh_token set revoked_at = now() where consent_id = $1 and revoked_at is null")
        .bind(consent_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Revoke a consent whatever its owner (a replayed code or refresh token).
async fn revoke_by_server(conn: &mut PgConnection, consent_id: Uuid) -> Result<()> {
    sqlx::query("update oauth_consent set revoked_at = now() where id = $1 and revoked_at is null")
        .bind(consent_id)
        .execute(&mut *conn)
        .await?;
    revoke_tokens(conn, consent_id).await
}

/// One approved client, for the settings list (newest first).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConsentListed {
    pub id: Uuid,
    pub client_id: String,
    pub client_name: Option<String>,
    pub scopes: Vec<String>,
    pub profile_ids: Vec<Uuid>,
    pub default_profile: Option<Uuid>,
    pub identity_mode: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
}

pub async fn list_consents(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<ConsentListed>> {
    Ok(sqlx::query_as(
        "select c.id, c.client_id, k.client_name, c.scopes, c.profile_ids, c.default_profile, c.identity_mode, \
         c.created_at, c.expires_at, c.revoked_at, (select max(t.last_used_at) from api_token t \
         where t.consent_id = c.id) as last_used_at from oauth_consent c join oauth_client k \
         on k.client_id = c.client_id where c.user_id = $1 order by c.created_at desc",
    )
    .bind(user_id)
    .fetch_all(conn)
    .await?)
}

// ------------------------------------------------------------------- tokens ----

/// What the token endpoint answers (RFC 6749 5.1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenPair {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub refresh_token: String,
    pub scope: String,
}

fn new_secret() -> (String, String) {
    let key: String = {
        let mut rng = rand::rng();
        (0..8)
            .map(|_| ID_ALPHABET[rng.random_range(0..ID_ALPHABET.len())] as char)
            .collect()
    };
    (format!("{ACCESS_TOKEN_PREFIX}{key}"), token_urlsafe(32))
}

#[derive(sqlx::FromRow)]
struct ConsentRow {
    id: Uuid,
    user_id: Uuid,
    client_id: String,
    scopes: Vec<String>,
    profile_ids: Vec<Uuid>,
    default_profile: Option<Uuid>,
    identity_mode: String,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

async fn consent_row(conn: &mut PgConnection, id: Uuid) -> Result<Option<ConsentRow>> {
    Ok(sqlx::query_as(
        "select id, user_id, client_id, scopes, profile_ids, default_profile, identity_mode, expires_at, revoked_at \
         from oauth_consent where id = $1 for update",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?)
}

fn live(c: &ConsentRow) -> bool {
    c.revoked_at.is_none() && c.expires_at > Utc::now()
}

/// Issue (or rotate) the consent's access token and a new refresh token.
async fn issue(conn: &mut PgConnection, c: &ConsentRow, resource: &str) -> Result<TokenPair> {
    let (prefix, secret) = new_secret();
    let expires = (Utc::now() + Duration::seconds(ACCESS_SECONDS)).min(c.expires_at);
    let existing: Option<Uuid> =
        sqlx::query_scalar("select id from api_token where consent_id = $1 and revoked_at is null limit 1")
            .bind(c.id)
            .fetch_optional(&mut *conn)
            .await?;
    let token_id = match existing {
        Some(id) => {
            sqlx::query(
                "update api_token set prefix = $2, secret_sha256 = $3, expires_at = $4, audience = $5 where id = $1",
            )
            .bind(id)
            .bind(&prefix)
            .bind(sha256(&secret))
            .bind(expires)
            .bind(resource)
            .execute(&mut *conn)
            .await?;
            id
        }
        None => {
            let lang: String = sqlx::query_scalar("select locale from app_user where id = $1")
                .bind(c.user_id)
                .fetch_one(&mut *conn)
                .await?;
            sqlx::query_scalar(
                "insert into api_token (user_id, label, prefix, secret_sha256, scopes, profile_ids, default_profile, \
                 identity_mode, lang, units, expires_at, kind, audience, oauth_client_id, consent_id) values \
                 ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'si', $10, 'oauth', $11, $12, $13) returning id",
            )
            .bind(c.user_id)
            .bind(&c.client_id)
            .bind(&prefix)
            .bind(sha256(&secret))
            .bind(&c.scopes)
            .bind(&c.profile_ids)
            .bind(c.default_profile)
            .bind(&c.identity_mode)
            .bind(lang)
            .bind(expires)
            .bind(resource)
            .bind(&c.client_id)
            .bind(c.id)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    let refresh = format!("{REFRESH_TOKEN_PREFIX}{}", token_urlsafe(32));
    sqlx::query(
        "insert into oauth_refresh_token (consent_id, secret_sha256, access_token_id, expires_at) values ($1, $2, $3, $4)",
    )
    .bind(c.id)
    .bind(sha256(&refresh))
    .bind(token_id)
    .bind(c.expires_at)
    .execute(&mut *conn)
    .await?;
    Ok(TokenPair {
        access_token: format!("{prefix}_{secret}"),
        token_type: "Bearer",
        expires_in: (expires - Utc::now()).num_seconds().max(0),
        refresh_token: refresh,
        scope: c.scopes.join(" "),
    })
}

fn check_resource(resource: Option<&str>, expected: &str) -> std::result::Result<(), OAuthError> {
    match resource {
        None => Ok(()),
        Some(r) if r == expected => Ok(()),
        Some(_) => Err(OAuthError::new(
            "invalid_target",
            "This server issues tokens only for its own /mcp resource.",
        )),
    }
}

/// The `authorization_code` grant. The outer error is a database failure; the inner one is what the
/// client is told. `canonical` is our `/mcp` URI.
pub async fn exchange_code(
    conn: &mut PgConnection,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: Option<&str>,
    canonical: &str,
) -> Result<std::result::Result<TokenPair, OAuthError>> {
    let grant = |d: &str| Ok(Err(OAuthError::new("invalid_grant", d)));
    if !valid_verifier(verifier) {
        return Ok(Err(OAuthError::new(
            "invalid_request",
            "code_verifier must be 43 to 128 unreserved characters.",
        )));
    }
    type CodeRow = (
        Uuid,
        String,
        String,
        String,
        String,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
    );
    let row: Option<CodeRow> = sqlx::query_as(
        "select consent_id, client_id, redirect_uri, code_challenge, resource, expires_at, used_at from oauth_code \
         where code_sha256 = $1 for update",
    )
    .bind(sha256(code))
    .fetch_optional(&mut *conn)
    .await?;
    let Some((consent_id, c_client, c_redirect, challenge, c_resource, expires_at, used_at)) = row else {
        return grant("Unknown authorization code.");
    };
    // Who is asking comes first: a request that fails these changes nothing, so someone holding only
    // the code (no verifier) can neither burn it nor trigger the replay revocation below.
    if c_client != client_id {
        return grant("The code was issued to another client.");
    }
    if c_redirect != redirect_uri {
        return grant("redirect_uri does not match the authorization request.");
    }
    let computed = pkce_s256(verifier);
    if !bool::from(computed.as_bytes().ct_eq(challenge.as_bytes())) {
        return grant("The PKCE code_verifier does not match the code_challenge.");
    }
    if used_at.is_some() {
        // RFC 6749 4.1.2: a replayed code revokes what it issued. `used_at` is set only by a
        // redemption that issued tokens.
        revoke_by_server(&mut *conn, consent_id).await?;
        return grant("This authorization code was already used; its tokens are revoked.");
    }
    if expires_at <= Utc::now() {
        return grant("The authorization code expired.");
    }
    if c_resource != canonical {
        return Ok(Err(OAuthError::new(
            "invalid_target",
            "The code was issued for another resource.",
        )));
    }
    if let Err(e) = check_resource(resource, canonical) {
        return Ok(Err(e));
    }
    let Some(c) = consent_row(&mut *conn, consent_id).await?.filter(live) else {
        return grant("The consent behind this code was revoked or expired.");
    };
    sqlx::query("update oauth_code set used_at = now() where code_sha256 = $1")
        .bind(sha256(code))
        .execute(&mut *conn)
        .await?;
    Ok(Ok(issue(conn, &c, canonical).await?))
}

/// The `refresh_token` grant: rotates both tokens. A refresh token used twice revokes the consent.
pub async fn refresh(
    conn: &mut PgConnection,
    refresh_token: &str,
    client_id: &str,
    resource: Option<&str>,
    canonical: &str,
) -> Result<std::result::Result<TokenPair, OAuthError>> {
    let grant = |d: &str| Ok(Err(OAuthError::new("invalid_grant", d)));
    type RefreshRow = (Uuid, Uuid, DateTime<Utc>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);
    let row: Option<RefreshRow> = sqlx::query_as(
        "select id, consent_id, expires_at, used_at, revoked_at from oauth_refresh_token where secret_sha256 = $1 \
         for update",
    )
    .bind(sha256(refresh_token))
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, consent_id, expires_at, used_at, revoked_at)) = row else {
        return grant("Unknown refresh token.");
    };
    if used_at.is_some() {
        revoke_by_server(&mut *conn, consent_id).await?;
        return grant("This refresh token was already used; the consent is revoked.");
    }
    if revoked_at.is_some() || expires_at <= Utc::now() {
        return grant("The refresh token was revoked or expired.");
    }
    if let Err(e) = check_resource(resource, canonical) {
        return Ok(Err(e));
    }
    let Some(c) = consent_row(&mut *conn, consent_id).await?.filter(live) else {
        return grant("The consent was revoked or expired.");
    };
    if c.client_id != client_id {
        return grant("The refresh token was issued to another client.");
    }
    sqlx::query("update oauth_refresh_token set used_at = now() where id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(Ok(issue(conn, &c, canonical).await?))
}

#[derive(sqlx::FromRow)]
struct AccessRow {
    id: Uuid,
    user_id: Uuid,
    label: String,
    secret_sha256: Vec<u8>,
    audience: Option<String>,
    scopes: Vec<String>,
    profile_ids: Vec<Uuid>,
    default_profile: Option<Uuid>,
    identity_mode: String,
    lang: String,
    units: String,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    household_id: Uuid,
    disabled_at: Option<DateTime<Utc>>,
    consent_expires: DateTime<Utc>,
    consent_revoked: Option<DateTime<Utc>>,
}

/// The Principal of an OAuth access token whose audience is `resource`; None for an unknown,
/// expired, revoked or foreign-audience token.
pub async fn verify_access_token(conn: &mut PgConnection, token: &str, resource: &str) -> Result<Option<Principal>> {
    let Some(body) = token.strip_prefix(ACCESS_TOKEN_PREFIX) else {
        return Ok(None);
    };
    let Some((key, secret)) = body.split_once('_') else {
        return Ok(None);
    };
    if key.chars().count() != 8 || secret.is_empty() {
        return Ok(None);
    }
    let row: Option<AccessRow> = sqlx::query_as(
        "select t.id, t.user_id, t.label, t.secret_sha256, t.audience, t.scopes, t.profile_ids, t.default_profile, \
         t.identity_mode, t.lang, t.units, t.expires_at, t.revoked_at, u.household_id, u.disabled_at, \
         c.expires_at as consent_expires, c.revoked_at as consent_revoked from api_token t \
         join app_user u on u.id = t.user_id join oauth_consent c on c.id = t.consent_id \
         where t.prefix = $1 and t.kind = 'oauth'",
    )
    .bind(format!("{ACCESS_TOKEN_PREFIX}{key}"))
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if !bool::from(row.secret_sha256.ct_eq(&sha256(secret))) {
        return Ok(None);
    }
    let now = Utc::now();
    if row.revoked_at.is_some()
        || row.expires_at <= now
        || row.disabled_at.is_some()
        || row.consent_revoked.is_some()
        || row.consent_expires <= now
    {
        return Ok(None);
    }
    // RFC 8707: a token minted for another resource is never accepted here
    if row.audience.as_deref() != Some(resource) {
        return Ok(None);
    }
    sqlx::query("update api_token set last_used_at = now() where id = $1")
        .bind(row.id)
        .execute(conn)
        .await?;
    Ok(Some(Principal {
        kind: PrincipalKind::Token,
        user_id: Some(row.user_id),
        household_id: Some(row.household_id),
        scopes: row.scopes.into_iter().collect(),
        token_id: Some(row.id),
        token_label: Some(row.label),
        profile_ids: Some(row.profile_ids.into_iter().collect()),
        default_profile: row.default_profile,
        identity_mode: row.identity_mode,
        lang: row.lang,
        units: row.units,
        session_id: None,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_redirect_matches_on_any_port_and_nothing_else_is_loosened() {
        let reg = vec![
            "http://localhost/callback".to_string(),
            "http://127.0.0.1/callback".to_string(),
            "https://app.example/cb".to_string(),
        ];
        for ok in [
            "http://localhost/callback",
            "http://localhost:33418/callback",
            "http://127.0.0.1:5173/callback",
            "https://app.example/cb",
        ] {
            assert!(redirect_allowed(&reg, ok), "{ok}");
        }
        for no in [
            "http://localhost:33418/other",
            "http://127.0.0.1:1/callback?x=1",
            "http://localhost.evil.example:80/callback",
            "http://evil.example/callback",
            "https://localhost:33418/callback",
            "https://app.example:8443/cb",
            "https://app.example/cb2",
            "http://localhost:1/callback#f",
            "not a url",
        ] {
            assert!(!redirect_allowed(&reg, no), "{no}");
        }
    }

    #[test]
    fn pkce_matches_the_rfc_7636_example_and_the_resource_is_the_mcp_uri() {
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(valid_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert!(!valid_verifier("short"));
        assert!(!valid_verifier(&"a b".repeat(20)));
        assert!(valid_challenge("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!valid_challenge("plain-verifier"));
        assert_eq!(
            canonical_resource("https://health.example/"),
            "https://health.example/mcp"
        );
        assert!(enabled("https://health.example"));
        assert!(!enabled("http://127.0.0.1:8080"));
    }

    #[test]
    fn client_ids_must_be_https_urls_with_a_path() {
        assert!(check_client_id("https://app.example/oauth/client.json").is_ok());
        for bad in [
            "http://app.example/client.json",
            "https://app.example",
            "https://app.example/",
            "https://app.example/a/../client.json",
            "https://app.example/client.json#x",
            "https://user:pw@app.example/client.json",
            "https://APP.example/client.json",
            "not a url",
        ] {
            assert!(check_client_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn documents_must_name_themselves_be_public_and_list_redirects() {
        let id = "https://app.example/client.json";
        let ok = format!(
            r#"{{"client_id":"{id}","client_name":"Agent\u0007 X","redirect_uris":["https://app.example/cb"]}}"#
        );
        let (c, _) = parse_document(id, ok.as_bytes()).unwrap();
        assert_eq!(c.client_name.as_deref(), Some("Agent X"));
        for bad in [
            r#"{"client_id":"https://other.example/c.json","redirect_uris":["https://app.example/cb"]}"#.to_string(),
            format!(r#"{{"client_id":"{id}","redirect_uris":[]}}"#),
            format!(r#"{{"client_id":"{id}","redirect_uris":["https://a/cb"],"client_secret":"s"}}"#),
            format!(
                r#"{{"client_id":"{id}","redirect_uris":["https://a/cb"],"token_endpoint_auth_method":"private_key_jwt"}}"#
            ),
            format!(r#"{{"client_id":"{id}","redirect_uris":["https://a/cb"],"grant_types":["client_credentials"]}}"#),
            "[]".to_string(),
        ] {
            assert!(parse_document(id, bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn cache_lifetime_is_clamped() {
        assert_eq!(cache_seconds(None), CIMD_MIN_SECONDS);
        assert_eq!(cache_seconds(Some("public, max-age=3600")), 3600);
        assert_eq!(cache_seconds(Some("max-age=9999999")), CIMD_MAX_SECONDS);
        assert_eq!(cache_seconds(Some("no-store")), CIMD_MIN_SECONDS);
    }

    #[test]
    fn scopes_parse_and_default_to_summary() {
        assert_eq!(parse_scopes(None).unwrap(), vec!["read:summary"]);
        assert_eq!(
            parse_scopes(Some("read:labs  read:summary read:labs")).unwrap(),
            vec!["read:labs", "read:summary"]
        );
        assert_eq!(parse_scopes(Some("admin")).unwrap_err().code, "invalid_scope");
    }

    #[test]
    fn redirects_carry_iss_and_state() {
        let state = Some("s 1".to_string());
        let mut p = vec![("code", "abc")];
        p.extend(response_params(&state, "https://h.example"));
        assert_eq!(
            redirect_with("https://app.example/cb?x=1", &p).unwrap(),
            "https://app.example/cb?x=1&code=abc&state=s+1&iss=https%3A%2F%2Fh.example"
        );
    }
}
