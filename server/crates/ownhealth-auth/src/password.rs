//! Password fallback, only where passkeys cannot run (`auth/password.py`).
//!
//! WebAuthn needs a secure context: https, or http on localhost / *.localhost. An install reached over
//! plain HTTP at a LAN address cannot create or use a passkey, so there, and only there, the owner may
//! set a password. Behind HTTPS the password routes answer 403 `passkeys_available`.
//!
//! Hash: scrypt (n=2^14, r=8, p=1, 16 MiB), stored as `scrypt$n$r$p$salt$hash` in standard base64.
//! Hashing takes tens of milliseconds, so the async helpers run it on the blocking pool.

use base64::Engine;
use ownhealth_core::{Error, Result};
use sqlx::PgConnection;
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub const MIN_LENGTH: usize = 12;
const LOG_N: u8 = 14;
const R: u32 = 8;
const P: u32 = 1;

/// True when a browser at `base_url` is a secure context, so WebAuthn can run. An IP address is never
/// a WebAuthn RP ID, so a loopback IP gets the password fallback instead.
pub fn passkeys_possible(base_url: &str) -> bool {
    let (scheme, _) = crate::split_url(base_url);
    let host = crate::url_hostname(base_url).unwrap_or_default();
    scheme == "https" || host == "localhost" || host.ends_with(".localhost")
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn derive(password: &str, salt: &[u8], log_n: u8, r: u32, p: u32, len: usize) -> Option<Vec<u8>> {
    let params = scrypt::Params::new(log_n, r, p).ok()?;
    let mut out = vec![0u8; len];
    scrypt::scrypt(password.as_bytes(), salt, &params, &mut out).ok()?;
    Some(out)
}

pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    rand::fill(&mut salt[..]);
    // Params (14, 8, 1) and a 32-byte output are always valid.
    let digest = derive(password, &salt, LOG_N, R, P, 32).unwrap_or_default();
    format!(
        "scrypt${}${R}${P}${}${}",
        1u32 << LOG_N,
        b64().encode(salt),
        b64().encode(digest)
    )
}

/// Constant-time check of `password` against a stored `scrypt$...` string; false for anything else.
pub fn check_password(password: &str, encoded: Option<&str>) -> bool {
    let Some(encoded) = encoded.filter(|e| !e.is_empty()) else {
        return false;
    };
    let parts: Vec<&str> = encoded.split('$').collect();
    let [algo, n, r, p, salt, digest] = parts[..] else {
        return false;
    };
    if algo != "scrypt" {
        return false;
    }
    let (Ok(n), Ok(r), Ok(p)) = (n.parse::<u64>(), r.parse::<u32>(), p.parse::<u32>()) else {
        return false;
    };
    if n < 2 || !n.is_power_of_two() {
        return false;
    }
    let (Ok(salt), Ok(want)) = (b64().decode(salt), b64().decode(digest)) else {
        return false;
    };
    let Some(got) = derive(password, &salt, n.trailing_zeros() as u8, r, p, want.len()) else {
        return false;
    };
    got.ct_eq(&want).into()
}

/// `hash_password` on the blocking pool.
pub async fn hash_password_async(password: &str) -> Result<String> {
    let pw = password.to_string();
    tokio::task::spawn_blocking(move || hash_password(&pw))
        .await
        .map_err(|e| Error::Other(e.into()))
}

/// `check_password` on the blocking pool.
pub async fn check_password_async(password: &str, encoded: Option<String>) -> Result<bool> {
    let pw = password.to_string();
    tokio::task::spawn_blocking(move || check_password(&pw, encoded.as_deref()))
        .await
        .map_err(|e| Error::Other(e.into()))
}

/// Python's `validate`: at least 12 characters.
pub fn validate(password: &str) -> Result<()> {
    if password.chars().count() < MIN_LENGTH {
        return Err(Error::Invalid(format!("Use at least {MIN_LENGTH} characters.")));
    }
    Ok(())
}

pub async fn set_password(conn: &mut PgConnection, user_id: Uuid, password: &str) -> Result<()> {
    validate(password)?;
    let h = hash_password_async(password).await?;
    sqlx::query("update app_user set password_hash = $1 where id = $2")
        .bind(h)
        .bind(user_id)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn verify_user_password(conn: &mut PgConnection, user_id: Uuid, password: &str) -> Result<bool> {
    let encoded: Option<Option<String>> =
        sqlx::query_scalar("select password_hash from app_user where id = $1 and disabled_at is null")
            .bind(user_id)
            .fetch_optional(conn)
            .await?;
    check_password_async(password, encoded.flatten()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: &str = "http://192.168.1.20:8080";

    #[test]
    fn secure_context_rule() {
        assert!(passkeys_possible("https://health.example.lan"));
        assert!(passkeys_possible("http://localhost:8080"));
        assert!(
            !passkeys_possible("http://127.0.0.1:8080"),
            "an IP is never a WebAuthn RP ID"
        );
        assert!(passkeys_possible("http://ownhealth.localhost"));
        assert!(!passkeys_possible(LAN) && !passkeys_possible("http://health.example.lan"));
    }

    #[test]
    fn hash() {
        let h = hash_password("correct horse battery");
        assert!(h.starts_with("scrypt$16384$8$1$") && !h.contains("correct"));
        assert!(check_password("correct horse battery", Some(&h)));
        assert!(!check_password("correct horse batterx", Some(&h)));
        assert_ne!(hash_password("correct horse battery"), h, "salted");
        assert!(!check_password("x", None) && !check_password("x", Some("garbage")));
    }

    #[test]
    fn verifies_a_hash_python_made() {
        // password.hash_password("carmen-ortega-labnorte-2026") in 0.1.1 (tests/auth_python_rows.json)
        let py = "scrypt$16384$8$1$o6t1Snlqx1uVDsfAsT5IYw==$zga3nP2cXIHg25UMEBiPbNtrH/wCHSBcOYZhHZMaytA="; // ggignore: synthetic test hash
        assert!(check_password("carmen-ortega-labnorte-2026", Some(py)));
        assert!(!check_password("carmen-ortega-labnorte-2025", Some(py)));
    }
}
