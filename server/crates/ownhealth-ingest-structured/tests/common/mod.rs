//! Lane C fixtures (`tests/imports/conftest.py`): a throwaway database with the Rivera family, a blob
//! store in a temp dir, and synthetic FHIR files and Apple Health exports stored as documents.
#![allow(dead_code)]

use std::path::PathBuf;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::testing::TestDb;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

pub type R = anyhow::Result<()>;

pub const JSON: &str = "application/fhir+json";
pub const ZIP: &str = "application/zip";

pub struct Env {
    pub db: TestDb,
    pub dir: tempfile::TempDir,
    pub blobs: BlobStore,
}

impl Env {
    pub async fn new() -> anyhow::Result<Env> {
        ownhealth_ingest_structured::register();
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let blobs = BlobStore::new(dir.path().join("blobs"))?;
        Ok(Env { db, dir, blobs })
    }

    pub async fn tx(&self) -> anyhow::Result<Transaction<'static, Postgres>> {
        Ok(self.db.pool.begin().await?)
    }
}

pub async fn family(conn: &mut PgConnection) -> anyhow::Result<Family> {
    Ok(create_rivera_family(conn, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?)
}

/// `store_doc(conn, family, slug, data, filename, mime)` -> document id.
pub async fn store_doc(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    fam: &Family,
    slug: &str,
    data: &[u8],
    filename: &str,
    mime: &str,
) -> anyhow::Result<Uuid> {
    let sha = blobs.put_bytes(data)?;
    let d = create_document(
        conn,
        &NewDocument {
            filename: Some(filename),
            ..NewDocument::upload(fam.household_id, Some(fam.profile(slug)), &sha, mime, data.len() as i64)
        },
    )
    .await?;
    Ok(d.id)
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures")
}

/// The bytes of `fixtures/fhir/<name>`.
pub fn fhir_file(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join("fhir").join(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}
