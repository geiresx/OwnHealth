//! Passkey login (`auth/passkeys.py`, security-privacy.md 5), verified with webauthn-rs.
//!
//! WebAuthn runs only in a secure context: https, or http on localhost. The relying party id is the
//! host of OWNHEALTH_BASE_URL and the expected origin is that URL. Every challenge is single use and
//! expires after 5 minutes (`auth_challenge`). Login is username-less: the passkey is a discoverable
//! credential and the browser picks it.
//!
//! Compatibility with 0.1.1 (py_webauthn): the options sent to the browser have py_webauthn's JSON
//! shape, and a `passkey` row keeps what py_webauthn kept: the credential id, the raw COSE public key
//! from the attested credential data, and the sign count. A login rebuilds the webauthn-rs credential
//! from those three columns (SPIKES.md c). The webauthn-rs ceremony state rides in the challenge row's
//! `data.webauthn`.

use std::time::Duration;

use base64::Engine;
use ownhealth_core::Error;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;
use webauthn_rs::prelude::Url;
use webauthn_rs_core::WebauthnCore;
use webauthn_rs_core::proto::{
    AttestationConveyancePreference, AttestationFormat, AttestationMetadata, AuthenticationState, COSEAlgorithm,
    COSEKey, Credential, ParsedAttestation, ParsedAttestationData, PublicKeyCredential, RegisterPublicKeyCredential,
    RegisteredExtensions, RegistrationState, UserVerificationPolicy,
};

use crate::challenges::{self, Challenge, Issue};
use crate::{AuthError, AuthResult, truncate_chars};

pub const RP_NAME: &str = "OwnHealth";
const TIMEOUT_MS: u64 = 60_000;
/// py_webauthn's default offer: EdDSA, ES256, RS256.
const ALGORITHMS: [COSEAlgorithm; 3] = [COSEAlgorithm::EDDSA, COSEAlgorithm::ES256, COSEAlgorithm::RS256];
const TRANSPORTS: [&str; 6] = ["ble", "cable", "hybrid", "internal", "nfc", "usb"];

/// The relying party: `(rp_id, expected origin)` from OWNHEALTH_BASE_URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rp {
    pub id: String,
    pub origin: String,
}

impl Rp {
    pub fn from_base_url(base_url: &str) -> Rp {
        let (scheme, netloc) = crate::split_url(base_url);
        Rp {
            id: crate::url_hostname(base_url).unwrap_or_else(|| "localhost".into()),
            origin: format!("{scheme}://{netloc}"),
        }
    }

    fn core(&self) -> AuthResult<WebauthnCore> {
        let origin =
            Url::parse(&self.origin).map_err(|e| Error::Invalid(format!("OWNHEALTH_BASE_URL is not a URL: {e}")))?;
        Ok(WebauthnCore::new_unsafe_experts_only(
            RP_NAME,
            &self.id,
            vec![origin],
            Duration::from_millis(TIMEOUT_MS),
            None,
            None,
        ))
    }
}

fn b64u() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// base64url, padded or not (Python's `urlsafe_b64decode(s + "=" * ...)`).
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    b64u().decode(s.trim_end_matches('=')).ok()
}

pub fn b64url(b: &[u8]) -> String {
    b64u().encode(b)
}

fn malformed() -> AuthError {
    AuthError::refused("passkey_invalid", "The passkey response is malformed.")
}

fn not_verified(e: impl std::fmt::Display) -> AuthError {
    AuthError::refused("passkey_invalid", format!("The passkey could not be verified: {e}"))
}

fn client_challenge(credential: &Value) -> AuthResult<Vec<u8>> {
    let raw = credential["response"]["clientDataJSON"]
        .as_str()
        .ok_or_else(malformed)?;
    let client: Value = serde_json::from_slice(&b64url_decode(raw).ok_or_else(malformed)?).map_err(|_| malformed())?;
    b64url_decode(client["challenge"].as_str().ok_or_else(malformed)?).ok_or_else(malformed)
}

/// The user's passkeys as descriptors `{"id", "type", "transports"?}`, in py_webauthn's JSON.
async fn user_credentials(conn: &mut PgConnection, user_id: Uuid) -> AuthResult<Vec<(Vec<u8>, Value)>> {
    let rows: Vec<(Vec<u8>, Vec<String>)> = sqlx::query_as("select id, transports from passkey where user_id = $1")
        .bind(user_id)
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, transports)| {
            let mut d = Map::new();
            d.insert("id".into(), json!(b64url(&id)));
            d.insert("type".into(), json!("public-key"));
            let known: Vec<&String> = transports.iter().filter(|t| TRANSPORTS.contains(&t.as_str())).collect();
            if !known.is_empty() {
                d.insert("transports".into(), json!(known));
            }
            (id, Value::Object(d))
        })
        .collect())
}

fn state_of<T: serde::de::DeserializeOwned>(ch: &Challenge) -> AuthResult<T> {
    ch.data
        .get("webauthn")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| {
            AuthError::refused(
                "challenge_invalid",
                "This request expired or was already used. Start again.",
            )
        })
}

// ------------------------------------------------------------- registration ----

/// Creation options for navigator.credentials.create(). `user_id` may be a not-yet-created user
/// (first-run setup, invite): then pass purpose `setup` or `enroll` and the row is checked at finish.
pub async fn begin_registration(
    conn: &mut PgConnection,
    user_id: Uuid,
    rp: &Rp,
    user_name: &str,
    purpose: &str,
    data: Map<String, Value>,
) -> AuthResult<Value> {
    let existing = purpose == "register";
    let exclude = if existing {
        user_credentials(&mut *conn, user_id).await?
    } else {
        Vec::new()
    };
    let name = if user_name.is_empty() {
        user_id.to_string()
    } else {
        user_name.to_string()
    };
    let core = rp.core()?;
    let builder = core
        .new_challenge_register_builder(user_id.as_bytes(), &name, &name)
        .map_err(|e| Error::Invalid(format!("passkey options: {e:?}")))?
        .attestation(AttestationConveyancePreference::None)
        .credential_algorithms(ALGORITHMS.to_vec())
        .require_resident_key(true)
        .authenticator_attachment(None)
        .user_verification_policy(UserVerificationPolicy::Required)
        .reject_synchronised_authenticators(false)
        .exclude_credentials(Some(exclude.iter().map(|(id, _)| id.clone()).collect()))
        .extensions(None);
    let (ccr, state) = core
        .generate_challenge_register(builder)
        .map_err(|e| Error::Invalid(format!("passkey options: {e:?}")))?;
    let challenge: Vec<u8> = ccr.public_key.challenge.to_vec();
    let mut row_data = data;
    row_data.insert("user_id".into(), json!(user_id.to_string()));
    row_data.insert("origin".into(), json!(rp.origin));
    row_data.insert("webauthn".into(), serde_json::to_value(&state).map_err(Error::from)?);
    challenges::issue(
        conn,
        purpose,
        &challenge,
        Issue {
            user_id: existing.then_some(user_id),
            data: row_data,
            ..Issue::default()
        },
    )
    .await?;
    let params: Vec<Value> = ALGORITHMS
        .iter()
        .map(|a| json!({"type": "public-key", "alg": *a as i64}))
        .collect();
    Ok(json!({
        "rp": {"name": RP_NAME, "id": rp.id},
        "user": {"id": b64url(user_id.as_bytes()), "name": name, "displayName": name},
        "challenge": b64url(&challenge),
        "pubKeyCredParams": params,
        "timeout": TIMEOUT_MS,
        "excludeCredentials": exclude.into_iter().map(|(_, d)| d).collect::<Vec<_>>(),
        "authenticatorSelection": {"residentKey": "required", "requireResidentKey": true, "userVerification": "required"},
        "attestation": "none",
    }))
}

/// A verified registration, not stored yet.
#[derive(Debug, Clone)]
pub struct Registered {
    pub challenge: Challenge,
    pub credential_id: Vec<u8>,
    /// The raw COSE key from the attested credential data, as py_webauthn stored it.
    pub public_key: Vec<u8>,
    pub sign_count: u32,
    pub transports: Vec<String>,
}

/// The COSE key bytes of the attested credential data in an attestation object.
fn attested_cose_key(attestation_object: &[u8]) -> Option<Vec<u8>> {
    let att: serde_cbor_2::Value = serde_cbor_2::from_slice(attestation_object).ok()?;
    let serde_cbor_2::Value::Map(m) = att else {
        return None;
    };
    let auth = m.iter().find_map(|(k, v)| match (k, v) {
        (serde_cbor_2::Value::Text(k), serde_cbor_2::Value::Bytes(b)) if k == "authData" => Some(b),
        _ => None,
    })?;
    // rpIdHash 32, flags 1, signCount 4, aaguid 16, credentialIdLength 2, credentialId, publicKey
    if auth.len() < 55 || auth[32] & 0x40 == 0 {
        return None;
    }
    let len = u16::from_be_bytes([auth[53], auth[54]]) as usize;
    let rest = auth.get(55 + len..)?;
    let mut de = serde_cbor_2::Deserializer::from_slice(rest);
    let _: serde_cbor_2::Value = serde::Deserialize::deserialize(&mut de).ok()?;
    Some(rest[..de.byte_offset()].to_vec())
}

/// Check the attestation against a live challenge; consumes it. Does not store anything.
pub async fn verify_registration(
    conn: &mut PgConnection,
    credential: &Value,
    rp: &Rp,
    purposes: &[&str],
) -> AuthResult<Registered> {
    let ch = challenges::consume(&mut *conn, &client_challenge(credential)?, purposes).await?;
    let state: RegistrationState = state_of(&ch)?;
    let reg: RegisterPublicKeyCredential = serde_json::from_value(credential.clone()).map_err(not_verified)?;
    let cred = rp
        .core()?
        .register_credential(&reg, &state, None)
        .map_err(not_verified)?;
    let public_key = attested_cose_key(reg.response.attestation_object.as_ref())
        .ok_or_else(|| not_verified("no attested credential data"))?;
    let transports: Vec<String> = credential["response"]["transports"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .take(8)
                .collect()
        })
        .unwrap_or_default();
    Ok(Registered {
        challenge: ch,
        credential_id: cred.cred_id.clone(),
        public_key,
        sign_count: cred.counter,
        transports,
    })
}

pub async fn store_passkey(
    conn: &mut PgConnection,
    user_id: Uuid,
    reg: &Registered,
    label: Option<&str>,
) -> AuthResult<Vec<u8>> {
    let exists: Option<i32> = sqlx::query_scalar("select 1 from passkey where id = $1")
        .bind(&reg.credential_id)
        .fetch_optional(&mut *conn)
        .await?;
    if exists.is_some() {
        return Err(AuthError::refused(
            "passkey_exists",
            "This passkey is already registered.",
        ));
    }
    let label = truncate_chars(label.unwrap_or_default().trim(), 80);
    sqlx::query(
        "insert into passkey (id, user_id, public_key, sign_count, transports, label) values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&reg.credential_id)
    .bind(user_id)
    .bind(&reg.public_key)
    .bind(i64::from(reg.sign_count))
    .bind(&reg.transports)
    .bind((!label.is_empty()).then_some(label))
    .execute(conn)
    .await?;
    Ok(reg.credential_id.clone())
}

/// Add a passkey to an existing, logged-in user. Returns the user id.
pub async fn finish_registration(
    conn: &mut PgConnection,
    user_id: Uuid,
    credential: &Value,
    rp: &Rp,
    label: Option<&str>,
) -> AuthResult<Uuid> {
    let reg = verify_registration(&mut *conn, credential, rp, &["register"]).await?;
    if reg.challenge.user_id != Some(user_id) {
        return Err(AuthError::refused(
            "passkey_invalid",
            "This registration was started by another user.",
        ));
    }
    store_passkey(conn, user_id, &reg, label).await?;
    Ok(user_id)
}

// -------------------------------------------------------------------- login ----

/// Request options for navigator.credentials.get(). With `user_id` it is a step-up for that user.
pub async fn begin_login(
    conn: &mut PgConnection,
    rp: &Rp,
    user_id: Option<Uuid>,
    session_id: Option<Uuid>,
) -> AuthResult<Value> {
    let allow = match user_id {
        Some(u) => user_credentials(&mut *conn, u).await?,
        None => Vec::new(),
    };
    if user_id.is_some() && allow.is_empty() {
        return Err(AuthError::refused("no_passkey", "This account has no passkey yet."));
    }
    let core = rp.core()?;
    // The state names no credential: finish_login finds the row by its id and checks the user.
    let builder = core
        .new_challenge_authenticate_builder(Vec::new(), Some(UserVerificationPolicy::Required))
        .map_err(|e| Error::Invalid(format!("passkey options: {e:?}")))?
        .extensions(None)
        .allow_backup_eligible_upgrade(true);
    let (rcr, state) = core
        .generate_challenge_authenticate(builder)
        .map_err(|e| Error::Invalid(format!("passkey options: {e:?}")))?;
    let challenge: Vec<u8> = rcr.public_key.challenge.to_vec();
    let mut data = Map::new();
    data.insert("webauthn".into(), serde_json::to_value(&state).map_err(Error::from)?);
    let purpose = if user_id.is_some() { "step_up" } else { "login" };
    challenges::issue(
        conn,
        purpose,
        &challenge,
        Issue {
            user_id,
            session_id,
            data,
            ..Issue::default()
        },
    )
    .await?;
    Ok(json!({
        "challenge": b64url(&challenge),
        "timeout": TIMEOUT_MS,
        "rpId": rp.id,
        "allowCredentials": allow.into_iter().map(|(_, d)| d).collect::<Vec<_>>(),
        "userVerification": "required",
    }))
}

/// The webauthn-rs credential for a stored row: the COSE key, the sign count, and the backup flags
/// the assertion reports (py_webauthn kept no backup state, and did not check it).
pub fn credential_from_row(cred_id: Vec<u8>, public_key: &[u8], sign_count: i64, flags: u8) -> Option<Credential> {
    let cbor: serde_cbor_2::Value = serde_cbor_2::from_slice(public_key).ok()?;
    let cred = COSEKey::try_from(&cbor).ok()?;
    Some(Credential {
        cred_id,
        cred,
        counter: u32::try_from(sign_count).unwrap_or(u32::MAX),
        transports: None,
        user_verified: true,
        backup_eligible: flags & 0x08 != 0,
        backup_state: flags & 0x10 != 0,
        registration_policy: UserVerificationPolicy::Required,
        extensions: RegisteredExtensions::none(),
        attestation: ParsedAttestation {
            data: ParsedAttestationData::None,
            metadata: AttestationMetadata::None,
        },
        attestation_format: AttestationFormat::None,
    })
}

/// Verify an assertion. Returns the user id; the caller creates the session (login) or marks it
/// stepped up (step_up).
pub async fn finish_login(conn: &mut PgConnection, credential: &Value, rp: &Rp, purpose: &str) -> AuthResult<Uuid> {
    let ch = challenges::consume(&mut *conn, &client_challenge(credential)?, &[purpose]).await?;
    let id_text = credential["rawId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| credential["id"].as_str())
        .ok_or_else(malformed)?;
    let cred_id = b64url_decode(id_text).ok_or_else(malformed)?;
    let row: Option<(Uuid, Vec<u8>, i64)> = sqlx::query_as(
        "select p.user_id, p.public_key, p.sign_count from passkey p join app_user u on u.id = p.user_id \
         where p.id = $1 and u.disabled_at is null",
    )
    .bind(&cred_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((user_id, public_key, sign_count)) = row else {
        return Err(AuthError::refused(
            "passkey_unknown",
            "This passkey is not registered here.",
        ));
    };
    if ch.user_id.is_some_and(|u| u != user_id) {
        return Err(AuthError::refused(
            "passkey_invalid",
            "This passkey belongs to another user.",
        ));
    }
    let state: AuthenticationState = state_of(&ch)?;
    let mut pkc: PublicKeyCredential = serde_json::from_value(credential.clone()).map_err(not_verified)?;
    pkc.raw_id = cred_id.clone();
    let flags = pkc.response.authenticator_data.get(32).copied().unwrap_or(0);
    let cred = credential_from_row(cred_id.clone(), &public_key, sign_count, flags)
        .ok_or_else(|| not_verified("the stored public key is not a COSE key"))?;
    let result = rp
        .core()?
        .authenticate_single_credential(&pkc, &state, &cred)
        .map_err(not_verified)?;
    sqlx::query("update passkey set sign_count = $1, last_used_at = now() where id = $2")
        .bind(i64::from(result.counter()))
        .bind(&cred_id)
        .execute(conn)
        .await?;
    Ok(user_id)
}

/// `{"id" (base64url), "label", "transports", "created_at", "last_used_at"}` per passkey.
pub async fn list_passkeys(conn: &mut PgConnection, user_id: Uuid) -> AuthResult<Vec<Value>> {
    type Row = (
        Vec<u8>,
        Option<String>,
        Vec<String>,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select id, label, transports, created_at, last_used_at from passkey where user_id = $1 order by created_at",
    )
    .bind(user_id)
    .fetch_all(conn)
    .await?;
    // the routes that send this are annotated `-> dict`: pydantic writes UTC as `Z`
    use ownhealth_core::pyjson::pydantic_datetime_utc;
    Ok(rows
        .into_iter()
        .map(|(id, label, transports, created, used)| {
            json!({
                "id": b64url(&id),
                "label": label,
                "transports": transports,
                "created_at": pydantic_datetime_utc(&created),
                "last_used_at": used.as_ref().map(pydantic_datetime_utc),
            })
        })
        .collect())
}

pub async fn delete_passkey(conn: &mut PgConnection, user_id: Uuid, credential_id_b64: &str) -> AuthResult<bool> {
    let Some(id) = b64url_decode(credential_id_b64) else {
        return Ok(false);
    };
    let n = sqlx::query("delete from passkey where id = $1 and user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rp_from_base_url() {
        assert_eq!(
            Rp::from_base_url("https://testserver"),
            Rp {
                id: "testserver".into(),
                origin: "https://testserver".into()
            }
        );
        assert_eq!(
            Rp::from_base_url("http://localhost:8080").origin,
            "http://localhost:8080"
        );
        assert_eq!(b64url_decode("YWJj"), Some(b"abc".to_vec()));
        assert_eq!(b64url_decode("YWJj=="), Some(b"abc".to_vec()));
    }
}
