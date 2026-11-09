//! Lane B: lab PDF import (`ingest/pdf/`, `ingest/classify.py`, `jobs/kinds/pdf.py`; ingestion.md
//! 5-7, 9, 11). Job kind `import.pdf`.
//!
//! Steps, all in the caller's transaction (so a crashed job leaves nothing half done):
//!   1. read the text layer (`textlayer`, PDFium): words with bboxes and fonts, hidden-text and
//!      injection flags;
//!   2. store one `document_page` per page (word layer as a blob, page text, full-text vector);
//!   3. classify (`classify`): type, language, patient block, dates, report status; check the identity
//!      on the page against the profile and guess the right profile when it does not match;
//!   4. read the lab table (`extract` -> vendor template, else generic rules) into candidate rows;
//!   5. normalize each row (lane A), add grounding, identity, date, duplicate and conflict checks, and
//!      route it through `core::review::submit_candidate` (auto-accept or review queue);
//!   6. draw grouping: `report_key` per draw; an amended or final edition of an earlier report links
//!      identical rows, confirms provisional ones, and queues changed values as `amended_report`;
//!      "pending" lines become `expected_test` rows.
//!
//! A page without a usable text layer, an image, or a lab report whose table nobody could read becomes
//! one `pending_document` review item for agent-assisted extraction (MCP `propose`), or, with
//! `OWNHEALTH_OCR` on, an `ocr.document` job (lane C) and no review item until OCR has run.
//! Review kinds it files: `candidate_row` (through the core), `amended_report`, `document_identity`,
//! `pending_document`.

pub mod afm;
pub mod classify;
pub mod extract;
mod import;
pub mod pages;
pub mod pdfium;
pub mod py;
pub mod tables;
pub mod templates;
pub mod textlayer;

use futures::future::BoxFuture;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::review::register_accept_handler;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use import::{
    ImportOptions, Normalizer, OcrPages, OcrRowCheck, assign_document, import_ocr, import_pdf, import_pdf_opts,
    import_pdf_with, use_normalizer,
};
pub use pages::{decode_image, ground_value, page_words, render_page_png};

/// One word of a page's stored text layer. bbox is `[x0, top, x1, bottom]` in PDF points, origin
/// top-left. `hidden` is white_on_white | tiny | outside_page: never read as a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub bbox: [f64; 4],
    #[serde(default)]
    pub font: String,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub hidden: Option<String>,
}

/// Python `UUID(str(job.payload["document_id"]))` and its error text.
fn payload_document_id(job: &Job) -> Result<Uuid, JobError> {
    let Some(v) = job.payload.get("document_id") else {
        return Err(JobError::permanent("bad payload: 'document_id'"));
    };
    let s = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
    Uuid::parse_str(&s).map_err(|_| JobError::permanent("bad payload: badly formed hexadecimal UUID string"))
}

async fn run_import_pdf(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let document_id = payload_document_id(&job)?;
    let force = job.payload.get("force").is_some_and(truthy);
    let mut tx = ctx.pool.begin().await?;
    let summary = match import_pdf_opts(
        &mut tx,
        &ctx.blobs,
        document_id,
        force,
        ImportOptions {
            threshold: ctx.settings.auto_accept_threshold,
            ocr: ctx.settings.ocr,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(ownhealth_core::Error::NotFound(msg)) => return Err(JobError::permanent(msg)),
        Err(e) => return Err(e.into()),
    };
    tx.commit().await?;
    Ok(Some(summary.to_value()))
}

/// Python `bool(value)` for a JSON value.
fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

fn import_pdf_job(ctx: JobContext, job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(run_import_pdf(ctx, job))
}

/// Register lane B's job handler (`import.pdf`) and accept handlers (`amended_report`,
/// `document_identity`).
pub fn register() {
    jobs::register_handler("import.pdf", import_pdf_job);
    register_accept_handler("amended_report", |conn, item, user, edits| {
        Box::pin(import::accept_amended(conn, item, user, edits))
    });
    register_accept_handler("document_identity", |conn, item, user, edits| {
        Box::pin(import::accept_identity(conn, item, user, edits))
    });
}
