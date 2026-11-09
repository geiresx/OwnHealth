//! v0.2 inboxes (ingestion.md 3, connectors.md 2.1): data that arrives without an upload.
//!
//! | Module | Lane | What |
//! |---|---|---|
//! | `email` | A | IMAP poll of a dedicated mailbox, per-profile routing by sender rule, allowlist, quarantine and its release, per-sender PDF passwords (`email::pdf_password`) |
//! | `paperless` | B | Paperless-ngx "Document Added" webhook, API pull of the original, polling fallback; the Paperless id is the source link |
//! | `folder` | B | the watched folder `<OWNHEALTH_INBOX_DIR>/<profile-slug>/`, polled; `.done` and `.failed` |
//! | `share` | B | the PWA share target and the iOS Shortcut upload (a push token bound to one profile) |
//!
//! Every item ends in `ownhealth_core::arrive::arrive` (or `ownhealth_core::quarantine::hold` for mail
//! nobody allowed). Pollers register with `ownhealth_core::sources::register_poller`; the scheduler and
//! the run records are the core's. Free text from outside (subjects, bodies, file names, Paperless
//! notes) is untrusted text and is never logged.

pub mod email;
pub mod folder;
pub mod paperless;
pub mod share;

/// Register the pollers of this crate (email, folder, paperless). Called by `ownhealth::register_lanes`.
pub fn register() {
    email::register();
    folder::register();
    paperless::register();
}
