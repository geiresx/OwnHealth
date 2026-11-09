//! "A document arrives": the one entry point every inbox calls (email, watched folder, Paperless,
//! share target, iOS Shortcut), ingestion.md 2 and 3.
//!
//! The bytes are already in the blob store (`store_bytes`). `arrive` then runs, in the caller's
//! transaction:
//!
//! 1. the source's own id (Message-ID, Paperless id, path) seen before: stop (`AlreadySeen`);
//! 2. sniff the kind; a kind nobody imports is remembered and listed, not processed (`Unsupported`);
//! 3. exact file hash for the profile: one more `document_source` row on the existing document, stop
//!    (`Duplicate`);
//! 4. enqueue the importer: `import.pdf` for PDFs and images (classify, text layer first, OCR or
//!    pending for pages without one, extraction, review queue), the structured importers otherwise.
//!
//! The near-duplicate check (dedup level 2) runs inside `import.pdf` once the pages exist, and the
//! observation match (level 3) when rows are routed. Quarantined mail never comes here until a person
//! releases it.

use std::io::Read;

use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::blobs::BlobStore;
use crate::dedup::{record_item, seen_item};
use crate::documents::{NewDocument, UploadKind, create_document, job_kind, mime, sniff};
use crate::error::{Error, Result};
use crate::ids::token_hex;
use crate::jobs::{EnqueueOptions, enqueue};

/// The OCR job (lane C) on the `ocr` queue: `{document_id, pages: [i32], why}`.
pub const OCR_JOB: &str = "ocr.document";

#[derive(Debug, Clone, PartialEq)]
pub struct Arrival<'a> {
    pub household_id: Uuid,
    /// The profile the routing rule picked (sender rule, subfolder, Paperless rule, token binding).
    pub profile_id: Uuid,
    pub source_id: Option<Uuid>,
    /// `document_source.channel`: email | folder | paperless | share | shortcut | upload | ...
    pub channel: &'a str,
    /// Where it came from, never content: Message-ID, Paperless document id, file name.
    pub source_ref: Option<&'a str>,
    /// The per-source dedup key (`source_item.external_id`); needs `source_id`.
    pub external_id: Option<&'a str>,
    /// As sent; untrusted text.
    pub filename: Option<&'a str>,
    pub sha256: &'a str,
    pub size: i64,
    /// The locked original of an unlocked password PDF.
    pub derived_from: Option<Uuid>,
}

impl<'a> Arrival<'a> {
    pub fn new(household_id: Uuid, profile_id: Uuid, channel: &'a str, sha256: &'a str, size: i64) -> Self {
        Arrival {
            household_id,
            profile_id,
            source_id: None,
            channel,
            source_ref: None,
            external_id: None,
            filename: None,
            sha256,
            size,
            derived_from: None,
        }
    }
}

/// What happened to one arrival.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Arrived {
    /// The source handed us this item before.
    AlreadySeen { document_id: Option<Uuid> },
    /// The same bytes are already a document of this profile.
    Duplicate { document_id: Uuid },
    /// An import job was queued (or one for these bytes is queued already: `job_id` None). Structured
    /// imports make their own document, so `document_id` is None for them.
    Queued {
        document_id: Option<Uuid>,
        kind: UploadKind,
        job_id: Option<i64>,
    },
    /// Stored and remembered, not processed: nothing imports this kind.
    Unsupported { kind: UploadKind },
}

/// Put bytes in the blob store off the async runtime. Returns `(sha256 hex, size)`.
pub async fn store_bytes(blobs: &BlobStore, data: Vec<u8>) -> Result<(String, u64)> {
    let blobs = blobs.clone();
    tokio::task::spawn_blocking(move || {
        let n = data.len() as u64;
        blobs.put_bytes(&data).map(|sha| (sha, n))
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("blob write task failed: {e}")))?
    .map_err(Error::from)
}

async fn sniff_blob(blobs: &BlobStore, sha256: &str, filename: String) -> Result<UploadKind> {
    let path = blobs.path(sha256)?;
    tokio::task::spawn_blocking(move || -> Result<UploadKind> {
        let mut head = Vec::with_capacity(4096);
        std::fs::File::open(&path)?.take(4096).read_to_end(&mut head)?;
        Ok(sniff(&filename, &head, Some(&path)))
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("sniff task failed: {e}")))?
}

async fn remember(conn: &mut PgConnection, a: &Arrival<'_>, document_id: Option<Uuid>) -> Result<()> {
    if let (Some(source_id), Some(ext)) = (a.source_id, a.external_id) {
        record_item(conn, source_id, ext, Some(a.sha256), document_id, None).await?;
    }
    Ok(())
}

/// One arrival, in the caller's transaction. See the module docs for the steps.
pub async fn arrive(conn: &mut PgConnection, blobs: &BlobStore, a: &Arrival<'_>) -> Result<Arrived> {
    if a.external_id.is_some() && a.source_id.is_none() {
        return Err(Error::Invalid("an external_id needs its source_id".into()));
    }
    if let (Some(source_id), Some(ext)) = (a.source_id, a.external_id)
        && let Some(item) = seen_item(&mut *conn, source_id, ext).await?
    {
        return Ok(Arrived::AlreadySeen {
            document_id: item.document_id,
        });
    }
    let filename = a.filename.unwrap_or("").to_string();
    let kind = sniff_blob(blobs, a.sha256, filename.clone()).await?;
    match kind {
        "pdf" | "image" => {
            let m = mime(kind).unwrap_or_else(|| image_mime(&filename));
            let r = create_document(
                &mut *conn,
                &NewDocument {
                    filename: a.filename,
                    channel: a.channel,
                    source_ref: a.source_ref,
                    source_id: a.source_id,
                    derived_from: a.derived_from,
                    ..NewDocument::upload(a.household_id, Some(a.profile_id), a.sha256, m, a.size)
                },
            )
            .await?;
            remember(&mut *conn, a, Some(r.id)).await?;
            if !(r.created || r.reactivated) {
                return Ok(Arrived::Duplicate { document_id: r.id });
            }
            let mut payload = json!({
                "document_id": r.id.to_string(),
                "profile_id": a.profile_id.to_string(),
                "household_id": a.household_id.to_string(),
            });
            let mut key = format!("import.pdf:{}", r.id);
            if r.reactivated {
                payload["force"] = json!(true);
                key = format!("{key}:reupload:{}", token_hex(4));
            }
            let job_id = enqueue(&mut *conn, "import.pdf", &payload, EnqueueOptions::dedupe(&key)).await?;
            Ok(Arrived::Queued {
                document_id: Some(r.id),
                kind,
                job_id,
            })
        }
        _ => match job_kind(kind) {
            None => {
                remember(&mut *conn, a, None).await?;
                Ok(Arrived::Unsupported { kind })
            }
            Some(job) => {
                let mut payload = json!({
                    "profile_id": a.profile_id.to_string(),
                    "household_id": a.household_id.to_string(),
                    "blob_sha256": a.sha256,
                    "filename": filename,
                    "channel": a.channel,
                });
                if kind == "fhir_ndjson" {
                    payload["format"] = json!("ndjson");
                }
                if let Some(s) = a.source_id {
                    payload["source_id"] = Value::String(s.to_string());
                }
                let key = format!("{job}:{}:{}", a.profile_id, a.sha256);
                let job_id = enqueue(&mut *conn, job, &payload, EnqueueOptions::dedupe(&key)).await?;
                remember(&mut *conn, a, None).await?;
                Ok(Arrived::Queued {
                    document_id: None,
                    kind,
                    job_id,
                })
            }
        },
    }
}

/// Send unread pages of a document to the OCR queue (the `ocr` compose profile is on) and mark the
/// document `ocr_state = 'queued'`. Called by `import.pdf` instead of filing a `pending_document`.
pub async fn enqueue_ocr(conn: &mut PgConnection, document_id: Uuid, pages: &[i32], why: &str) -> Result<Option<i64>> {
    sqlx::query("update document set ocr_state = 'queued' where id = $1")
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    let key = format!("{OCR_JOB}:{document_id}");
    Ok(enqueue(
        conn,
        OCR_JOB,
        &json!({"document_id": document_id.to_string(), "pages": pages, "why": why}),
        EnqueueOptions {
            queue: "ocr",
            dedupe_key: Some(&key),
            ..Default::default()
        },
    )
    .await?)
}

/// Set `document.ocr_state` (lane C moves it through running, done and failed; `import.pdf` sets
/// `off` for a scan that waits because the profile is off).
pub async fn set_ocr_state(conn: &mut PgConnection, document_id: Uuid, state: &str) -> Result<()> {
    sqlx::query("update document set ocr_state = $2 where id = $1")
        .bind(document_id)
        .bind(state)
        .execute(conn)
        .await?;
    Ok(())
}

/// The mime of an image by its name: PNG, HEIC/HEIF (iPhone photos), JPEG otherwise.
fn image_mime(filename: &str) -> &'static str {
    let name = filename.to_lowercase();
    if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".heic") || name.ends_with(".heif") {
        "image/heic"
    } else {
        "image/jpeg"
    }
}

#[cfg(test)]
mod tests {
    use super::image_mime;

    #[test]
    fn images_are_labelled_by_their_name() {
        assert_eq!(image_mime("IMG_0042.HEIC"), "image/heic");
        assert_eq!(image_mime("scan.heif"), "image/heic");
        assert_eq!(image_mime("page.PNG"), "image/png");
        assert_eq!(image_mime("photo.jpg"), "image/jpeg");
        assert_eq!(image_mime(""), "image/jpeg");
    }
}
