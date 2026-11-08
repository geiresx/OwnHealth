//! Jobs `import.fhir_bundle` and `import.apple_health` (`jobs/kinds/{fhir,apple_health}.py`): store
//! the uploaded file as a document, then import it, in one transaction.

use ownhealth_core::Error;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::{Job, JobContext, JobError};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::py;

fn uuid_field(payload: &Value, key: &str) -> Result<Uuid, JobError> {
    let v = payload
        .get(key)
        .ok_or_else(|| JobError::permanent(format!("bad payload: '{key}'")))?;
    Uuid::parse_str(&py::py_str(Some(v)))
        .map_err(|_| JobError::permanent("bad payload: badly formed hexadecimal UUID string"))
}

/// The document a structured-import job works on: given in the payload, or made from its blob. A
/// malformed payload or a blob that is not stored cannot heal on a retry: it fails for good.
pub async fn document_for(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    payload: &Value,
    mime: &str,
    channel: &str,
) -> Result<Uuid, JobError> {
    if py::truthy(payload.get("document_id")) {
        return uuid_field(payload, "document_id");
    }
    let sha = payload
        .get("blob_sha256")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let Some(sha) = sha.filter(|s| blobs.exists(s)) else {
        return Err(JobError::permanent(format!(
            "{}: the uploaded file is not in the blob store",
            py::py_str(payload.get("filename"))
        )));
    };
    let household_id = uuid_field(payload, "household_id")?;
    let profile_id = uuid_field(payload, "profile_id")?;
    let size = blobs.size(sha).map_err(|e| JobError::from(Error::from(e)))? as i64;
    let filename = payload.get("filename").and_then(Value::as_str);
    let doc = create_document(
        conn,
        &NewDocument {
            filename,
            channel,
            doc_type: Some("other"),
            ..NewDocument::upload(household_id, Some(profile_id), sha, mime, size)
        },
    )
    .await?;
    Ok(doc.id)
}

pub async fn import_fhir_job(ctx: JobContext, job: Job) -> Result<Option<Value>, JobError> {
    let mime = if job.payload.get("format").and_then(Value::as_str) == Some("ndjson") {
        "application/fhir+ndjson"
    } else {
        "application/fhir+json"
    };
    let mut tx = ctx.pool.begin().await?;
    let doc = document_for(&mut tx, &ctx.blobs, &job.payload, mime, "fhir").await?;
    let summary = crate::fhir::import_fhir(&mut tx, &ctx.blobs, doc)
        .await
        .map_err(|e| match e {
            // malformed JSON or resource: a retry cannot help
            Error::Invalid(m) => JobError::permanent(format!("not a readable FHIR file: {m}")),
            other => JobError::from(other),
        })?;
    tx.commit().await?;
    Ok(Some(summary.to_value()))
}

pub async fn import_apple_health_job(ctx: JobContext, job: Job) -> Result<Option<Value>, JobError> {
    let mut tx = ctx.pool.begin().await?;
    let doc = document_for(&mut tx, &ctx.blobs, &job.payload, "application/zip", "apple_health").await?;
    let summary = crate::apple_health::import_apple_health(&mut tx, &ctx.blobs, doc)
        .await
        .map_err(|e| match e {
            Error::Upload(m) => JobError::permanent(m),
            Error::Invalid(m) => JobError::permanent(format!("not a readable Apple Health export: {m}")),
            other => JobError::from(other),
        })?;
    tx.commit().await?;
    Ok(Some(summary.to_value()))
}
