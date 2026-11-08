//! Optional OIDC login against any OpenID Connect provider (Pocket ID, Authentik, Authelia, ...),
//! `auth/oidc.py`.
//!
//! ```text
//! OWNHEALTH_OIDC_ISSUER=https://id.example.lan          (discovery: <issuer>/.well-known/openid-configuration)
//! OWNHEALTH_OIDC_CLIENT_ID=ownhealth
//! OWNHEALTH_OIDC_CLIENT_SECRET=...                      (or OWNHEALTH_OIDC_CLIENT_SECRET_FILE; empty = public client)
//! OWNHEALTH_OIDC_LABEL=Pocket ID                        (button text, optional)
//! OWNHEALTH_OIDC_SCOPES="openid profile email"          (optional)
//! ```
//!
//! Register the redirect URI `<OWNHEALTH_BASE_URL>/api/auth/oidc/callback` at the provider.
//! Authorization code flow with PKCE (S256), `state` and `nonce`, each single use (`auth_challenge`).
//! The ID token's signature is checked against the provider's JWKS (openidconnect), plus iss, aud,
//! exp and nonce. A person is matched by `app_user.oidc_subject` = `<issuer>#<sub>`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::Utc;
use openidconnect::core::{CoreIdToken, CoreIdTokenVerifier, CoreJsonWebKeySet, CoreJwsSigningAlgorithm};
use openidconnect::{ClientId, ClientSecret, IssuerUrl, Nonce};
use ownhealth_core::Error;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::challenges::{self, Challenge, Issue, TTL_SECONDS};
use crate::{AuthError, AuthResult, token_urlsafe, truncate_chars};

pub const MODES: [&str; 4] = ["login", "link", "enroll", "step_up"];
const ALGS: [CoreJwsSigningAlgorithm; 7] = [
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha512,
    CoreJwsSigningAlgorithm::EcdsaP256Sha256,
    CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPssSha256,
    CoreJwsSigningAlgorithm::EdDsa,
];
const LEEWAY_SECONDS: i64 = 60;
/// The OIDC state lives twice the WebAuthn TTL: ten minutes for the provider round trip.
pub const STATE_TTL_SECONDS: i64 = TTL_SECONDS * 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub label: String,
    pub scopes: String,
}

/// OWNHEALTH_<name>, or the contents of the file named by OWNHEALTH_<name>_FILE.
fn env(name: &str) -> Option<String> {
    if let Ok(path) = std::env::var(format!("OWNHEALTH_{name}_FILE"))
        && !path.is_empty()
    {
        return std::fs::read_to_string(path).ok().map(|s| s.trim().to_string());
    }
    std::env::var(format!("OWNHEALTH_{name}")).ok()
}

impl OidcConfig {
    pub fn new(issuer: &str, client_id: &str) -> OidcConfig {
        OidcConfig {
            issuer: issuer.trim_end_matches('/').into(),
            client_id: client_id.into(),
            client_secret: None,
            label: "OIDC".into(),
            scopes: "openid profile email".into(),
        }
    }

    /// From the environment, read on every call as the Python app did; None when OIDC is off.
    pub fn from_env() -> Option<OidcConfig> {
        let issuer = env("OIDC_ISSUER").filter(|s| !s.is_empty())?;
        let client_id = env("OIDC_CLIENT_ID").filter(|s| !s.is_empty())?;
        let nonempty = |n: &str| env(n).filter(|s| !s.is_empty());
        Some(OidcConfig {
            client_secret: nonempty("OIDC_CLIENT_SECRET"),
            label: nonempty("OIDC_LABEL").unwrap_or_else(|| "OIDC".into()),
            scopes: nonempty("OIDC_SCOPES").unwrap_or_else(|| "openid profile email".into()),
            ..OidcConfig::new(&issuer, &client_id)
        })
    }
}

#[derive(Debug, Clone)]
pub struct OidcResult {
    pub mode: String,
    /// `<issuer>#<sub>`
    pub subject: String,
    pub name: Option<String>,
    pub auth_time: Option<i64>,
    pub challenge: Challenge,
}

fn failed(message: impl Into<String>) -> AuthError {
    AuthError::refused("oidc_failed", message)
}

/// One HTTP client for every provider call.
pub fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default()
    })
}

fn transport(e: reqwest::Error) -> AuthError {
    AuthError::Core(Error::Other(anyhow::anyhow!("OIDC provider: {e}")))
}

async fn get_json(url: &str) -> AuthResult<Value> {
    let r = http().get(url).send().await.map_err(transport)?;
    let r = r.error_for_status().map_err(transport)?;
    r.json().await.map_err(transport)
}

/// The provider's metadata, cached for an hour per issuer.
pub async fn discover(cfg: &OidcConfig) -> AuthResult<Value> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(c) = cache.lock()
        && let Some((until, meta)) = c.get(&cfg.issuer)
        && *until > Instant::now()
    {
        return Ok(meta.clone());
    }
    let meta = get_json(&format!("{}/.well-known/openid-configuration", cfg.issuer)).await?;
    if meta["issuer"].as_str().unwrap_or_default().trim_end_matches('/') != cfg.issuer {
        return Err(AuthError::refused(
            "oidc_misconfigured",
            "The provider's issuer does not match OWNHEALTH_OIDC_ISSUER.",
        ));
    }
    if let Ok(mut c) = cache.lock() {
        c.insert(
            cfg.issuer.clone(),
            (Instant::now() + Duration::from_secs(3600), meta.clone()),
        );
    }
    Ok(meta)
}

fn s256(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Python's `urllib.parse.quote_plus`: letters, digits and `_.-~` stay, a space is `+`.
pub fn quote_plus(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urlencode(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// What the challenge keeps of the browser binder cookie set when the flow began.
pub fn binder_hash(binder: &str) -> String {
    hex::encode(Sha256::digest(binder.as_bytes()))
}

#[derive(Debug, Clone, Default)]
pub struct Begin<'a> {
    pub mode: &'a str,
    pub redirect_uri: &'a str,
    pub user_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub data: Map<String, Value>,
}

/// The provider URL to send the browser to.
pub async fn begin(conn: &mut PgConnection, cfg: &OidcConfig, b: Begin<'_>) -> AuthResult<String> {
    if !MODES.contains(&b.mode) {
        return Err(AuthError::refused("bad_input", format!("unknown mode {}", b.mode)));
    }
    let meta = discover(cfg).await?;
    let (state, nonce, verifier) = (token_urlsafe(32), token_urlsafe(24), token_urlsafe(48));
    let mut data = b.data;
    data.insert("mode".into(), json!(b.mode));
    data.insert("nonce".into(), json!(nonce));
    data.insert("verifier".into(), json!(verifier));
    challenges::issue(
        conn,
        "oidc",
        state.as_bytes(),
        Issue {
            user_id: b.user_id,
            session_id: b.session_id,
            data,
            ttl_seconds: STATE_TTL_SECONDS,
        },
    )
    .await?;
    let challenge = s256(&verifier);
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", cfg.client_id.as_str()),
        ("redirect_uri", b.redirect_uri),
        ("scope", cfg.scopes.as_str()),
        ("state", state.as_str()),
        ("nonce", nonce.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    if b.mode == "step_up" {
        params.extend([("prompt", "login"), ("max_age", "0")]);
    }
    let endpoint = meta["authorization_endpoint"].as_str().ok_or_else(|| {
        AuthError::Core(Error::Other(anyhow::anyhow!(
            "OIDC provider: no authorization_endpoint"
        )))
    })?;
    Ok(format!("{endpoint}?{}", urlencode(&params)))
}

/// Exchange the code, verify the ID token. Consumes the state.
///
/// `binder` is the cookie the browser got when the flow began: a callback opened in another browser
/// (a link or login URL someone else started and sent) is refused.
pub async fn finish(
    conn: &mut PgConnection,
    cfg: &OidcConfig,
    state: &str,
    code: &str,
    redirect_uri: &str,
    binder: &str,
) -> AuthResult<OidcResult> {
    let ch = challenges::consume(conn, state.as_bytes(), &["oidc"]).await?;
    let kept = ch.text("binder").unwrap_or_default();
    if binder.is_empty() || !bool::from(kept.as_bytes().ct_eq(binder_hash(binder).as_bytes())) {
        return Err(failed("This sign-in was started in another browser. Start again."));
    }
    let meta = discover(cfg).await?;
    let verifier = ch.text("verifier").unwrap_or_default().to_string();
    let form = urlencode(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", &verifier),
        ("client_id", &cfg.client_id),
    ]);
    let token_endpoint = meta["token_endpoint"].as_str().unwrap_or_default();
    let mut req = http()
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form);
    if let Some(secret) = &cfg.client_secret {
        req = req.basic_auth(&cfg.client_id, Some(secret));
    }
    let r = req.send().await.map_err(transport)?;
    if r.status().as_u16() != 200 {
        return Err(failed(format!(
            "The provider refused the code ({}).",
            r.status().as_u16()
        )));
    }
    let body: Value = r.json().await.map_err(transport)?;
    let Some(id_token) = body["id_token"].as_str().filter(|t| !t.is_empty()) else {
        return Err(failed("The provider sent no ID token."));
    };
    let jwks = get_json(meta["jwks_uri"].as_str().unwrap_or_default()).await?;
    let nonce = ch.text("nonce").unwrap_or_default().to_string();
    let claims =
        verify_id_token(cfg, id_token, jwks, &nonce).map_err(|e| failed(format!("The ID token is not valid: {e}")))?;
    let name = ["name", "preferred_username", "email"]
        .iter()
        .find_map(|k| match &claims[*k] {
            Value::Null | Value::Bool(false) => None,
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        })
        .map(|n| truncate_chars(&n, 80).to_string());
    let auth_time = match &claims["auth_time"] {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        _ => None,
    };
    let sub = match &claims["sub"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Ok(OidcResult {
        mode: ch.text("mode").unwrap_or_default().to_string(),
        subject: format!("{}#{sub}", cfg.issuer),
        name,
        auth_time,
        challenge: ch,
    })
}

/// Signature (JWKS), iss, aud, exp (60 s leeway), iat and nonce; returns the claims as JSON.
fn verify_id_token(cfg: &OidcConfig, id_token: &str, jwks: Value, nonce: &str) -> Result<Value, String> {
    let keys: CoreJsonWebKeySet = serde_json::from_value(jwks).map_err(|e| e.to_string())?;
    let issuer = IssuerUrl::new(cfg.issuer.clone()).map_err(|e| e.to_string())?;
    let client = ClientId::new(cfg.client_id.clone());
    let verifier = match &cfg.client_secret {
        Some(s) => CoreIdTokenVerifier::new_confidential_client(client, ClientSecret::new(s.clone()), issuer, keys),
        None => CoreIdTokenVerifier::new_public_client(client, issuer, keys),
    }
    .set_allowed_algs(ALGS)
    .set_other_audience_verifier_fn(|_| true)
    .set_time_fn(|| Utc::now() - chrono::Duration::seconds(LEEWAY_SECONDS))
    .set_issue_time_verifier_fn(|iat| {
        if iat > Utc::now() + chrono::Duration::seconds(LEEWAY_SECONDS) {
            Err("the token is issued in the future".into())
        } else {
            Ok(())
        }
    });
    let token: CoreIdToken = id_token.parse().map_err(|e: serde_json::Error| e.to_string())?;
    token
        .claims(&verifier, &Nonce::new(nonce.to_string()))
        .map_err(|e| e.to_string())?;
    // verified: read the payload as plain JSON for the optional claims
    let payload = id_token.split('.').nth(1).ok_or("not a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_matches_python() {
        assert_eq!(quote_plus("openid profile email"), "openid+profile+email");
        assert_eq!(
            quote_plus("https://testserver/api/auth/oidc/callback"),
            "https%3A%2F%2Ftestserver%2Fapi%2Fauth%2Foidc%2Fcallback"
        );
        assert_eq!(quote_plus("a~b*c"), "a~b%2Ac");
        assert_eq!(s256("x").len(), 43);
        assert_eq!(binder_hash("b").len(), 64);
    }
}
