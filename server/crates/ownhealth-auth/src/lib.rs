//! Lane G: security and auth (`auth/`; security-privacy.md, mcp.md 2). The export is in `ownhealth-ops`.
//!
//! `principal::resolve` (Bearer PAT, session cookie, dev auth, in that order) and the `csrf` check
//! run on every request. The rest is the login side: first-run owner, passkeys (webauthn-rs, reading
//! the rows py_webauthn wrote), recovery codes, the password fallback, OIDC, sessions and step-up,
//! PATs and the household (invites, relatives, claiming, deletion). The formats that
//! must not change are in RUST-CONTRACTS.md section 7.

pub mod challenges;
pub mod csrf;
pub mod household;
pub mod oidc;
pub mod passkeys;
pub mod password;
pub mod principal;
pub mod recovery;
pub mod sessions;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tokens;

use serde_json::{Map, Value};

/// A refused login or household step with a stable `code` (Python's PasskeyError, ChallengeError,
/// HouseholdError and OIDCError), or a core error (database, access).
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{message}")]
    Refused {
        code: String,
        message: String,
        /// Merged into the error body after code and message (e.g. `profiles`).
        extra: Map<String, Value>,
    },
    #[error(transparent)]
    Core(#[from] ownhealth_core::Error),
}

pub type AuthResult<T> = Result<T, AuthError>;

impl AuthError {
    pub fn refused(code: &str, message: impl Into<String>) -> AuthError {
        AuthError::Refused {
            code: code.into(),
            message: message.into(),
            extra: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: Value) -> AuthError {
        if let AuthError::Refused { extra, .. } = &mut self {
            extra.insert(key.into(), value);
        }
        self
    }

    /// The stable code: the refusal's, or `bad_input` (Python's `getattr(exc, "code", "bad_input")`).
    pub fn code(&self) -> &str {
        match self {
            AuthError::Refused { code, .. } => code,
            AuthError::Core(_) => "bad_input",
        }
    }
}

impl From<sqlx::Error> for AuthError {
    fn from(e: sqlx::Error) -> AuthError {
        AuthError::Core(e.into())
    }
}

/// Python's `urlsplit(url)`: `(scheme, netloc)`, netloc as written.
pub fn split_url(url: &str) -> (&str, &str) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return ("", "");
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    (scheme, netloc)
}

/// `urlsplit(url).hostname`: the host of the netloc, lowercased, without userinfo, port or brackets.
pub fn url_hostname(url: &str) -> Option<String> {
    let (_, netloc) = split_url(url);
    let host = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or_default()
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// `secrets.token_urlsafe(n)`: base64url of n random bytes, no padding.
pub fn token_urlsafe(n: usize) -> String {
    use base64::Engine;
    let mut b = vec![0u8; n];
    rand::fill(&mut b[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// sha256 of the UTF-8 text, as the bytea the tables store.
pub fn sha256(text: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).to_vec()
}

/// Python's `s[:n]` on characters.
pub fn truncate_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// Register lane G's demo hook (none: the demo family needs no login).
pub fn register() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlsplit_parts() {
        assert_eq!(
            split_url("https://Health.example:8443/x?y"),
            ("https", "Health.example:8443")
        );
        assert_eq!(
            url_hostname("https://Health.example:8443/x").as_deref(),
            Some("health.example")
        );
        assert_eq!(url_hostname("http://u:p@[::1]:80").as_deref(), Some("::1"));
        assert_eq!(url_hostname("http://localhost").as_deref(), Some("localhost"));
        assert_eq!(url_hostname("nonsense"), None);
        assert_eq!(token_urlsafe(32).len(), 43);
        assert_eq!(truncate_chars("Lucía Rivera", 4), "Lucí");
    }
}
