//! Ported from tests/test_jobs.py (the queue itself; the lane handlers have their own tests).

mod common;

use std::time::Duration;

use common::*;
use ownhealth_core::jobs::{
    self, EnqueueOptions, Job, JobContext, JobError, claim, enqueue, register_handler, run_one,
};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use sqlx::{Acquire, PgPool};

fn handlers() {
    register_handler("test.ok", |_ctx, job| {
        Box::pin(async move { Ok(Some(json!({"seen": job.payload["n"]}))) })
    });
    register_handler("test.flaky", |_ctx, _job| {
        Box::pin(async { Err(JobError::typed("RuntimeError", "temporary")) })
    });
    register_handler("test.bad_input", |_ctx, _job| {
        Box::pin(async { Err(JobError::permanent("unreadable file")) })
    });
    register_handler("test.panics", |_ctx, _job| Box::pin(async { panic!("handler bug") }));
}

async fn setup() -> anyhow::Result<(TestDb, JobContext, tempfile::TempDir)> {
    handlers();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let rt = ownhealth_core::Runtime::with_pool(settings, db.pool.clone())?;
    let ctx = JobContext {
        worker_id: "test-worker".into(),
        ..rt.job_context()
    };
    Ok((db, ctx, dir))
}

async fn add(pool: &PgPool, kind: &str, payload: Value, opts: EnqueueOptions<'_>) -> anyhow::Result<Option<i64>> {
    let mut c = pool.acquire().await?;
    Ok(enqueue(&mut c, kind, &payload, opts).await?)
}

#[derive(Debug, sqlx::FromRow)]
struct State {
    state: String,
    attempts: i32,
    last_error: Option<String>,
    result: Option<Value>,
    later: bool,
}

async fn state(pool: &PgPool, id: i64) -> anyhow::Result<State> {
    Ok(
        sqlx::query_as("select state, attempts, last_error, result, run_after > now() as later from job where id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

fn only(kind: &str) -> Vec<String> {
    vec![kind.to_string()]
}

#[tokio::test]
async fn enqueue_dedupes_while_queued() -> R {
    let (db, _ctx, _dir) = setup().await?;
    let mut c = db.pool.acquire().await?;
    let mut tx = c.begin().await?;
    let a = enqueue(
        &mut tx,
        "test.ok",
        &json!({"n": 1}),
        EnqueueOptions::dedupe("state.rebuild:x"),
    )
    .await?;
    let b = enqueue(
        &mut tx,
        "test.ok",
        &json!({"n": 2}),
        EnqueueOptions::dedupe("state.rebuild:x"),
    )
    .await?;
    tx.commit().await?;
    assert!(a.is_some() && b.is_none());
    sqlx::query("update job set state = 'done' where id = $1")
        .bind(a)
        .execute(&mut *c)
        .await?;
    assert!(
        enqueue(
            &mut c,
            "test.ok",
            &json!({"n": 3}),
            EnqueueOptions::dedupe("state.rebuild:x")
        )
        .await?
        .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn run_success_and_retry_with_backoff() -> R {
    let (db, ctx, _dir) = setup().await?;
    let ok = add(&db.pool, "test.ok", json!({"n": 7}), EnqueueOptions::default())
        .await?
        .unwrap_or_default();
    let flaky = add(
        &db.pool,
        "test.flaky",
        json!({"n": 0}),
        EnqueueOptions {
            max_attempts: 2,
            ..Default::default()
        },
    )
    .await?
    .unwrap_or_default();
    assert_eq!(
        run_one(&ctx, "core", Some(&only("test.ok"))).await?.map(|j| j.id),
        Some(ok)
    );
    let s = state(&db.pool, ok).await?;
    assert!(s.state == "done" && s.result == Some(json!({"seen": 7})));
    assert_eq!(
        run_one(&ctx, "core", Some(&only("test.flaky"))).await?.map(|j| j.id),
        Some(flaky)
    );
    let s = state(&db.pool, flaky).await?;
    assert!(s.state == "queued" && s.attempts == 1 && s.later);
    assert_eq!(s.last_error.as_deref(), Some("RuntimeError: temporary"));
    assert!(
        run_one(&ctx, "core", Some(&only("test.flaky"))).await?.is_none(),
        "backoff: not ready yet"
    );
    Ok(())
}

#[tokio::test]
async fn a_dead_job_files_a_review_item_and_trusts_no_payload_id() -> R {
    let (db, ctx, _dir) = setup().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    drop(c);
    let pid = f.profile("alex").to_string();
    let cases = [
        (json!({"profile_id": pid}), Some(f.household_id), true),
        (
            json!({"profile_id": pid, "document_id": "00000000-0000-0000-0000-000000000000"}),
            Some(f.household_id),
            true,
        ),
        (
            json!({"profile_id": pid, "document_id": "not-a-uuid"}),
            Some(f.household_id),
            true,
        ),
        (json!({"profile_id": "not-a-uuid"}), None, false),
        (
            json!({"household_id": "00000000-0000-0000-0000-000000000000"}),
            None,
            false,
        ),
        (json!({"household_id": 7}), None, false),
    ];
    for (payload, household, has_profile) in cases {
        let jid = add(&db.pool, "test.bad_input", payload.clone(), EnqueueOptions::default())
            .await?
            .unwrap_or_default();
        run_one(&ctx, "core", Some(&only("test.bad_input"))).await?;
        let s = state(&db.pool, jid).await?;
        assert_eq!(s.state, "dead", "{payload}");
        assert_eq!(s.last_error.as_deref(), Some("PermanentJobError: unreadable file"));
        let item: (String, Option<uuid::Uuid>, Option<uuid::Uuid>, Option<uuid::Uuid>, Value) = sqlx::query_as(
            "select kind, household_id, profile_id, document_id, payload from review_item where payload->>'job_id' = $1",
        )
        .bind(jid.to_string())
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(item.0, "job_failed");
        assert_eq!(item.1, household, "{payload}");
        assert_eq!(item.2.is_some(), has_profile, "{payload}");
        assert_eq!(item.3, None);
        assert_eq!(
            item.4["payload_document_id"],
            payload.get("document_id").cloned().unwrap_or(Value::Null)
        );
        assert_eq!(item.4["error"], "PermanentJobError: unreadable file");
    }
    Ok(())
}

#[tokio::test]
async fn skip_locked_gives_each_worker_its_own_job() -> R {
    let (db, _ctx, _dir) = setup().await?;
    let mut ids = std::collections::BTreeSet::new();
    for i in 0..2 {
        ids.insert(
            add(&db.pool, "test.claim", json!({"n": i}), EnqueueOptions::default())
                .await?
                .unwrap_or_default(),
        );
    }
    let mut holder = db.pool.acquire().await?;
    let mut tx = holder.begin().await?;
    let first: i64 = sqlx::query_scalar(
        "select id from job where kind = 'test.claim' and state = 'queued' order by id for update skip locked limit 1",
    )
    .fetch_one(&mut *tx)
    .await?;
    // Must skip the locked row, not wait for it: a regression fails on the timeout instead of hanging.
    let other = tokio::time::timeout(
        Duration::from_secs(10),
        claim(&db.pool, "core", "w2", Some(&only("test.claim"))),
    )
    .await??;
    tx.rollback().await?;
    let other = other.map(|j| j.id).unwrap_or_default();
    assert_ne!(other, first);
    assert_eq!(
        [first, other].into_iter().collect::<std::collections::BTreeSet<_>>(),
        ids
    );
    Ok(())
}

#[tokio::test]
async fn unknown_kinds_and_panics_are_recorded_not_silent() -> R {
    let (db, ctx, _dir) = setup().await?;
    let jid = add(
        &db.pool,
        "test.nobody_handles_this",
        json!({}),
        EnqueueOptions::default(),
    )
    .await?
    .unwrap_or_default();
    run_one(&ctx, "core", Some(&only("test.nobody_handles_this"))).await?;
    let s = state(&db.pool, jid).await?;
    assert!(s.state == "dead" && s.last_error.is_some_and(|e| e.contains("no handler")));
    let jid = add(&db.pool, "test.panics", json!({}), EnqueueOptions::default())
        .await?
        .unwrap_or_default();
    run_one(&ctx, "core", Some(&only("test.panics"))).await?;
    let s = state(&db.pool, jid).await?;
    assert!(
        s.state == "queued"
            && s.last_error
                .is_some_and(|e| e.starts_with("RuntimeError: handler panicked"))
    );
    Ok(())
}

async fn expire(pool: &PgPool, id: i64) -> R {
    sqlx::query("update job set locked_until = now() - interval '1 second' where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn ready(pool: &PgPool, id: i64) -> R {
    sqlx::query("update job set run_after = now() where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn the_reaper_requeues_and_a_late_worker_cannot_finish() -> R {
    let (db, _ctx, _dir) = setup().await?;
    let jid = add(&db.pool, "test.lost_lease", json!({}), EnqueueOptions::default())
        .await?
        .unwrap_or_default();
    let first = claim(&db.pool, "core", "w0", Some(&only("test.lost_lease")))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no job"))?;
    expire(&db.pool, jid).await?;
    assert_eq!(jobs::reap_expired(&db.pool).await?, 1);
    assert_eq!(state(&db.pool, jid).await?.state, "queued");
    ready(&db.pool, jid).await?;
    let second = claim(&db.pool, "core", "w1", Some(&only("test.lost_lease")))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no job"))?;
    assert!(second.id == first.id && first.id == jid);
    assert!(!jobs::complete(&db.pool, &first, "w0", Some(&json!({"from": "w0"}))).await?);
    assert_eq!(
        jobs::fail(&db.pool, &first, &JobError::typed("RuntimeError", "late"), "w0").await?,
        "lost"
    );
    let (st, by): (String, Option<String>) = sqlx::query_as("select state, locked_by from job where id = $1")
        .bind(jid)
        .fetch_one(&db.pool)
        .await?;
    assert!(st == "running" && by.as_deref() == Some("w1"));
    assert!(jobs::complete(&db.pool, &second, "w1", Some(&json!({"from": "w1"}))).await?);
    assert_eq!(state(&db.pool, jid).await?.state, "done");
    Ok(())
}

#[tokio::test]
async fn a_job_that_keeps_killing_its_worker_ends_dead_with_one_item() -> R {
    let (db, _ctx, _dir) = setup().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    drop(c);
    let jid = add(
        &db.pool,
        "test.crashloop",
        json!({"profile_id": f.profile("alex").to_string()}),
        EnqueueOptions {
            max_attempts: 3,
            ..Default::default()
        },
    )
    .await?
    .unwrap_or_default();
    for i in 0..6 {
        ready(&db.pool, jid).await?;
        if claim(&db.pool, "core", &format!("w{i}"), Some(&only("test.crashloop")))
            .await?
            .is_none()
        {
            break;
        }
        expire(&db.pool, jid).await?; // the worker was OOM-killed: nobody calls fail()
        jobs::reap_expired(&db.pool).await?;
    }
    let s = state(&db.pool, jid).await?;
    assert!(s.state == "dead" && s.attempts == 3);
    assert_eq!(
        s.last_error.as_deref(),
        Some("JobError: lease expired: the worker died mid-job")
    );
    let n: i64 =
        sqlx::query_scalar("select count(*) from review_item where kind = 'job_failed' and payload->>'job_id' = $1")
            .bind(jid.to_string())
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(n, 1);
    Ok(())
}

#[tokio::test]
async fn one_failing_item_does_not_stall_the_reaper() -> R {
    let (db, _ctx, _dir) = setup().await?;
    let opts = || EnqueueOptions {
        max_attempts: 1,
        ..Default::default()
    };
    let a = add(&db.pool, "test.crashloop", json!({"n": "a"}), opts())
        .await?
        .unwrap_or_default();
    let b = add(&db.pool, "test.crashloop", json!({"n": "b"}), opts())
        .await?
        .unwrap_or_default();
    for jid in [a, b] {
        ready(&db.pool, jid).await?;
        claim(&db.pool, "core", "w", Some(&only("test.crashloop"))).await?;
        expire(&db.pool, jid).await?;
    }
    // a database error inside the insert path of a's item (Python monkeypatches create_item)
    let fail_a = format!(
        "create function fail_a() returns trigger language plpgsql as $$ begin \
         if new.payload->>'job_id' = '{a}' then perform 1/0; end if; return new; end $$"
    );
    sqlx::query(sqlx::AssertSqlSafe(fail_a)).execute(&db.pool).await?;
    sqlx::query("create trigger fail_a before insert on review_item for each row execute function fail_a()")
        .execute(&db.pool)
        .await?;
    jobs::reap_expired(&db.pool).await?;
    assert_eq!(
        (state(&db.pool, a).await?.state, state(&db.pool, b).await?.state),
        ("dead".into(), "dead".into())
    );
    let items: Vec<String> = sqlx::query_scalar("select payload->>'job_id' from review_item where kind = 'job_failed'")
        .fetch_all(&db.pool)
        .await?;
    assert_eq!(items, [b.to_string()]);
    Ok(())
}

#[tokio::test]
async fn heartbeat_renews_the_lease() -> R {
    let (db, ctx, _dir) = setup().await?;
    let jid = add(&db.pool, "test.hb", json!({}), EnqueueOptions::default())
        .await?
        .unwrap_or_default();
    let job: Job = claim(&db.pool, "core", &ctx.worker_id, Some(&only("test.hb")))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no job"))?;
    sqlx::query("update job set locked_until = now() + interval '1 second' where id = $1")
        .bind(jid)
        .execute(&db.pool)
        .await?;
    ctx.heartbeat(&job).await?;
    let left: f64 =
        sqlx::query_scalar("select extract(epoch from locked_until - now())::float8 from job where id = $1")
            .bind(jid)
            .fetch_one(&db.pool)
            .await?;
    assert!(left > 290.0, "{left}");
    Ok(())
}
