//! Inbox credentials at rest (security-privacy.md 7.1, ingestion.md 3.2): an IMAP password, a
//! Paperless or Nightscout token, a per-sender PDF password. AES-256-GCM with the key in
//! `OWNHEALTH_SECRETS_KEY` (or `OWNHEALTH_SECRETS_KEY_FILE`): 32 random bytes, standard base64
//! (`openssl rand -base64 32`).
//!
//! The `secret` row id and purpose are the associated data, so a ciphertext copied into another row
//! does not open. A value read back is a [`SecretString`], whose `Debug` and `Display` print `***`.
//! Nothing here logs, and no error message carries a value.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::config::Settings;
use crate::error::{Error, Result};

/// The `secret.purpose` check constraint.
pub const PURPOSES: [&str; 4] = ["imap_password", "paperless_token", "nightscout_token", "pdf_password"];

const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;

/// A secret in memory. Never printed; `expose()` is the one way to read it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        SecretString(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// The sealing key. Cheap to clone; `Debug` shows only the key fingerprint.
#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
    fingerprint: [u8; 8],
}

impl std::fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBox({})", hex::encode(self.fingerprint))
    }
}

fn not_configured() -> Error {
    Error::Invalid(
        "OWNHEALTH_SECRETS_KEY is not set: generate one with `openssl rand -base64 32` and set it (or \
         OWNHEALTH_SECRETS_KEY_FILE) before saving a password or token"
            .into(),
    )
}

impl SecretBox {
    /// A key from 32 raw bytes.
    pub fn new(key: &[u8]) -> Result<SecretBox> {
        if key.len() != 32 {
            return Err(Error::Invalid(format!(
                "OWNHEALTH_SECRETS_KEY must be 32 bytes, got {}",
                key.len()
            )));
        }
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| Error::Invalid("bad secrets key".into()))?;
        let digest = Sha256::digest(key);
        let mut fingerprint = [0u8; 8];
        fingerprint.copy_from_slice(&digest[..8]);
        Ok(SecretBox { cipher, fingerprint })
    }

    /// The key of `OWNHEALTH_SECRETS_KEY`: standard base64 of 32 bytes.
    pub fn from_base64(value: &str) -> Result<SecretBox> {
        let key = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .map_err(|_| Error::Invalid("OWNHEALTH_SECRETS_KEY is not standard base64".into()))?;
        SecretBox::new(&key)
    }

    /// The configured key; `Error::Invalid` with the fix when it is not set.
    pub fn from_settings(settings: &Settings) -> Result<SecretBox> {
        SecretBox::from_base64(settings.secrets_key.as_deref().ok_or_else(not_configured)?)
    }

    pub fn fingerprint(&self) -> &[u8; 8] {
        &self.fingerprint
    }

    /// `version || nonce || ciphertext+tag`, bound to `aad`.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce[..]);
        let ct = self
            .cipher
            .encrypt(&Nonce::from(nonce), Payload { msg: plaintext, aad })
            .map_err(|_| Error::Invalid("could not seal the secret".into()))?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open what `seal` made with the same key and `aad`. A wrong key, other associated data or a
    /// changed byte is the same error, without detail.
    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let refused = || Error::Invalid("the stored secret does not open with this key".into());
        if sealed.len() < 1 + NONCE_LEN + 16 || sealed[0] != VERSION {
            return Err(refused());
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&sealed[1..1 + NONCE_LEN]);
        self.cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &sealed[1 + NONCE_LEN..],
                    aad,
                },
            )
            .map_err(|_| refused())
    }
}

fn aad(id: Uuid, purpose: &str) -> Vec<u8> {
    let mut a = id.as_bytes().to_vec();
    a.extend_from_slice(purpose.as_bytes());
    a
}

fn check_purpose(purpose: &str) -> Result<()> {
    if PURPOSES.contains(&purpose) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("unknown secret purpose {purpose:?}")))
    }
}

/// Seal and store a new secret; returns its id.
pub async fn store_secret(
    conn: &mut PgConnection,
    sb: &SecretBox,
    household_id: Uuid,
    purpose: &str,
    label: Option<&str>,
    value: &SecretString,
) -> Result<Uuid> {
    check_purpose(purpose)?;
    let id = Uuid::new_v4();
    let sealed = sb.seal(value.expose().as_bytes(), &aad(id, purpose))?;
    sqlx::query(
        "insert into secret (id, household_id, purpose, label, key_fingerprint, ciphertext) values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(household_id)
    .bind(purpose)
    .bind(label)
    .bind(&sb.fingerprint[..])
    .bind(&sealed)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Replace the value of an existing secret (a changed password), keeping its id and purpose.
pub async fn replace_secret(conn: &mut PgConnection, sb: &SecretBox, id: Uuid, value: &SecretString) -> Result<()> {
    let purpose: Option<String> = sqlx::query_scalar("select purpose from secret where id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let purpose = purpose.ok_or_else(|| Error::NotFound("Unknown secret.".into()))?;
    let sealed = sb.seal(value.expose().as_bytes(), &aad(id, &purpose))?;
    sqlx::query("update secret set ciphertext = $2, key_fingerprint = $3, updated_at = now() where id = $1")
        .bind(id)
        .bind(&sealed)
        .bind(&sb.fingerprint[..])
        .execute(conn)
        .await?;
    Ok(())
}

/// Read a secret back. `household_id` must own it. A key other than the one that sealed it is
/// `Error::Invalid` naming the problem (rotated key), never the value.
pub async fn read_secret(
    conn: &mut PgConnection,
    sb: &SecretBox,
    household_id: Uuid,
    id: Uuid,
) -> Result<SecretString> {
    let row: Option<(String, Vec<u8>, Vec<u8>)> =
        sqlx::query_as("select purpose, key_fingerprint, ciphertext from secret where id = $1 and household_id = $2")
            .bind(id)
            .bind(household_id)
            .fetch_optional(conn)
            .await?;
    let (purpose, fp, sealed) = row.ok_or_else(|| Error::NotFound("Unknown secret.".into()))?;
    if fp != sb.fingerprint {
        return Err(Error::Invalid(
            "this secret was sealed with another OWNHEALTH_SECRETS_KEY; enter it again".into(),
        ));
    }
    let plain = sb.open(&sealed, &aad(id, &purpose))?;
    String::from_utf8(plain)
        .map(SecretString)
        .map_err(|_| Error::Invalid("the stored secret is not text".into()))
}

pub async fn delete_secret(conn: &mut PgConnection, household_id: Uuid, id: Uuid) -> Result<bool> {
    let n = sqlx::query("delete from secret where id = $1 and household_id = $2")
        .bind(id)
        .bind(household_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb() -> SecretBox {
        SecretBox::new(&[7u8; 32]).expect("key")
    }

    #[test]
    fn seal_and_open_round_trip_and_bind_the_associated_data() {
        let b = sb();
        let sealed = b.seal(b"12345678Z", b"row-1").expect("seal");
        assert_eq!(b.open(&sealed, b"row-1").expect("open"), b"12345678Z");
        assert!(
            b.open(&sealed, b"row-2").is_err(),
            "another row's associated data opens it"
        );
        let other = SecretBox::new(&[8u8; 32]).expect("key");
        assert!(other.open(&sealed, b"row-1").is_err(), "another key opens it");
        let mut flipped = sealed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(b.open(&flipped, b"row-1").is_err(), "a changed byte opens");
        assert_ne!(
            b.seal(b"x", b"a").expect("seal"),
            b.seal(b"x", b"a").expect("seal"),
            "nonce reused"
        );
        assert!(
            !sealed.windows(9).any(|w| w == b"12345678Z"),
            "plaintext in the sealed bytes"
        );
    }

    #[test]
    fn the_key_comes_from_base64_of_32_bytes_and_errors_name_no_value() {
        assert!(SecretBox::from_base64("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").is_ok());
        let short = SecretBox::from_base64("c2hvcnQ=").expect_err("refused");
        assert!(short.to_string().contains("32 bytes"));
        let missing = SecretBox::from_settings(&Settings::default()).expect_err("refused");
        assert!(missing.to_string().contains("OWNHEALTH_SECRETS_KEY"));
    }

    #[test]
    fn a_secret_never_prints() {
        let s = SecretString::new("12345678Z");
        assert_eq!(format!("{s} {s:?}"), "*** SecretString(***)");
        assert!(format!("{:?}", sb()).starts_with("SecretBox("));
    }
}
