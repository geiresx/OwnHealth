//! Lane I: `api/imports.py`. Uploads (PDF, image, Apple Health export.zip, FHIR bundle, OwnGenome
//! output) and the import jobs they start.
//!
//! ```text
//! POST /api/imports   multipart: profile, files (one or more)
//! GET  /api/jobs      ?ids=1,2,3&limit=50   import jobs of the caller's profiles, newest first
//! ```
//!
//! Files are spooled to disk while the form is read, then land in the blob store once the caller may
//! import on the profile; the dispatcher creates documents and enqueues each lane's import job.

use std::path::PathBuf;

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Query, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use ownhealth_core::access::accessible_profiles;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::documents::{StoredFile, dispatch_upload};
use ownhealth_core::pyjson::row_to_json;
use ownhealth_core::{Error, Runtime, ids};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::Caller;
use crate::error::ApiError;
use crate::routes::auth::{Fail, R};
use crate::validate::{Errors, Params, param, pydantic_datetimes};

pub fn router() -> Router<Runtime> {
    Router::new()
        // uploads have no size cap, as in 0.1.1 (an Apple Health export runs to gigabytes)
        .route("/imports", post(upload).layer(DefaultBodyLimit::disable()))
        .route("/jobs", get(jobs))
}

/// A spooled upload part, removed from disk when dropped.
struct Spool {
    filename: String,
    path: PathBuf,
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn bad_body() -> Fail {
    Fail::Api(ApiError::new(
        StatusCode::BAD_REQUEST,
        "bad_request",
        "There was an error parsing the body",
    ))
}

/// Starlette's MultiPartParser limits in 0.1.1: a non-file part holds at most 1 MiB, and a form has
/// at most 1000 file parts and 1000 other parts. File parts have no size cap.
const MAX_PART: usize = 1024 * 1024;
const MAX_PARTS: usize = 1000;

/// A non-file part's bytes, refused past [`MAX_PART`].
async fn read_capped(field: &mut axum::extract::multipart::Field<'_>) -> R<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(c) = field.chunk().await.map_err(|_| bad_body())? {
        if buf.len() + c.len() > MAX_PART {
            return Err(bad_body());
        }
        buf.extend_from_slice(&c);
    }
    Ok(buf)
}

async fn spool(field: &mut axum::extract::multipart::Field<'_>, path: &std::path::Path) -> R<()> {
    let mut out = tokio::fs::File::create(path).await.map_err(Error::from)?;
    while let Some(chunk) = field.chunk().await.map_err(|_| bad_body())? {
        out.write_all(&chunk).await.map_err(Error::from)?;
    }
    out.flush().await.map_err(Error::from)?;
    Ok(())
}

async fn upload(State(rt): State<Runtime>, caller: Caller, req: Request) -> R<Json<Value>> {
    let principal = &caller.0;
    let mut profile: Option<String> = None;
    let mut files: Vec<Spool> = Vec::new();
    // a body that is not multipart has neither field, which FastAPI reports as two missing fields
    if let Ok(mut form) = Multipart::from_request(req, &rt).await {
        let (mut file_parts, mut field_parts) = (0usize, 0usize);
        while let Some(mut field) = form.next_field().await.map_err(|_| bad_body())? {
            let is_file = field.file_name().is_some();
            if is_file {
                file_parts += 1;
            } else {
                field_parts += 1;
            }
            if file_parts > MAX_PARTS || field_parts > MAX_PARTS {
                return Err(bad_body());
            }
            match field.name() {
                Some("profile") => {
                    let bytes = read_capped(&mut field).await?;
                    profile = Some(String::from_utf8(bytes).map_err(|_| bad_body())?);
                }
                Some("files") => {
                    let filename = field
                        .file_name()
                        .filter(|f| !f.is_empty())
                        .unwrap_or("upload")
                        .to_string();
                    let path = rt
                        .blobs
                        .root()
                        .join("tmp")
                        .join(format!("upload-{}", ids::token_hex(16)));
                    // the Spool owns the file before it exists, so a failed read still removes it
                    files.push(Spool {
                        filename,
                        path: path.clone(),
                    });
                    spool(&mut field, &path).await?;
                }
                _ if !is_file => {
                    read_capped(&mut field).await?;
                }
                _ => {}
            }
        }
    }
    let mut e = Errors::new();
    if profile.is_none() {
        e.missing(vec![json!("body"), json!("profile")], Value::Null);
    }
    if files.is_empty() {
        e.missing(vec![json!("body"), json!("files")], Value::Null);
    }
    e.done()?;
    let profile = profile.unwrap_or_default();

    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &profile, None, Some("import")).await?;
    let mut stored = Vec::with_capacity(files.len());
    for f in &files {
        let blobs = rt.blobs.clone();
        let path = f.path.clone();
        let (sha256, size) = tokio::task::spawn_blocking(move || blobs.put_file(&path))
            .await
            .map_err(|e| Error::Other(e.into()))?
            .map_err(|e| Error::Other(e.into()))?;
        stored.push(StoredFile {
            filename: f.filename.clone(),
            sha256,
            size: i64::try_from(size).unwrap_or(i64::MAX),
        });
    }
    drop(files);
    let result = match dispatch_upload(
        &mut tx,
        &rt.blobs,
        perm.profile.household_id,
        perm.profile.id,
        &stored,
        principal.user_id,
    )
    .await
    {
        Ok(r) => r,
        Err(Error::Upload(m)) => {
            return Err(Fail::Api(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_file",
                m,
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let shas: Vec<String> = result
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|i| i.get("sha256").and_then(Value::as_str).unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut audit = AuditEvent::new(principal.actor_kind(), "ui", "import");
    audit.actor_id = principal.user_id;
    audit.tool = Some("POST /api/imports".into());
    audit.profile_id = Some(perm.profile.id);
    audit.resource_type = Some("upload".into());
    audit.resource_ids = Some(shas);
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;
    Ok(Json(result))
}

/// Import jobs of the caller's profiles, newest first; `ids=1,2,3` to poll specific ones.
async fn jobs(State(rt): State<Runtime>, caller: Caller, Query(q): Query<Params>) -> R<Json<Value>> {
    let mut e = Errors::new();
    let limit = e.int("query", &q, "limit", 50, Some(1), Some(200));
    e.done()?;
    let ids: Option<Vec<i64>> = param(&q, "ids").filter(|s| !s.is_empty()).map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()))
            .filter_map(|i| i.parse().ok())
            .collect()
    });
    let mut tx = rt.pool.begin().await?;
    let pids: Vec<String> = accessible_profiles(&mut tx, &caller.0)
        .await?
        .iter()
        .map(|p| p.profile.id.to_string())
        .collect();
    let rows = sqlx::query(
        "select id, kind, state, attempts, max_attempts, last_error, result, created_at, finished_at, \
         payload->>'profile_id' as profile_id, payload->>'filename' as filename, \
         payload->>'document_id' as document_id from job \
         where payload->>'profile_id' = any($1) and ($2::bigint[] is null or id = any($2)) \
         order by id desc limit $3",
    )
    .bind(&pids)
    .bind(&ids)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut jobs = rows
        .iter()
        .map(|r| row_to_json(r).map(Value::Object))
        .collect::<Result<Vec<_>, _>>()?;
    for j in &mut jobs {
        pydantic_datetimes(j, &["created_at", "finished_at"]);
    }
    Ok(Json(json!({"jobs": jobs})))
}
