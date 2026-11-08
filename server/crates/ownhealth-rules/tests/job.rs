//! Job `state.rebuild` (0.1.1 tests/state/test_job.py): rebuilds and stores the state; a second
//! run with the same data writes nothing.

mod common;

use std::sync::Arc;

use common::seeded;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::jobs::{Job, JobContext, handler_for};
use serde_json::{Value, json};
use uuid::Uuid;

async fn count(pool: &sqlx::PgPool, pid: Uuid) -> anyhow::Result<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from state_snapshot where profile_id = $1")
            .bind(pid)
            .fetch_one(pool)
            .await?,
    )
}

fn job(payload: Value) -> Job {
    Job {
        id: 0,
        queue: "core".into(),
        kind: "state.rebuild".into(),
        payload,
        attempts: 0,
        max_attempts: 3,
        dedupe_key: None,
    }
}

#[tokio::test]
async fn state_rebuild_job() -> anyhow::Result<()> {
    let s = seeded().await?;
    ownhealth_rules::register();
    let handler = handler_for("state.rebuild").ok_or_else(|| anyhow::anyhow!("no handler"))?;
    let dir = std::env::temp_dir().join(format!("oh-rules-job-{}", ownhealth_core::ids::token_hex(4)));
    let ctx = JobContext::new(
        s.pool().clone(),
        Arc::new(s.db.settings(&dir)?),
        BlobStore::new(dir.join("blobs"))?,
    );
    let pid = s.pid("lucia");
    let one = job(json!({"profile_id": pid.to_string(), "as_of": "2026-06-30"}));
    let before = count(s.pool(), pid).await?;
    let out = handler(ctx.clone(), one.clone()).await?.unwrap_or_default();
    let hash = out["hashes"][pid.to_string()].clone();
    assert!(
        out["profiles"] == 1 && hash.as_str().is_some_and(|h| h.starts_with("sha256:")),
        "{out}"
    );
    assert_eq!(count(s.pool(), pid).await?, before + 1);
    assert_eq!(handler(ctx.clone(), one).await?.unwrap_or_default(), out); // idempotent: same hash
    assert_eq!(count(s.pool(), pid).await?, before + 1);
    let every = handler(ctx.clone(), job(json!({"as_of": "2026-06-30"})))
        .await?
        .unwrap_or_default();
    assert_eq!(every["hashes"][pid.to_string()], hash);
    assert!(every["profiles"].as_i64().is_some_and(|n| n >= 4), "{every}");
    let bad = handler(ctx, job(json!({"as_of": "30/06/2026"}))).await;
    assert!(bad.is_err_and(|e| e.type_name == "PermanentJobError"));
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
