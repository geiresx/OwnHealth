//! Lane B (v0.2): the push inboxes over REST (`ownhealth_inbox::{paperless, share}`).
//!
//! | Route | Auth | What |
//! |---|---|---|
//! | `POST /ingest/paperless` | `X-OwnHealth-Secret: oh_ing_...` of a `paperless` source | `{"doc_url": ...}` -> `{"outcome": ...}` |
//! | `POST /ingest/share` | session (the PWA share target, Android) | multipart `files`, `profile` (default `me`) -> `{"items": [...]}` |
//! | `POST /ingest/shortcut` | `Authorization: Bearer oh_ing_...` of a `shortcut` source | multipart `file` or `files` (the iOS Shortcut) -> `{"items": [...]}` |
//!
//! ingestion.md 3 names `/api/ingest/upload` for the Shortcut; that route takes sessions only, so the
//! token-bound upload has its own path (RUST-CONTRACTS.md section 10). A push token works only on the
//! route of its source's kind (403 `wrong_source` elsewhere). The share target is opened by the phone
//! as a page: a request with `Sec-Fetch-Mode: navigate` is answered with a redirect to the review queue
//! instead of JSON.

use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use ownhealth_core::arrive::Arrived;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::documents::StoredFile;
use ownhealth_core::sources::{RunOutcome, RunProblem, Source, begin_run, finish_run};
use ownhealth_core::{Error, Runtime, ids};
use ownhealth_inbox::{paperless, share};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::error::ApiError;
use crate::routes::auth::{Fail, R};
use crate::{Caller, SourceCaller};

/// A phone push holds at most this much (photos and lab PDFs, not Apple Health exports).
const MAX_BODY: usize = 100 * 1024 * 1024;
/// At most this many files per push, and 1 MiB per text field.
const MAX_FILES: usize = 20;
const MAX_FIELD: usize = 1024 * 1024;

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/ingest/paperless", post(paperless_webhook))
        .route(
            "/ingest/share",
            post(share_target).layer(DefaultBodyLimit::max(MAX_BODY)),
        )
        .route(
            "/ingest/shortcut",
            post(shortcut).layer(DefaultBodyLimit::max(MAX_BODY)),
        )
}

fn wrong_source(kind: &str) -> Fail {
    Fail::Api(ApiError::new(
        StatusCode::FORBIDDEN,
        "wrong_source",
        format!("This token belongs to a {kind} source, not to this inbox."),
    ))
}

fn bad_body(message: &str) -> Fail {
    Fail::Api(ApiError::new(StatusCode::BAD_REQUEST, "bad_request", message))
}

// ------------------------------------------------------------ Paperless ----

async fn paperless_webhook(
    State(rt): State<Runtime>,
    SourceCaller(source): SourceCaller,
    body: Bytes,
) -> R<Json<Value>> {
    if source.kind != paperless::CHANNEL {
        return Err(wrong_source(&source.kind));
    }
    let body: Value = serde_json::from_slice(&body).map_err(|_| bad_body("The body is not JSON."))?;
    let arrived = paperless::handle_webhook(&rt, &source, &body).await?;
    Ok(Json(serde_json::to_value(arrived).map_err(Error::from)?))
}

// ------------------------------------------------------- phone uploads ----

/// A spooled file part, removed from disk when dropped.
struct Spool {
    filename: String,
    path: PathBuf,
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Form {
    profile: Option<String>,
    files: Vec<Spool>,
}

async fn read_form(rt: &Runtime, req: Request, file_fields: &[&str]) -> R<Form> {
    let mut form = Multipart::from_request(req, rt)
        .await
        .map_err(|_| bad_body("Send the files as multipart/form-data."))?;
    let mut out = Form {
        profile: None,
        files: Vec::new(),
    };
    while let Some(mut field) = form.next_field().await.map_err(|_| bad_body("The form is broken."))? {
        let name = field.name().unwrap_or_default().to_string();
        if file_fields.contains(&name.as_str()) && field.file_name().is_some() {
            if out.files.len() >= MAX_FILES {
                return Err(bad_body("Too many files in one push."));
            }
            let filename = field
                .file_name()
                .filter(|f| !f.is_empty())
                .unwrap_or("shared")
                .to_string();
            let path = rt.blobs.root().join("tmp").join(format!("push-{}", ids::token_hex(16)));
            out.files.push(Spool {
                filename,
                path: path.clone(),
            });
            let mut f = tokio::fs::File::create(&path).await.map_err(Error::from)?;
            while let Some(chunk) = field.chunk().await.map_err(|_| bad_body("The form is broken."))? {
                f.write_all(&chunk).await.map_err(Error::from)?;
            }
            f.flush().await.map_err(Error::from)?;
        } else {
            let mut buf = Vec::new();
            while let Some(c) = field.chunk().await.map_err(|_| bad_body("The form is broken."))? {
                if buf.len() + c.len() > MAX_FIELD {
                    return Err(bad_body("A form field is too large."));
                }
                buf.extend_from_slice(&c);
            }
            if name == "profile" {
                out.profile = Some(String::from_utf8(buf).map_err(|_| bad_body("The profile is not text."))?);
            }
        }
    }
    if out.files.is_empty() {
        return Err(Fail::Api(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no_files",
            "The push holds no file.",
        )));
    }
    Ok(out)
}

/// Put the spooled files in the blob store (off the async threads).
async fn store(rt: &Runtime, files: &[Spool]) -> R<Vec<StoredFile>> {
    let mut stored = Vec::with_capacity(files.len());
    for f in files {
        let (blobs, path) = (rt.blobs.clone(), f.path.clone());
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
    Ok(stored)
}

fn items(stored: &[StoredFile], arrived: &[Arrived]) -> R<Value> {
    let mut out = Vec::with_capacity(stored.len());
    for (f, a) in stored.iter().zip(arrived) {
        let mut v = serde_json::to_value(a).map_err(Error::from)?;
        if let Value::Object(m) = &mut v {
            m.insert("filename".into(), json!(f.filename));
            m.insert("sha256".into(), json!(f.sha256));
        }
        out.push(v);
    }
    Ok(json!({"items": out}))
}

async fn share_target(State(rt): State<Runtime>, caller: Caller, headers: HeaderMap, req: Request) -> R<Response> {
    let form = read_form(&rt, req, &["files", "file"]).await?;
    let profile = form
        .profile
        .clone()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "me".into());
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, profile.trim(), None, Some("import")).await?;
    let stored = store(&rt, &form.files).await?;
    drop(form);
    let arrived = share::ingest_files(
        &mut tx,
        &rt.blobs,
        perm.profile.household_id,
        perm.profile.id,
        None,
        "share",
        &stored,
    )
    .await?;
    let mut audit = AuditEvent::new(caller.0.actor_kind(), "ui", "import");
    audit.actor_id = caller.0.user_id;
    audit.tool = Some("POST /api/ingest/share".into());
    audit.profile_id = Some(perm.profile.id);
    audit.resource_type = Some("upload".into());
    audit.resource_ids = Some(stored.iter().map(|s| s.sha256.clone()).collect());
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;
    if headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok()) == Some("navigate") {
        return Ok((StatusCode::SEE_OTHER, [(header::LOCATION, "/import-review")]).into_response());
    }
    Ok(Json(items(&stored, &arrived)?).into_response())
}

async fn shortcut_push(rt: &Runtime, source: &Source, req: Request) -> R<(RunOutcome, Value)> {
    let profile_id = source.profile_id.ok_or_else(|| {
        Fail::Api(ApiError::new(
            StatusCode::CONFLICT,
            "no_profile",
            "This Shortcut source is not bound to a profile.",
        ))
    })?;
    let form = read_form(rt, req, &["file", "files"]).await?;
    let stored = store(rt, &form.files).await?;
    drop(form);
    let mut tx = rt.pool.begin().await?;
    let arrived = share::ingest_files(
        &mut tx,
        &rt.blobs,
        source.household_id,
        profile_id,
        Some(source.id),
        "shortcut",
        &stored,
    )
    .await?;
    tx.commit().await?;
    let mut o = RunOutcome::default();
    for a in &arrived {
        o.seen += 1;
        match a {
            Arrived::Queued { .. } => o.new += 1,
            Arrived::Duplicate { .. } | Arrived::AlreadySeen { .. } => o.duplicate += 1,
            Arrived::Unsupported { .. } => o.problems.push(RunProblem {
                code: "unsupported".into(),
                message: "the Shortcut sent a file type OwnHealth does not import".into(),
            }),
        }
    }
    Ok((o, items(&stored, &arrived)?))
}

async fn shortcut(State(rt): State<Runtime>, SourceCaller(source): SourceCaller, req: Request) -> R<Json<Value>> {
    if source.kind != "shortcut" {
        return Err(wrong_source(&source.kind));
    }
    let run_id = {
        let mut conn = rt.pool.acquire().await?;
        begin_run(&mut conn, source.id, "push", None).await?
    };
    let result = shortcut_push(&rt, &source, req).await;
    let mut conn = rt.pool.acquire().await?;
    match &result {
        Ok((o, _)) => finish_run(&mut conn, run_id, Ok(o)).await?,
        Err(Fail::Api(e)) => finish_run(&mut conn, run_id, Err(&e.message)).await?,
        Err(Fail::Invalid(_)) => finish_run(&mut conn, run_id, Err("invalid request")).await?,
    }
    result.map(|(_, v)| Json(v))
}
