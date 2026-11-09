//! Lane D fixtures: a throwaway database with the Rivera family, a blob store in a temp dir, a job
//! context with a test secrets key, and a source helper. Synthetic data only.
#![allow(dead_code)]

use std::sync::Arc;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::jobs::JobContext;
use ownhealth_core::sources::{self, NewSource, Source};
use ownhealth_core::testing::TestDb;
use serde_json::Value;
use uuid::Uuid;

pub type R = anyhow::Result<()>;

/// 32 bytes of 'L', base64: a test key, never a real one.
pub const TEST_SECRETS_KEY: &str = "TExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTEw=";

pub struct Env {
    pub db: TestDb,
    pub dir: tempfile::TempDir,
    pub blobs: BlobStore,
    pub ctx: JobContext,
    pub fam: Family,
}

impl Env {
    pub async fn new() -> anyhow::Result<Env> {
        ownhealth_ingest_structured::register();
        ownhealth_devices::register();
        ownhealth_core::sources::register();
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let mut settings = db.settings(dir.path())?;
        settings.secrets_key = Some(TEST_SECRETS_KEY.into());
        let blobs = BlobStore::new(dir.path().join("blobs"))?;
        let ctx = JobContext::new(db.pool.clone(), Arc::new(settings), blobs.clone());
        let mut c = db.pool.acquire().await?;
        let fam = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
        Ok(Env {
            db,
            dir,
            blobs,
            ctx,
            fam,
        })
    }

    pub async fn source(
        &self,
        kind: &str,
        slug: Option<&str>,
        config: Value,
        secret_id: Option<Uuid>,
    ) -> anyhow::Result<Source> {
        let mut c = self.db.pool.acquire().await?;
        let id = sources::create_source(
            &mut c,
            &NewSource {
                profile_id: slug.map(|s| self.fam.profile(s)),
                config,
                secret_id,
                ..NewSource::new(self.fam.household_id, kind, "test inbox")
            },
        )
        .await?;
        sources::get_source(&mut c, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no source"))
    }

    pub async fn count(&self, sql: &str, profile: Uuid) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(profile)
            .fetch_one(&self.db.pool)
            .await?)
    }
}
