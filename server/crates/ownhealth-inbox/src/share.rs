//! Lane B: phone pushes (ingestion.md 3). The PWA share target (Android) posts multipart to
//! `POST /api/ingest/share` from a logged-in session; the profile is chosen on the share screen. The
//! iOS Shortcut posts the shared file to `POST /api/ingest/shortcut` with a push token
//! (`oh_ing_...`, `core::sources::issue_token`) of a `shortcut` source bound to one profile.
//! Both store the files and call `core::arrive::arrive` once per file.

use ownhealth_core::arrive::{Arrival, Arrived, arrive};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::StoredFile;
use ownhealth_core::error::{Error, Result};
use sqlx::PgConnection;
use uuid::Uuid;

/// The channels a phone push arrives on (`document_source.channel`).
pub const CHANNELS: [&str; 2] = ["share", "shortcut"];

/// Files already in the blob store, from a share or a Shortcut, for one profile, in the caller's
/// transaction. One `Arrived` per file, in order.
pub async fn ingest_files(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    household_id: Uuid,
    profile_id: Uuid,
    source_id: Option<Uuid>,
    channel: &str,
    files: &[StoredFile],
) -> Result<Vec<Arrived>> {
    if !CHANNELS.contains(&channel) {
        return Err(Error::Invalid(format!("unknown phone channel {channel:?}")));
    }
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        out.push(
            arrive(
                &mut *conn,
                blobs,
                &Arrival {
                    source_id,
                    filename: Some(&f.filename),
                    ..Arrival::new(household_id, profile_id, channel, &f.sha256, f.size)
                },
            )
            .await?,
        );
    }
    Ok(out)
}
