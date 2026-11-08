//! Database fixtures for the import tests (`0.1.1 tests/pdf/conftest.py`): a throwaway database with
//! the Rivera family, a blob store in a temp dir, the stand-in normalizer and lane C's retract.

use std::sync::Arc;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::config::Settings;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::JobContext;
use ownhealth_core::testing::TestDb;
use serde_json::Value;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::pool::PoolConnection;
use uuid::Uuid;

pub struct Env {
    pub db: TestDb,
    pub dir: tempfile::TempDir,
    pub blobs: BlobStore,
    pub fam: Family,
    pub settings: Settings,
}

/// Lane B's hooks: the job and accept handlers.
pub fn hooks() {
    ownhealth_ingest_pdf::register();
}

/// The stand-in normalizer on (lane B's pipeline alone), as the Python `fake_normalizer` fixture.
pub fn fake_normalizer() {
    ownhealth_ingest_pdf::use_normalizer(Some(super::fake_normalize::normalize_row));
}

pub async fn env() -> anyhow::Result<Env> {
    hooks();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(dir.path().join("blobs"))?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    drop(c);
    Ok(Env {
        db,
        dir,
        blobs,
        fam,
        settings,
    })
}

impl Env {
    pub async fn conn(&self) -> anyhow::Result<PoolConnection<Postgres>> {
        Ok(self.db.pool.acquire().await?)
    }

    /// A stored upload on that profile; returns the document id.
    pub async fn store(
        &self,
        conn: &mut PgConnection,
        data: &[u8],
        slug: &str,
        mime: &str,
        filename: &str,
    ) -> anyhow::Result<Uuid> {
        let sha = self.blobs.put_bytes(data)?;
        let d = NewDocument {
            filename: Some(filename),
            ..NewDocument::upload(
                self.fam.household_id,
                Some(self.fam.profile(slug)),
                &sha,
                mime,
                data.len() as i64,
            )
        };
        Ok(create_document(conn, &d).await?.id)
    }

    pub async fn store_pdf(&self, conn: &mut PgConnection, data: &[u8], filename: &str) -> anyhow::Result<Uuid> {
        self.store(conn, data, "alex", "application/pdf", filename).await
    }

    pub fn job_context(&self) -> JobContext {
        JobContext {
            pool: self.db.pool.clone(),
            settings: Arc::new(self.settings.clone()),
            blobs: self.blobs.clone(),
            worker_id: "test".into(),
        }
    }

    /// Run import.pdf jobs until the queue is empty (at most 20).
    pub async fn drain(&self) -> anyhow::Result<()> {
        let ctx = self.job_context();
        let kinds = ["import.pdf".to_string()];
        for _ in 0..20 {
            if ownhealth_core::jobs::run_one(&ctx, "core", Some(&kinds))
                .await?
                .is_none()
            {
                return Ok(());
            }
        }
        anyhow::bail!("the import.pdf queue did not drain in 20 runs")
    }
}

/// review_item rows of a document as JSON, oldest first.
pub async fn items(conn: &mut PgConnection, doc: Uuid, kind: Option<&str>) -> anyhow::Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select to_jsonb(r) from review_item r where document_id = $1 and ($2::text is null or kind = $2) \
         order by created_at, id",
    )
    .bind(doc)
    .bind(kind)
    .fetch_all(conn)
    .await?)
}

/// The document's observations with their src page, bbox and kind.
pub async fn obs(conn: &mut PgConnection, doc: Uuid) -> anyhow::Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select to_jsonb(o) || jsonb_build_object('src_page', s.page_no, 'src_bbox', s.bbox, 'src_kind', s.kind) \
         from observation o join src s on s.id = o.src_id where o.document_id = $1",
    )
    .bind(doc)
    .fetch_all(conn)
    .await?)
}

pub fn uuid(v: &Value) -> Uuid {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or_default()
}
