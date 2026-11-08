//! Personal access tokens for MCP clients (`auth/tokens.py`). Format `oh_pat_<id8>_<secret43>`: id8
//! is the lookup key, secret43 is 256 random bits in base64url. The row stores sha256(secret); a
//! 256-bit random secret needs no slow hash. The plaintext is returned once, at creation.

use std::collections::BTreeSet;

use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use ownhealth_core::access::{Principal, PrincipalKind, is_scope};
use ownhealth_core::{Error, Result};
use rand::RngExt;
use sqlx::PgConnection;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::sha256;

pub const PREFIX: &str = "oh_pat_";
pub const DEFAULT_DAYS: i64 = 90;
pub const MAX_DAYS: i64 = 365;
const ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// The per-token limits, in the column order of `api_token` (the MCP guard reads them).
pub const LIMIT_KEYS: [&str; 6] = [
    "rate_per_min",
    "egress_facts_per_day",
    "egress_bytes_per_day",
    "genome_raw_lookups_per_day",
    "document_pages_per_day",
    "proposals_per_day",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewToken {
    pub id: Uuid,
    /// Shown once.
    pub plaintext: String,
    pub prefix: String,
}

#[derive(Debug, Clone)]
pub struct TokenIn<'a> {
    pub user_id: Uuid,
    pub label: &'a str,
    pub profile_ids: Vec<Uuid>,
    pub scopes: Vec<String>,
    pub default_profile: Option<Uuid>,
    pub days: i64,
    /// full | pseudonymous | minimal
    pub identity_mode: &'a str,
    pub lang: &'a str,
    pub units: &'a str,
}

/// Mint a PAT. Profiles must be ones the user holds a grant on (else `AccessDenied`
/// `profile_not_granted`); scopes must be known and the expiry 1 to 365 days (else `Error::Invalid`,
/// Python's TokenError).
pub async fn create_token(conn: &mut PgConnection, t: &TokenIn<'_>) -> Result<NewToken> {
    if t.profile_ids.is_empty() {
        return Err(Error::Invalid("a token needs at least one profile".into()));
    }
    let unknown: BTreeSet<&String> = t.scopes.iter().filter(|s| !is_scope(s)).collect();
    if !unknown.is_empty() {
        let list: Vec<String> = unknown.iter().map(|s| format!("'{s}'")).collect();
        return Err(Error::Invalid(format!("unknown scopes [{}]", list.join(", "))));
    }
    if !(1..=MAX_DAYS).contains(&t.days) {
        return Err(Error::Invalid(format!("expiry must be 1 to {MAX_DAYS} days")));
    }
    let granted: Vec<Uuid> =
        sqlx::query_scalar("select profile_id from profile_grant where user_id = $1 and profile_id = any($2)")
            .bind(t.user_id)
            .bind(&t.profile_ids)
            .fetch_all(&mut *conn)
            .await?;
    if t.profile_ids.iter().any(|p| !granted.contains(p)) {
        return Err(Error::access_denied(
            "profile_not_granted",
            "The token names a profile you have no grant on.",
        ));
    }
    if let Some(dp) = t.default_profile
        && !t.profile_ids.contains(&dp)
    {
        return Err(Error::Invalid(
            "default_profile must be one of the token's profiles".into(),
        ));
    }
    let key: String = {
        let mut rng = rand::rng();
        (0..8)
            .map(|_| ID_ALPHABET[rng.random_range(0..ID_ALPHABET.len())] as char)
            .collect()
    };
    let mut raw = [0u8; 32];
    rand::fill(&mut raw[..]);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let prefix = format!("{PREFIX}{key}");
    let scopes: Vec<&String> = t.scopes.iter().collect::<BTreeSet<_>>().into_iter().collect();
    let id: Uuid = sqlx::query_scalar(
        "insert into api_token (user_id, label, prefix, secret_sha256, scopes, profile_ids, default_profile, \
         identity_mode, lang, units, expires_at) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning id",
    )
    .bind(t.user_id)
    .bind(t.label)
    .bind(&prefix)
    .bind(sha256(&secret))
    .bind(&scopes)
    .bind(&t.profile_ids)
    .bind(t.default_profile.unwrap_or(t.profile_ids[0]))
    .bind(t.identity_mode)
    .bind(t.lang)
    .bind(t.units)
    .bind(Utc::now() + Duration::days(t.days))
    .fetch_one(conn)
    .await?;
    Ok(NewToken {
        id,
        plaintext: format!("{prefix}_{secret}"),
        prefix,
    })
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: Uuid,
    user_id: Uuid,
    label: String,
    secret_sha256: Vec<u8>,
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
}

/// The token's Principal (kind token), or None for an unknown, expired or revoked token.
pub async fn verify_token(conn: &mut PgConnection, plaintext: &str) -> Result<Option<Principal>> {
    verify_token_with(conn, plaintext, true).await
}

/// `verify_token`; `touch` records `last_used_at`.
pub async fn verify_token_with(conn: &mut PgConnection, plaintext: &str, touch: bool) -> Result<Option<Principal>> {
    let Some(body) = plaintext.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let Some((key, secret)) = body.split_once('_') else {
        return Ok(None);
    };
    if key.chars().count() != 8 || secret.is_empty() {
        return Ok(None);
    }
    let row: Option<TokenRow> = sqlx::query_as(
        "select t.id, t.user_id, t.label, t.secret_sha256, t.scopes, t.profile_ids, t.default_profile, \
         t.identity_mode, t.lang, t.units, t.expires_at, t.revoked_at, u.household_id, u.disabled_at \
         from api_token t join app_user u on u.id = t.user_id where t.prefix = $1",
    )
    .bind(format!("{PREFIX}{key}"))
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if !bool::from(row.secret_sha256.ct_eq(&sha256(secret))) {
        return Ok(None);
    }
    if row.revoked_at.is_some() || row.expires_at <= Utc::now() || row.disabled_at.is_some() {
        return Ok(None);
    }
    if touch {
        sqlx::query("update api_token set last_used_at = now() where id = $1")
            .bind(row.id)
            .execute(conn)
            .await?;
    }
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

pub async fn revoke_token(conn: &mut PgConnection, user_id: Uuid, token_id: Uuid) -> Result<bool> {
    let n =
        sqlx::query("update api_token set revoked_at = now() where id = $1 and user_id = $2 and revoked_at is null")
            .bind(token_id)
            .bind(user_id)
            .execute(conn)
            .await?
            .rows_affected();
    Ok(n > 0)
}

/// One row of `list_tokens`, newest first.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TokenListed {
    pub id: Uuid,
    pub label: String,
    pub prefix: String,
    pub scopes: Vec<String>,
    pub profile_ids: Vec<Uuid>,
    pub default_profile: Option<Uuid>,
    pub identity_mode: String,
    pub lang: String,
    pub units: String,
    pub rate_per_min: i32,
    pub egress_facts_per_day: i32,
    pub egress_bytes_per_day: i64,
    pub genome_raw_lookups_per_day: i32,
    pub document_pages_per_day: i32,
    pub proposals_per_day: i32,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub async fn list_tokens(conn: &mut PgConnection, user_id: Uuid) -> Result<Vec<TokenListed>> {
    Ok(sqlx::query_as(
        "select id, label, prefix, scopes, profile_ids, default_profile, identity_mode, lang, units, \
         rate_per_min, egress_facts_per_day, egress_bytes_per_day, genome_raw_lookups_per_day, \
         document_pages_per_day, proposals_per_day, created_at, expires_at, last_used_at, revoked_at \
         from api_token where user_id = $1 order by created_at desc",
    )
    .bind(user_id)
    .fetch_all(conn)
    .await?)
}
