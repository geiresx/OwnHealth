//! Postgres table job queue, run inside the app process (`jobs/__init__.py`).
//!
//! Workers claim one job with `SELECT ... FOR UPDATE SKIP LOCKED`, run its handler, and mark it
//! done. A failure retries with backoff (2^attempts * 30 s); after max_attempts the job is `dead`
//! and a `job_failed` review item appears. Handlers must be idempotent: a job may run twice after
//! a crash.
//!
//! Lanes register their kinds with `register_handler` from their crate's `register()`:
//!
//! ```ignore
//! jobs::register_handler("import.pdf", |ctx, job| Box::pin(import_pdf(ctx, job)));
//! ```

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::{Acquire, PgConnection, PgPool};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::blobs::BlobStore;
use crate::config::Settings;
use crate::review::{ReviewItemIn, create_item};

pub const LEASE_SECONDS: f64 = 300.0;
/// `run_one` renews the lease this often while a handler runs.
pub const HEARTBEAT_SECONDS: u64 = 60;
pub const BACKOFF_BASE_SECONDS: f64 = 30.0;
const REAP_EVERY_SECONDS: u64 = 60;

/// The job kinds the Python backend registers; the Rust lanes register the same names.
pub const JOB_KINDS: [&str; 6] = [
    "import.pdf",
    "import.apple_health",
    "import.fhir_bundle",
    "import.genome_facts",
    "normalize.rederive",
    "state.rebuild",
];

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub queue: String,
    pub kind: String,
    pub payload: Value,
    pub attempts: i32,
    pub max_attempts: i32,
    pub dedupe_key: Option<String>,
}

impl Job {
    /// A payload field as text (`payload["document_id"]` and friends).
    pub fn str(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(Value::as_str)
    }

    /// A payload field as a uuid; a `PermanentJobError` when missing or malformed.
    pub fn uuid(&self, key: &str) -> Result<Uuid, JobError> {
        self.str(key)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| JobError::permanent(format!("payload has no valid {key}")))
    }
}

#[derive(Debug, Clone)]
pub struct JobContext {
    pub pool: PgPool,
    pub settings: Arc<Settings>,
    pub blobs: BlobStore,
    pub worker_id: String,
}

impl JobContext {
    pub fn new(pool: PgPool, settings: Arc<Settings>, blobs: BlobStore) -> JobContext {
        let host = hostname();
        JobContext {
            pool,
            settings,
            blobs,
            worker_id: format!("{host}:{}", std::process::id()),
        }
    }

    /// Long handlers may call this to keep their lease; `run_one` already does it every minute.
    pub async fn heartbeat(&self, job: &Job) -> sqlx::Result<()> {
        sqlx::query("update job set locked_until = now() + make_interval(secs => $1) where id = $2 and locked_by = $3")
            .bind(LEASE_SECONDS)
            .bind(job.id)
            .bind(&self.worker_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// A handler failure. `type_name` is what `last_error` starts with, matching the Python class
/// names (`JobError`, `PermanentJobError`, or the exception type a handler hit, e.g. `ValueError`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{type_name}: {message}")]
pub struct JobError {
    pub type_name: String,
    pub message: String,
    /// A retry cannot help (bad input): the job goes straight to dead.
    pub permanent: bool,
}

impl JobError {
    pub fn retry(message: impl Into<String>) -> JobError {
        JobError {
            type_name: "JobError".into(),
            message: message.into(),
            permanent: false,
        }
    }

    pub fn permanent(message: impl Into<String>) -> JobError {
        JobError {
            type_name: "PermanentJobError".into(),
            message: message.into(),
            permanent: true,
        }
    }

    /// A retryable error with a chosen type name.
    pub fn typed(type_name: &str, message: impl Into<String>) -> JobError {
        JobError {
            type_name: type_name.into(),
            message: message.into(),
            permanent: false,
        }
    }

    /// The `last_error` text: `"<Type>: <message>"`, at most 500 characters.
    pub fn text(&self) -> String {
        self.to_string().chars().take(500).collect()
    }
}

impl From<crate::Error> for JobError {
    fn from(e: crate::Error) -> Self {
        use crate::Error as E;
        let type_name = match &e {
            E::AccessDenied { .. } => "AccessDenied",
            E::Review(_) => "ReviewError",
            E::Lifecycle(_) => "LifecycleError",
            E::Provenance(_) => "ProvenanceError",
            E::Upload(_) => "UploadError",
            E::Invalid(_) => "ValueError",
            E::NotFound(_) => "LookupError",
            E::Db(_) => "DBAPIError",
            E::Io(_) => "OSError",
            E::Json(_) => "JSONDecodeError",
            E::Other(_) => "RuntimeError",
        };
        JobError::typed(type_name, e.to_string())
    }
}

impl From<sqlx::Error> for JobError {
    fn from(e: sqlx::Error) -> Self {
        crate::Error::Db(e).into()
    }
}

pub type HandlerResult = Result<Option<Value>, JobError>;
pub type Handler = fn(JobContext, Job) -> BoxFuture<'static, HandlerResult>;

static HANDLERS: LazyLock<RwLock<HashMap<String, Handler>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Register the handler of one job kind. A second, different handler for the same kind is a bug.
pub fn register_handler(kind: &str, handler: Handler) {
    if let Ok(mut m) = HANDLERS.write()
        && let Some(prev) = m.insert(kind.to_string(), handler)
        && !std::ptr::fn_addr_eq(prev, handler)
    {
        tracing::error!(kind, "two handlers registered for one job kind; the last one wins");
    }
}

pub fn handler_for(kind: &str) -> Option<Handler> {
    HANDLERS.read().ok().and_then(|m| m.get(kind).copied())
}

pub fn registered_kinds() -> Vec<String> {
    let mut v: Vec<String> = HANDLERS.read().map(|m| m.keys().cloned().collect()).unwrap_or_default();
    v.sort();
    v
}

// --------------------------------------------------------------------- queue ----

#[derive(Debug, Clone)]
pub struct EnqueueOptions<'a> {
    pub queue: &'a str,
    pub dedupe_key: Option<&'a str>,
    pub priority: i16,
    pub run_after: Option<DateTime<Utc>>,
    pub max_attempts: i32,
}

impl Default for EnqueueOptions<'_> {
    fn default() -> Self {
        EnqueueOptions {
            queue: "core",
            dedupe_key: None,
            priority: 5,
            run_after: None,
            max_attempts: 5,
        }
    }
}

impl<'a> EnqueueOptions<'a> {
    pub fn dedupe(key: &'a str) -> Self {
        EnqueueOptions {
            dedupe_key: Some(key),
            ..Default::default()
        }
    }
}

/// Add a job in the caller's transaction. Returns its id, or None when a queued or running job
/// with the same dedupe_key exists already. Payloads carry ids only, never document content.
pub async fn enqueue(
    conn: &mut PgConnection,
    kind: &str,
    payload: &Value,
    opts: EnqueueOptions<'_>,
) -> sqlx::Result<Option<i64>> {
    let id: Option<i64> = sqlx::query_scalar(
        "insert into job (queue, kind, payload, dedupe_key, priority, run_after, max_attempts) \
         values ($1, $2, $3, $4, $5, coalesce($6, now()), $7) \
         on conflict (dedupe_key) where state in ('queued', 'running') do nothing returning id",
    )
    .bind(opts.queue)
    .bind(kind)
    .bind(payload)
    .bind(opts.dedupe_key)
    .bind(opts.priority)
    .bind(opts.run_after)
    .bind(opts.max_attempts)
    .fetch_optional(&mut *conn)
    .await?;
    if id.is_some() {
        sqlx::query("select pg_notify('ownhealth_job', $1)")
            .bind(opts.queue)
            .execute(conn)
            .await?;
    }
    Ok(id)
}

/// Claim the next ready job on `queue` (optionally only these kinds) with SKIP LOCKED.
pub async fn claim(pool: &PgPool, queue: &str, worker_id: &str, kinds: Option<&[String]>) -> sqlx::Result<Option<Job>> {
    sqlx::query_as(
        "with next as (select id from job where queue = $1 and state = 'queued' and run_after <= now() \
         and ($4::text[] is null or kind = any($4::text[])) order by priority, id for update skip locked limit 1) \
         update job set state = 'running', attempts = attempts + 1, locked_by = $2, \
         locked_until = now() + make_interval(secs => $3) from next where job.id = next.id \
         returning job.id, job.queue, job.kind, job.payload, job.attempts, job.max_attempts, job.dedupe_key",
    )
    .bind(queue)
    .bind(worker_id)
    .bind(LEASE_SECONDS)
    .bind(kinds)
    .fetch_optional(pool)
    .await
}

/// Mark the job done, only while `worker_id` still holds its lease. False when the lease was lost.
pub async fn complete(pool: &PgPool, job: &Job, worker_id: &str, result: Option<&Value>) -> sqlx::Result<bool> {
    let n = sqlx::query(
        "update job set state = 'done', finished_at = now(), locked_by = null, locked_until = null, \
         result = $3, last_error = null where id = $1 and locked_by = $2 and state = 'running'",
    )
    .bind(job.id)
    .bind(worker_id)
    .bind(result)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        tracing::warn!(job = job.id, kind = %job.kind, "lease lost before it finished, result dropped");
    }
    Ok(n > 0)
}

/// Record a failure while `worker_id` holds the lease. Returns the new state: `queued` (retry
/// later), `dead`, or `lost` when another worker or the reaper has the job now.
pub async fn fail(pool: &PgPool, job: &Job, err: &JobError, worker_id: &str) -> sqlx::Result<&'static str> {
    let mut tx = pool.begin().await?;
    let state = record_failure(&mut tx, job, err, Guard::Worker(worker_id)).await?;
    tx.commit().await?;
    Ok(state)
}

enum Guard<'a> {
    Worker(&'a str),
    Expired,
}

async fn record_failure(
    conn: &mut PgConnection,
    job: &Job,
    err: &JobError,
    guard: Guard<'_>,
) -> sqlx::Result<&'static str> {
    let dead = job.attempts >= job.max_attempts || err.permanent;
    let (guard_sql, worker) = match guard {
        Guard::Worker(w) => ("locked_by = $3", Some(w)),
        Guard::Expired => ("locked_until < now() and $3::text is null", None),
    };
    let text = err.text();
    if !dead {
        let backoff = 2f64.powi(job.attempts) * BACKOFF_BASE_SECONDS;
        let n = sqlx::query(sqlx::AssertSqlSafe(format!(
            "update job set state = 'queued', locked_by = null, locked_until = null, last_error = $2, \
             run_after = now() + make_interval(secs => $4) where id = $1 and state = 'running' and {guard_sql}"
        )))
        .bind(job.id)
        .bind(&text)
        .bind(worker)
        .bind(backoff)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        return Ok(if n > 0 { "queued" } else { "lost" });
    }
    let n = sqlx::query(sqlx::AssertSqlSafe(format!(
        "update job set state = 'dead', finished_at = now(), locked_by = null, locked_until = null, \
         last_error = $2 where id = $1 and state = 'running' and {guard_sql}"
    )))
    .bind(job.id)
    .bind(&text)
    .bind(worker)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n == 0 {
        return Ok("lost");
    }
    job_failed_item(conn, job, &text).await;
    Ok("dead")
}

/// `value` when it is a uuid of a row in `table`, else None: a payload is not trusted for an FK.
async fn existing_id(conn: &mut PgConnection, table: &str, value: Option<&Value>) -> sqlx::Result<Option<Uuid>> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let Ok(uid) = Uuid::parse_str(&text) else {
        return Ok(None);
    };
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select id from {table} where id = $1")))
        .bind(uid)
        .fetch_optional(conn)
        .await
}

async fn job_failed_item(conn: &mut PgConnection, job: &Job, error: &str) {
    // In a savepoint: the job is dead either way, and in reap_expired one bad insert must not roll
    // back the whole sweep and stall every other expired job.
    let filed: crate::Result<()> = async {
        let mut sp = conn.begin().await?;
        let p = &job.payload;
        let profile_id = existing_id(&mut sp, "profile", p.get("profile_id")).await?;
        let mut household_id = existing_id(&mut sp, "household", p.get("household_id")).await?;
        let document_id = existing_id(&mut sp, "document", p.get("document_id")).await?;
        if household_id.is_none()
            && let Some(pid) = profile_id
        {
            household_id = sqlx::query_scalar("select household_id from profile where id = $1")
                .bind(pid)
                .fetch_optional(&mut *sp)
                .await?;
        }
        if household_id.is_none()
            && let Some(did) = document_id
        {
            household_id = sqlx::query_scalar("select household_id from document where id = $1")
                .bind(did)
                .fetch_optional(&mut *sp)
                .await?;
        }
        let item = ReviewItemIn {
            kind: "job_failed".into(),
            household_id,
            profile_id,
            document_id,
            payload: json!({
                "job_id": job.id,
                "kind": job.kind,
                "attempts": job.attempts,
                "error": error,
                "payload_document_id": p.get("document_id").cloned().unwrap_or(Value::Null),
            }),
            ..ReviewItemIn::default()
        };
        create_item(&mut sp, &item).await?;
        sp.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = filed {
        tracing::error!(job = job.id, error = %e, "could not file its job_failed item");
    }
}

/// Running jobs whose lease ran out (the worker died, e.g. OOM) count as a failed attempt, so a
/// job that keeps killing its worker ends dead with a job_failed item instead of looping forever.
pub async fn reap_expired(pool: &PgPool) -> sqlx::Result<usize> {
    let mut tx = pool.begin().await?;
    let rows: Vec<Job> = sqlx::query_as(
        "select id, queue, kind, payload, attempts, max_attempts, dedupe_key from job \
         where state = 'running' and locked_until < now() for update skip locked",
    )
    .fetch_all(&mut *tx)
    .await?;
    let err = JobError::retry("lease expired: the worker died mid-job");
    for job in &rows {
        record_failure(&mut tx, job, &err, Guard::Expired).await?;
    }
    tx.commit().await?;
    Ok(rows.len())
}

/// Claim and run one job. Returns the job it ran, or None when the queue was empty.
pub async fn run_one(ctx: &JobContext, queue: &str, kinds: Option<&[String]>) -> sqlx::Result<Option<Job>> {
    let Some(job) = claim(&ctx.pool, queue, &ctx.worker_id, kinds).await? else {
        return Ok(None);
    };
    let lease = {
        let (ctx, job) = (ctx.clone(), job.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECONDS));
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = ctx.heartbeat(&job).await {
                    tracing::error!(job = job.id, error = %e, "heartbeat failed");
                }
            }
        })
    };
    let outcome = match handler_for(&job.kind) {
        None => Err(JobError::permanent(format!("no handler for job kind {}", job.kind))),
        // A panic in a handler is a failed attempt, never a dead worker.
        Some(h) => match tokio::spawn(h(ctx.clone(), job.clone())).await {
            Ok(r) => r,
            Err(e) => Err(JobError::typed("RuntimeError", format!("handler panicked: {e}"))),
        },
    };
    lease.abort();
    match outcome {
        Ok(result) => {
            complete(&ctx.pool, &job, &ctx.worker_id, result.as_ref()).await?;
        }
        Err(err) => {
            let state = fail(&ctx.pool, &job, &err, &ctx.worker_id).await?;
            tracing::warn!(job = job.id, kind = %job.kind, state, error = %err.text(), "job failed");
        }
    }
    Ok(Some(job))
}

/// Run jobs until the queue has none ready. For tests and the CLI.
pub async fn drain(ctx: &JobContext, queue: &str, limit: usize, kinds: Option<&[String]>) -> sqlx::Result<usize> {
    let mut n = 0;
    while n < limit && run_one(ctx, queue, kinds).await?.is_some() {
        n += 1;
    }
    Ok(n)
}

/// One worker: reap expired leases every minute, run jobs, sleep `poll_seconds` when idle.
pub async fn worker_loop(ctx: JobContext, queue: String, mut stop: watch::Receiver<bool>, poll_seconds: f64) {
    let mut last_reap: Option<tokio::time::Instant> = None;
    let poll = Duration::from_secs_f64(poll_seconds.max(0.05));
    while !*stop.borrow() {
        if last_reap.is_none_or(|t| t.elapsed() > Duration::from_secs(REAP_EVERY_SECONDS)) {
            if let Err(e) = reap_expired(&ctx.pool).await {
                tracing::error!(error = %e, "job reaper error");
            }
            last_reap = Some(tokio::time::Instant::now());
        }
        let ran = match run_one(&ctx, &queue, None).await {
            Ok(r) => r.is_some(),
            Err(e) => {
                tracing::error!(error = %e, "job loop error");
                false
            }
        };
        if !ran {
            tokio::select! {
                _ = stop.changed() => {}
                _ = tokio::time::sleep(poll) => {}
            }
        }
    }
}

/// `job_workers` workers per configured queue. Send `true` on the watch channel to stop them.
pub fn start_workers(ctx: &JobContext, stop: watch::Receiver<bool>) -> Vec<JoinHandle<()>> {
    let mut tasks = Vec::new();
    for queue in &ctx.settings.queues {
        for i in 0..ctx.settings.job_workers {
            let worker = JobContext {
                worker_id: format!("{}:{queue}:{i}", ctx.worker_id),
                ..ctx.clone()
            };
            tasks.push(tokio::spawn(worker_loop(
                worker,
                queue.clone(),
                stop.clone(),
                ctx.settings.job_poll_seconds,
            )));
        }
    }
    tasks
}
