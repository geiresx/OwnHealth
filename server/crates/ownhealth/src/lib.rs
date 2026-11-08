//! The OwnHealth binary as a library, so tests can build the app without a process.

pub mod app;
pub mod ui;

/// Register every lane's job handlers, accept handlers and demo hooks. Idempotent.
pub fn register_lanes() {
    ownhealth_normalize::register();
    ownhealth_ingest_pdf::register();
    ownhealth_ingest_structured::register();
    ownhealth_genome::register();
    ownhealth_rules::register();
    ownhealth_mcp::register();
    ownhealth_auth::register();
    ownhealth_ops::register();
}
