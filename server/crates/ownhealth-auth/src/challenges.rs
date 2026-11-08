//! Single-use, short-lived secrets in `auth_challenge` (`auth/challenges.py`): WebAuthn challenges,
//! OIDC state, invites.

use chrono::{Duration, Utc};
use serde_json::{Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{AuthError, AuthResult};

/// Five minutes.
pub const TTL_SECONDS: i64 = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct Challenge {
    pub id: Uuid,
    pub purpose: String,
    pub challenge: Vec<u8>,
    pub user_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub data: Map<String, Value>,
}

impl Challenge {
    /// `data[key]` as text, when it is a string.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }
}

type Row = (Uuid, String, Vec<u8>, Option<Uuid>, Option<Uuid>, Value);

fn from_row((id, purpose, challenge, user_id, session_id, data): Row) -> Challenge {
    let data = match data {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    Challenge {
        id,
        purpose,
        challenge,
        user_id,
        session_id,
        data,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Issue {
    pub user_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub data: Map<String, Value>,
    /// Seconds; 0 means `TTL_SECONDS`.
    pub ttl_seconds: i64,
}

pub async fn issue(conn: &mut PgConnection, purpose: &str, challenge: &[u8], opts: Issue) -> AuthResult<Uuid> {
    sqlx::query("delete from auth_challenge where expires_at < now() - interval '1 day'")
        .execute(&mut *conn)
        .await?;
    let ttl = if opts.ttl_seconds > 0 {
        opts.ttl_seconds
    } else {
        TTL_SECONDS
    };
    Ok(sqlx::query_scalar(
        "insert into auth_challenge (purpose, challenge, user_id, session_id, data, expires_at) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(purpose)
    .bind(challenge)
    .bind(opts.user_id)
    .bind(opts.session_id)
    .bind(Value::Object(opts.data))
    .bind(Utc::now() + Duration::seconds(ttl))
    .fetch_one(conn)
    .await?)
}

/// Mark the challenge used and return it. `challenge_invalid` when it cannot be used.
pub async fn consume(conn: &mut PgConnection, challenge: &[u8], purposes: &[&str]) -> AuthResult<Challenge> {
    let row: Option<Row> = sqlx::query_as(
        "update auth_challenge set used_at = now() where challenge = $1 and used_at is null \
         and expires_at > now() returning id, purpose, challenge, user_id, session_id, data",
    )
    .bind(challenge)
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else {
        return Err(AuthError::refused(
            "challenge_invalid",
            "This request expired or was already used. Start again.",
        ));
    };
    let ch = from_row(row);
    if !purposes.contains(&ch.purpose.as_str()) {
        return Err(AuthError::refused(
            "challenge_invalid",
            "This request belongs to another step. Start again.",
        ));
    }
    Ok(ch)
}

/// The live challenge without consuming it (to show an invite before it is redeemed).
pub async fn peek(conn: &mut PgConnection, challenge: &[u8], purposes: &[&str]) -> AuthResult<Option<Challenge>> {
    let purposes: Vec<&str> = purposes.to_vec();
    let row: Option<Row> = sqlx::query_as(
        "select id, purpose, challenge, user_id, session_id, data from auth_challenge where challenge = $1 \
         and used_at is null and expires_at > now() and purpose = any($2)",
    )
    .bind(challenge)
    .bind(&purposes)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(from_row))
}
