//! A structured-import job whose file is not stored fails for good (`tests/test_jobs.py`
//! `test_a_structured_import_with_no_stored_blob_fails_for_good`).

mod common;

use std::sync::Arc;

use common::{Env, R};
use ownhealth_core::jobs::{Job, JobContext};
use ownhealth_ingest_structured::jobs::import_fhir_job;
use serde_json::{Value, json};

fn job(payload: Value) -> Job {
    Job {
        id: 0,
        queue: "core".into(),
        kind: "import.fhir_bundle".into(),
        payload,
        attempts: 1,
        max_attempts: 5,
        dedupe_key: None,
    }
}

#[tokio::test]
async fn a_structured_import_with_no_stored_blob_fails_for_good() -> R {
    let env = Env::new().await?;
    let settings = Arc::new(env.db.settings(env.dir.path())?);
    let ctx = JobContext::new(env.db.pool.clone(), settings, env.blobs.clone());
    for payload in [
        json!({"household_id": "x", "profile_id": "y"}), // no blob_sha256
        json!({"blob_sha256": "ab".repeat(32), "household_id": "x", "profile_id": "y"}), // not stored
        json!({"blob_sha256": "not-hex", "household_id": "x", "profile_id": "y"}),
    ] {
        let Err(e) = import_fhir_job(ctx.clone(), job(payload.clone())).await else {
            panic!("{payload}: the job succeeded");
        };
        assert!(e.permanent, "{payload}: {e} is retryable");
        assert_eq!(e.type_name, "PermanentJobError", "{payload}");
    }
    Ok(())
}
