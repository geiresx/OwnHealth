//! Quarantine (ingestion.md 3.2, security-privacy.md 4.2 #7): mail from a sender not on the allowlist,
//! or that fails authentication, is kept as one blob (the whole message) and listed with sender,
//! subject and attachment names. It never becomes a document, a page, a candidate row or an
//! observation while it is held: [`hold`] writes only this table and the blob. A person releases it
//! (lane A parses it then, through `arrive::arrive`), or deletes it.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::blobs::sha256_hex_to_bytes;
use crate::error::{Error, Result};

/// The `quarantine_item.reason` check constraint.
pub const REASONS: [&str; 5] = [
    "sender_not_allowed",
    "auth_failed",
    "too_large",
    "unsupported_type",
    "no_profile",
];
pub const SUBJECT_MAX_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct QuarantineIn<'a> {
    pub household_id: Uuid,
    pub source_id: Option<Uuid>,
    /// The whole message, already in the blob store.
    pub raw_sha256: &'a str,
    pub message_id: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub subject: Option<&'a str>,
    /// `[{filename, mime, size}]`
    pub attachments: Value,
    pub reason: &'a str,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct QuarantineItem {
    pub id: Uuid,
    pub household_id: Uuid,
    pub source_id: Option<Uuid>,
    #[serde(skip)]
    pub raw_blob: Vec<u8>,
    pub message_id: Option<String>,
    pub sender: Option<String>,
    pub subject: Option<String>,
    pub attachments: Value,
    pub reason: String,
    pub status: String,
    pub received_at: DateTime<Utc>,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub released_document_ids: Option<Vec<Uuid>>,
}

impl QuarantineItem {
    pub fn raw_sha256_hex(&self) -> String {
        hex::encode(&self.raw_blob)
    }
}

const COLS: &str = "id, household_id, source_id, raw_blob, message_id, sender, subject, attachments, reason, status, \
    received_at, decided_by, decided_at, released_document_ids";

/// Control characters out, at most `max` characters: what we keep of untrusted header text.
pub fn clean_header(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Keep a message in quarantine. Nothing else is written: no document, no job.
pub async fn hold(conn: &mut PgConnection, q: &QuarantineIn<'_>) -> Result<Uuid> {
    if !REASONS.contains(&q.reason) {
        return Err(Error::Invalid(format!("unknown quarantine reason {:?}", q.reason)));
    }
    Ok(sqlx::query_scalar(
        "insert into quarantine_item (household_id, source_id, raw_blob, message_id, sender, subject, attachments, reason) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning id",
    )
    .bind(q.household_id)
    .bind(q.source_id)
    .bind(sha256_hex_to_bytes(q.raw_sha256)?)
    .bind(q.message_id.map(|m| clean_header(m, 998)))
    .bind(q.sender.map(|s| clean_header(s, 320)))
    .bind(q.subject.map(|s| clean_header(s, SUBJECT_MAX_CHARS)))
    .bind(&q.attachments)
    .bind(q.reason)
    .fetch_one(conn)
    .await?)
}

pub async fn get_item(conn: &mut PgConnection, id: Uuid) -> Result<Option<QuarantineItem>> {
    let sql = format!("select {COLS} from quarantine_item where id = $1");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(conn)
        .await?)
}

/// Newest first. `status` None lists every status.
pub async fn list_items(
    conn: &mut PgConnection,
    household_id: Uuid,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<QuarantineItem>> {
    let sql = format!(
        "select {COLS} from quarantine_item where household_id = $1 and ($2::text is null or status = $2) \
         order by received_at desc, id limit $3"
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(household_id)
        .bind(status)
        .bind(limit.clamp(1, 500))
        .fetch_all(conn)
        .await?)
}

/// Lock a held item for its release (`select ... for update`); `Error::Review` when it is not held.
pub async fn lock_held(conn: &mut PgConnection, id: Uuid) -> Result<QuarantineItem> {
    let sql = format!("select {COLS} from quarantine_item where id = $1 for update");
    let item: Option<QuarantineItem> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    let item = item.ok_or_else(|| Error::NotFound("Unknown quarantine item.".into()))?;
    if item.status != "held" {
        return Err(Error::Review(format!("This message was already {}.", item.status)));
    }
    Ok(item)
}

/// Record a release and the documents it produced. Only a held item can be released.
pub async fn mark_released(conn: &mut PgConnection, id: Uuid, user_id: Uuid, document_ids: &[Uuid]) -> Result<()> {
    let n = sqlx::query(
        "update quarantine_item set status = 'released', decided_by = $2, decided_at = now(), \
         released_document_ids = $3 where id = $1 and status = 'held'",
    )
    .bind(id)
    .bind(user_id)
    .bind(document_ids)
    .execute(conn)
    .await?
    .rows_affected();
    if n == 1 {
        Ok(())
    } else {
        Err(Error::Review("This message is not held.".into()))
    }
}

/// Delete a held message: the row stays as `deleted` (who and when); the blob is left to blob GC.
pub async fn mark_deleted(conn: &mut PgConnection, id: Uuid, user_id: Uuid) -> Result<()> {
    let n = sqlx::query(
        "update quarantine_item set status = 'deleted', decided_by = $2, decided_at = now() where id = $1 and status = 'held'",
    )
    .bind(id)
    .bind(user_id)
    .execute(conn)
    .await?
    .rows_affected();
    if n == 1 {
        Ok(())
    } else {
        Err(Error::Review("This message is not held.".into()))
    }
}
