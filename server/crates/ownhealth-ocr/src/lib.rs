//! Lane C (v0.2): local OCR for scans, photos and pages with garbage glyphs (ingestion.md 6, 6.1;
//! SPIKES-v0.2.md a). The text layer stays first: `import.pdf` sends only pages without a usable text
//! layer (and images) here, through the `ocr.document` job on the `ocr` queue, and only when the `ocr`
//! compose profile is on (`OWNHEALTH_OCR=1`). Without it those pages wait as pending for an outside
//! agent (`list_pending_documents`).
//!
//! The engine is PaddleOCR PP-OCRv5 (mobile detection + Latin mobile recognition) exported to ONNX,
//! run by oar-ocr on ONNX Runtime (`ort`), CPU by default; an execution provider (OpenVINO, CUDA) is
//! optional. Models come from `scripts/fetch-ocr-models.sh` (pinned, sha256-checked) into
//! `OWNHEALTH_OCR_MODELS_DIR`; only the `ocr` image carries them.
//!
//! | Module | What |
//! |---|---|
//! | `engine` | load the models, read one page image into words with boxes and confidences |
//! | `route` | which pages need OCR (no text layer, scans, photos, garbage glyphs) |
//! | `job` | `ocr.document`: render, OCR, store the word layer (`document_page.word_layer = 'ocr'`), extract, route rows |
//! | `grounding` | is an extracted value on the page's word layer (text layer or OCR)? |
//! | `bench` | pages/s and row F1 per engine on the scanned eval set (`docs/ocr-benchmark.md`) |
//! | `demo` | the demo's one scanned page waiting for OCR |

pub mod bench;
pub mod demo;
pub mod engine;
pub mod grounding;
pub mod job;
pub mod route;

pub use job::{Reader, ocr_document, use_reader};

use serde::{Deserialize, Serialize};

/// `ocr_run.engine`.
pub const ENGINE: &str = "paddleocr-onnx";
/// `ocr_run.engine_version`: the model pair and the runtime crate.
pub const ENGINE_VERSION: &str = "pp-ocrv5-mobile-det+latin-mobile-rec@oar-ocr-0.10.2";
/// Rendering resolution for OCR (the spike measured 200 dpi).
pub const OCR_DPI: f64 = 200.0;

/// One word OCR read. `bbox` is `[x0, top, x1, bottom]` in PDF points, origin top-left, like the text
/// layer's words, so grounding and crops work the same on both layers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrWord {
    pub text: String,
    pub bbox: [f64; 4],
    /// 0..1, the recognizer's confidence.
    pub conf: f32,
}

/// One page as OCR read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrPage {
    pub page_no: i32,
    pub width_pt: f64,
    pub height_pt: f64,
    pub words: Vec<OcrWord>,
    pub engine: String,
    pub engine_version: String,
    pub seconds: f64,
}

/// Why OCR could not run. Becomes `ocr_run.error` and `document.ocr_state = 'failed'`.
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("this build has no OCR engine (build with the `ocr` feature)")]
    EngineMissing,
    #[error("OCR model file missing or wrong: {0}")]
    Model(String),
    #[error("OCR failed: {0}")]
    Engine(String),
    #[error(transparent)]
    Core(#[from] ownhealth_core::Error),
}

/// Register `ocr.document`. Called by `ownhealth::register_lanes`.
pub fn register() {
    job::register();
    ownhealth_core::demo::register_demo_hook("ocr", 1030, |conn, ctx| Box::pin(demo::seed_demo(conn, ctx)));
}
