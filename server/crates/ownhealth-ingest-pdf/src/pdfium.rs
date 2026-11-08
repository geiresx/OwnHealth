//! The one PDFium binding of the process. The shared library is loaded at runtime (SPIKES.md b):
//! from `OWNHEALTH_PDFIUM_LIB` (the file or its directory), else `server/vendor/pdfium/<platform>/lib`
//! (what `scripts/fetch-pdfium.sh` writes, for local runs and tests), else the system library path.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ownhealth_core::{Error, Result};
use pdfium_render::prelude::*;

static PDFIUM: OnceLock<std::result::Result<Pdfium, String>> = OnceLock::new();

fn vendor_dir() -> Option<PathBuf> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        _ => return None,
    };
    Some(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/pdfium")
            .join(platform)
            .join("lib"),
    )
}

fn bind() -> std::result::Result<Pdfium, String> {
    let mut tried = Vec::new();
    let mut candidates = Vec::new();
    if let Ok(p) = std::env::var("OWNHEALTH_PDFIUM_LIB") {
        let p = PathBuf::from(p);
        candidates.push(if p.is_dir() {
            Pdfium::pdfium_platform_library_name_at_path(&p)
        } else {
            p
        });
    }
    if let Some(d) = vendor_dir() {
        candidates.push(Pdfium::pdfium_platform_library_name_at_path(&d));
    }
    for c in candidates {
        match Pdfium::bind_to_library(&c) {
            Ok(b) => return Ok(Pdfium::new(b)),
            Err(e) => tried.push(format!("{}: {e}", c.display())),
        }
    }
    match Pdfium::bind_to_system_library() {
        Ok(b) => Ok(Pdfium::new(b)),
        Err(e) => {
            tried.push(format!("system library: {e}"));
            Err(format!("PDFium is not available ({})", tried.join("; ")))
        }
    }
}

/// The process-wide PDFium (bound on first use).
pub fn pdfium() -> Result<&'static Pdfium> {
    PDFIUM
        .get_or_init(bind)
        .as_ref()
        .map_err(|e| Error::Other(anyhow::anyhow!("{e}")))
}

pub(crate) fn pdf_error(e: PdfiumError) -> Error {
    Error::Other(anyhow::anyhow!("unreadable PDF: {e}"))
}
