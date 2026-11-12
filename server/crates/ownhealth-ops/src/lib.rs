//! Lane H: operations (`ops/`, `export/`, `demo/base.py`; architecture.md 13-14).
//!
//! - `backup`, `restore`, `verify_restore`: a backup is a pg_dump plus hard links of the blobs and a
//!   manifest (`ownhealth.backup/v1`); restore and verify-restore compare table counts, blob hashes
//!   and the state_hash of every profile.
//! - `export::export_profile`: the full export of one profile (FHIR NDJSON plus the original files,
//!   one zip), for the step-up-protected REST route.
//! - `summary_pdf::render`: the printed doctor summary, two A4 pages through `pdfwrite`.
//! - the demo hook `base` (order 0), the port of `demo/base.py`.

mod backup;
mod demo_base;
pub mod export;
pub mod summary_pdf;

pub use backup::{MANIFEST_SCHEMA, backup, restore, verify_restore};
pub use demo_base::{seed_base, seed_family};

use ownhealth_core::demo;

/// Register lane H's demo hooks: `base` (order 0, the port of `demo/base.py`) and `family` (v0.3,
/// after every other hook).
pub fn register() {
    demo::register_demo_hook("base", 0, |conn, ctx| Box::pin(seed_base(conn, ctx)));
    demo::register_demo_hook("family", 1100, |conn, ctx| Box::pin(seed_family(conn, ctx)));
}
