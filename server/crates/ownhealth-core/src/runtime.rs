//! Process-wide handles (`runtime.py`): settings, the pool and the blob store. The binary builds
//! one `Runtime` and hands clones to the HTTP state, the MCP service and the job workers.

use std::sync::Arc;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::blobs::BlobStore;
use crate::config::Settings;
use crate::jobs::JobContext;

#[derive(Debug, Clone)]
pub struct Runtime {
    pub settings: Arc<Settings>,
    pub pool: PgPool,
    pub blobs: BlobStore,
}

impl Runtime {
    /// Connect lazily (the first query opens the pool), so `serve` starts and `/readyz` reports a
    /// database that is not up yet instead of the process exiting.
    pub fn new(settings: Settings) -> anyhow::Result<Runtime> {
        let pool = PgPoolOptions::new()
            .max_connections(20)
            .connect_lazy(&settings.sqlx_url())?;
        let blobs = BlobStore::new(settings.blobs_path())?;
        Ok(Runtime {
            settings: Arc::new(settings),
            pool,
            blobs,
        })
    }

    pub fn with_pool(settings: Settings, pool: PgPool) -> anyhow::Result<Runtime> {
        let blobs = BlobStore::new(settings.blobs_path())?;
        Ok(Runtime {
            settings: Arc::new(settings),
            pool,
            blobs,
        })
    }

    pub fn job_context(&self) -> JobContext {
        JobContext::new(self.pool.clone(), self.settings.clone(), self.blobs.clone())
    }
}
