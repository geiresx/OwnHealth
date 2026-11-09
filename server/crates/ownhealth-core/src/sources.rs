//! The source registry and its scheduler (connectors.md 6.1, ingestion.md 3): one row per configured
//! inbox or device stream, each run recorded, and the health a person (or an alert) reads: when did it
//! last work, and what was the last error.
//!
//! Polled kinds register a [`Poller`]; the scheduler enqueues one `source.poll` job per due source on
//! the existing job queue (dedupe key `source.poll:<id>`), and the job records the run around the
//! poller. Push kinds (Paperless webhook, share target, Shortcut, Health Auto Export) call
//! [`begin_run`] and [`finish_run`] themselves, so every source reports health the same way.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::jobs::{self, EnqueueOptions, Job, JobContext, JobError};

/// The `source.kind` check constraint.
pub const SOURCE_KINDS: [&str; 8] = [
    "email",
    "folder",
    "paperless",
    "share",
    "shortcut",
    "health_auto_export",
    "gadgetbridge",
    "nightscout",
];
pub const POLL_JOB: &str = "source.poll";
/// How often the scheduler looks for due sources.
pub const SCHEDULER_SECONDS: u64 = 30;
/// A source with no success for this many poll intervals is stale in the health report.
pub const STALE_INTERVALS: i32 = 3;
pub const TOKEN_PREFIX: &str = "oh_ing_";

const COLS: &str = "id, household_id, profile_id, kind, label, config, secret_id, token_prefix, enabled, poll_seconds, \
    cursor, last_run_at, last_success_at, last_error_at, last_error, consecutive_failures, created_by, created_at";

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Source {
    pub id: Uuid,
    pub household_id: Uuid,
    pub profile_id: Option<Uuid>,
    pub kind: String,
    pub label: String,
    pub config: Value,
    pub secret_id: Option<Uuid>,
    pub token_prefix: Option<String>,
    pub enabled: bool,
    pub poll_seconds: Option<i32>,
    pub cursor: Option<Value>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub consecutive_failures: i32,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewSource<'a> {
    pub household_id: Uuid,
    pub profile_id: Option<Uuid>,
    pub kind: &'a str,
    pub label: &'a str,
    pub config: Value,
    pub secret_id: Option<Uuid>,
    pub poll_seconds: Option<i32>,
    pub created_by: Option<Uuid>,
}

impl<'a> NewSource<'a> {
    pub fn new(household_id: Uuid, kind: &'a str, label: &'a str) -> Self {
        NewSource {
            household_id,
            profile_id: None,
            kind,
            label,
            config: json!({}),
            secret_id: None,
            poll_seconds: None,
            created_by: None,
        }
    }
}

/// A change to a source; `None` leaves the field as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourcePatch {
    pub label: Option<String>,
    pub profile_id: Option<Option<Uuid>>,
    pub config: Option<Value>,
    pub secret_id: Option<Option<Uuid>>,
    pub enabled: Option<bool>,
    pub poll_seconds: Option<Option<i32>>,
}

fn check_kind(kind: &str) -> Result<()> {
    if SOURCE_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("unknown source kind {kind:?}")))
    }
}

pub async fn create_source(conn: &mut PgConnection, s: &NewSource<'_>) -> Result<Uuid> {
    check_kind(s.kind)?;
    if s.poll_seconds.is_some_and(|p| p < 60) {
        return Err(Error::Invalid("poll_seconds must be at least 60".into()));
    }
    Ok(sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label, config, secret_id, poll_seconds, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) returning id",
    )
    .bind(s.household_id)
    .bind(s.profile_id)
    .bind(s.kind)
    .bind(s.label)
    .bind(&s.config)
    .bind(s.secret_id)
    .bind(s.poll_seconds)
    .bind(s.created_by)
    .fetch_one(conn)
    .await?)
}

pub async fn get_source(conn: &mut PgConnection, id: Uuid) -> Result<Option<Source>> {
    let sql = format!("select {COLS} from source where id = $1");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(conn)
        .await?)
}

pub async fn list_sources(conn: &mut PgConnection, household_id: Uuid) -> Result<Vec<Source>> {
    let sql = format!("select {COLS} from source where household_id = $1 order by kind, label, created_at");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(household_id)
        .fetch_all(conn)
        .await?)
}

pub async fn update_source(conn: &mut PgConnection, id: Uuid, p: &SourcePatch) -> Result<Source> {
    if p.poll_seconds.is_some_and(|ps| ps.is_some_and(|s| s < 60)) {
        return Err(Error::Invalid("poll_seconds must be at least 60".into()));
    }
    let sql = format!(
        "update source set label = coalesce($2, label), \
           profile_id = case when $3 then $4 else profile_id end, \
           config = coalesce($5, config), \
           secret_id = case when $6 then $7 else secret_id end, \
           enabled = coalesce($8, enabled), \
           poll_seconds = case when $9 then $10 else poll_seconds end \
         where id = $1 returning {COLS}"
    );
    let row: Option<Source> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .bind(&p.label)
        .bind(p.profile_id.is_some())
        .bind(p.profile_id.flatten())
        .bind(&p.config)
        .bind(p.secret_id.is_some())
        .bind(p.secret_id.flatten())
        .bind(p.enabled)
        .bind(p.poll_seconds.is_some())
        .bind(p.poll_seconds.flatten())
        .fetch_optional(conn)
        .await?;
    row.ok_or_else(|| Error::NotFound("Unknown source.".into()))
}

pub async fn delete_source(conn: &mut PgConnection, id: Uuid) -> Result<bool> {
    let n = sqlx::query("delete from source where id = $1")
        .bind(id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n == 1)
}

/// Save the position a poller reached, in the caller's transaction (connectors.md 6.3 rule 2: in the
/// same transaction as the item it follows).
pub async fn save_cursor(conn: &mut PgConnection, id: Uuid, cursor: &Value) -> Result<()> {
    sqlx::query("update source set cursor = $2 where id = $1")
        .bind(id)
        .bind(cursor)
        .execute(conn)
        .await?;
    Ok(())
}

// --------------------------------------------------------- push tokens ----

fn random_id8() -> String {
    use rand::RngExt;
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| ALNUM[rng.random_range(0..ALNUM.len())] as char)
        .collect()
}

/// A new bearer token for a push source, `oh_ing_<id8>_<secret43>`, replacing any earlier one. The
/// plaintext is returned once; the row keeps the prefix and sha256 of the secret.
pub async fn issue_token(conn: &mut PgConnection, source_id: Uuid) -> Result<String> {
    use base64::Engine as _;
    let mut raw = [0u8; 32];
    rand::fill(&mut raw[..]);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let prefix = format!("{TOKEN_PREFIX}{}", random_id8());
    let n = sqlx::query("update source set token_prefix = $2, token_sha256 = $3 where id = $1")
        .bind(source_id)
        .bind(&prefix)
        .bind(Sha256::digest(secret.as_bytes()).to_vec())
        .execute(conn)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(Error::NotFound("Unknown source.".into()));
    }
    Ok(format!("{prefix}_{secret}"))
}

/// The enabled source a push token belongs to, or None. Constant-time on the secret.
pub async fn source_for_token(conn: &mut PgConnection, token: &str) -> Result<Option<Source>> {
    use subtle::ConstantTimeEq;
    let Some(rest) = token.strip_prefix(TOKEN_PREFIX) else {
        return Ok(None);
    };
    let Some((id8, secret)) = rest.split_once('_') else {
        return Ok(None);
    };
    if id8.len() != 8 || secret.is_empty() {
        return Ok(None);
    }
    let sql = format!("select {COLS}, token_sha256 from source where token_prefix = $1 and enabled");
    let row: Option<sqlx::postgres::PgRow> = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(format!("{TOKEN_PREFIX}{id8}"))
        .fetch_optional(&mut *conn)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let stored: Vec<u8> = sqlx::Row::try_get(&row, "token_sha256")?;
    let given = Sha256::digest(secret.as_bytes());
    if !bool::from(given.as_slice().ct_eq(&stored)) {
        return Ok(None);
    }
    Ok(Some(sqlx::FromRow::from_row(&row)?))
}

// ---------------------------------------------------------------- runs ----

/// One problem inside a run, without content: `{code, message}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunProblem {
    pub code: String,
    pub message: String,
}

/// What a run did. `cursor`, when set, is saved with the run's end.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunOutcome {
    pub seen: i32,
    pub new: i32,
    pub duplicate: i32,
    pub quarantined: i32,
    pub problems: Vec<RunProblem>,
    pub cursor: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct SourceRun {
    pub id: i64,
    pub source_id: Uuid,
    pub trigger: String,
    pub job_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub items_seen: i32,
    pub items_new: i32,
    pub items_duplicate: i32,
    pub items_quarantined: i32,
    pub errors: Value,
    pub error: Option<String>,
}

/// Open a run (`trigger`: poll | push | manual) and stamp `last_run_at`.
pub async fn begin_run(conn: &mut PgConnection, source_id: Uuid, trigger: &str, job_id: Option<i64>) -> Result<i64> {
    let id: i64 =
        sqlx::query_scalar("insert into source_run (source_id, trigger, job_id) values ($1, $2, $3) returning id")
            .bind(source_id)
            .bind(trigger)
            .bind(job_id)
            .fetch_one(&mut *conn)
            .await?;
    sqlx::query("update source set last_run_at = now() where id = $1")
        .bind(source_id)
        .execute(conn)
        .await?;
    Ok(id)
}

/// Close a run with what it did, or with the error that stopped it (type and message only, cut at
/// 500 characters). Updates the source's health columns. A run with problems where something still
/// arrived is `partial` and counts as a success for "last successful sync"; a run where every item it
/// saw failed is an `error` (nothing arrived, and the cursor stays so the items are retried).
pub async fn finish_run(
    conn: &mut PgConnection,
    run_id: i64,
    result: std::result::Result<&RunOutcome, &str>,
) -> Result<()> {
    let first_problem = |o: &RunOutcome| {
        o.problems.first().map(|p| {
            format!("{}: {}", p.code, p.message)
                .chars()
                .take(500)
                .collect::<String>()
        })
    };
    let all_failed = |o: &RunOutcome| o.seen > 0 && o.new + o.duplicate + o.quarantined == 0;
    let (status, outcome, error) = match result {
        Ok(o) if o.problems.is_empty() => ("ok", Some(o), None),
        Ok(o) if all_failed(o) => ("error", Some(o), first_problem(o)),
        Ok(o) => ("partial", Some(o), None),
        Err(e) => ("error", None, Some(e.chars().take(500).collect::<String>())),
    };
    let d = RunOutcome::default();
    let o = outcome.unwrap_or(&d);
    let source_id: Uuid = sqlx::query_scalar(
        "update source_run set finished_at = now(), status = $2, items_seen = $3, items_new = $4, \
         items_duplicate = $5, items_quarantined = $6, errors = $7, error = $8 where id = $1 returning source_id",
    )
    .bind(run_id)
    .bind(status)
    .bind(o.seen)
    .bind(o.new)
    .bind(o.duplicate)
    .bind(o.quarantined)
    .bind(serde_json::to_value(&o.problems)?)
    .bind(&error)
    .fetch_one(&mut *conn)
    .await?;
    if let Some(e) = &error {
        sqlx::query(
            "update source set last_error_at = now(), last_error = $2, consecutive_failures = consecutive_failures + 1 \
             where id = $1",
        )
        .bind(source_id)
        .bind(e)
        .execute(conn)
        .await?;
    } else {
        sqlx::query(
            "update source set last_success_at = now(), consecutive_failures = 0, cursor = coalesce($2, cursor) \
             where id = $1",
        )
        .bind(source_id)
        .bind(&o.cursor)
        .execute(conn)
        .await?;
    }
    Ok(())
}

pub async fn list_runs(conn: &mut PgConnection, source_id: Uuid, limit: i64) -> Result<Vec<SourceRun>> {
    Ok(sqlx::query_as(
        "select id, source_id, trigger, job_id, started_at, finished_at, status, items_seen, items_new, \
         items_duplicate, items_quarantined, errors, error from source_run where source_id = $1 \
         order by started_at desc, id desc limit $2",
    )
    .bind(source_id)
    .bind(limit.clamp(1, 500))
    .fetch_all(conn)
    .await?)
}

// -------------------------------------------------------------- health ----

/// What the health endpoint and the inbox screen show per source.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct SourceHealth {
    pub id: Uuid,
    pub kind: String,
    pub label: String,
    pub profile_id: Option<Uuid>,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub consecutive_failures: i32,
    /// Enabled, polled, and no success within STALE_INTERVALS poll intervals (or never).
    pub stale: bool,
    /// The newest finished run: `ok`, `partial` or `error`; none before the first run ends.
    pub last_run_status: Option<String>,
    /// How many problems that run had, and the first one as `code: message` (no content), or the
    /// error that stopped it.
    pub last_run_problems: i32,
    pub last_run_problem: Option<String>,
}

pub async fn source_health(conn: &mut PgConnection, household_id: Uuid) -> Result<Vec<SourceHealth>> {
    Ok(sqlx::query_as(
        "select s.id, s.kind, s.label, s.profile_id, s.enabled, s.last_run_at, s.last_success_at, s.last_error_at, \
         s.last_error, s.consecutive_failures, \
         (s.enabled and s.poll_seconds is not null and (s.last_success_at is null or \
           s.last_success_at < now() - make_interval(secs => s.poll_seconds * $2))) as stale, \
         r.status as last_run_status, coalesce(jsonb_array_length(r.errors), 0)::int4 as last_run_problems, \
         left(coalesce((r.errors -> 0 ->> 'code') || ': ' || (r.errors -> 0 ->> 'message'), r.error), 500) \
           as last_run_problem \
         from source s left join lateral (select status, errors, error from source_run \
           where source_id = s.id and finished_at is not null order by started_at desc, id desc limit 1) r on true \
         where s.household_id = $1 order by s.kind, s.label",
    )
    .bind(household_id)
    .bind(STALE_INTERVALS)
    .fetch_all(conn)
    .await?)
}

// ------------------------------------------------------ pollers and jobs ----

/// Reads one source: fetches what is new, stores the bytes, hands each item to
/// `arrive::arrive` (or its own importer), and says what it did. The job records the run around it.
pub type Poller = fn(JobContext, Source) -> BoxFuture<'static, std::result::Result<RunOutcome, JobError>>;

static POLLERS: LazyLock<RwLock<HashMap<String, Poller>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn register_poller(kind: &str, poller: Poller) {
    if let Ok(mut m) = POLLERS.write() {
        m.insert(kind.to_string(), poller);
    }
}

pub fn poller_for(kind: &str) -> Option<Poller> {
    POLLERS.read().ok().and_then(|m| m.get(kind).copied())
}

fn polled_kinds() -> Vec<String> {
    POLLERS.read().map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

/// The `source.poll` job: run the kind's poller between `begin_run` and `finish_run`. A failed poll is
/// recorded on the source and the job completes: the next schedule is the retry, so a source that is
/// down does not fill the queue with dead jobs.
async fn poll_job(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let source_id = job.uuid("source_id")?;
    let mut conn = ctx.pool.acquire().await?;
    let Some(source) = get_source(&mut conn, source_id).await? else {
        return Ok(Some(json!({"skipped": "unknown_source"})));
    };
    if !source.enabled {
        return Ok(Some(json!({"skipped": "disabled"})));
    }
    let poller = poller_for(&source.kind)
        .ok_or_else(|| JobError::permanent(format!("no poller for source kind {}", source.kind)))?;
    let run_id = begin_run(&mut conn, source_id, "poll", Some(job.id)).await?;
    drop(conn);
    let result = poller(ctx.clone(), source).await;
    let mut conn = ctx.pool.acquire().await?;
    match &result {
        Ok(o) => finish_run(&mut conn, run_id, Ok(o)).await?,
        Err(e) => finish_run(&mut conn, run_id, Err(&e.text())).await?,
    }
    Ok(Some(match result {
        Ok(o) => json!({"run_id": run_id, "status": "ok", "new": o.new, "duplicate": o.duplicate,
                        "quarantined": o.quarantined, "problems": o.problems.len()}),
        Err(_) => json!({"run_id": run_id, "status": "error"}),
    }))
}

pub fn register() {
    jobs::register_handler(POLL_JOB, |ctx, job| Box::pin(poll_job(ctx, job)));
}

/// Enqueue a `source.poll` for every enabled source whose kind has a poller and whose interval has
/// passed. Returns the jobs added.
pub async fn schedule_due(conn: &mut PgConnection) -> Result<Vec<i64>> {
    let kinds = polled_kinds();
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let due: Vec<Uuid> = sqlx::query_scalar(
        "select id from source where enabled and poll_seconds is not null and kind = any($1) \
         and (last_run_at is null or last_run_at <= now() - make_interval(secs => poll_seconds)) order by id",
    )
    .bind(&kinds)
    .fetch_all(&mut *conn)
    .await?;
    let mut added = Vec::new();
    for id in due {
        if let Some(j) = enqueue_poll(&mut *conn, id).await? {
            added.push(j);
        }
    }
    Ok(added)
}

/// Poll one source now (the "sync now" button, or a webhook that only says something changed).
pub async fn enqueue_poll(conn: &mut PgConnection, source_id: Uuid) -> Result<Option<i64>> {
    let key = format!("{POLL_JOB}:{source_id}");
    Ok(jobs::enqueue(
        conn,
        POLL_JOB,
        &json!({"source_id": source_id.to_string()}),
        EnqueueOptions::dedupe(&key),
    )
    .await?)
}

/// Every SCHEDULER_SECONDS: `schedule_due`. Stops when `stop` turns true.
pub async fn scheduler_loop(ctx: JobContext, mut stop: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(SCHEDULER_SECONDS));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.changed() => {}
        }
        if *stop.borrow() {
            return;
        }
        let r = async {
            let mut conn = ctx.pool.acquire().await?;
            schedule_due(&mut conn).await
        }
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "source scheduler failed");
        }
    }
}
