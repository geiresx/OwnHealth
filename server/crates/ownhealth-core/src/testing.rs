//! Test helpers (feature `testing`, and this crate's own tests): a throwaway migrated database in
//! the shared test Postgres (`scripts/test-db.sh`, 127.0.0.1:55432), dropped when the `TestDb`
//! goes out of scope. Never touches another database.

use std::collections::HashMap;
use std::path::Path;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};

use crate::config::Settings;

pub const DEFAULT_ADMIN_URL: &str = "postgres://postgres:postgres@127.0.0.1:55432/postgres";

/// `OWNHEALTH_TEST_ADMIN_URL`, the same variable the Python tests read.
pub fn admin_url() -> String {
    std::env::var("OWNHEALTH_TEST_ADMIN_URL").unwrap_or_else(|_| DEFAULT_ADMIN_URL.into())
}

fn url_for(db: &str) -> String {
    let admin = admin_url();
    let base = admin.rsplit_once('/').map_or(admin.as_str(), |(b, _)| b);
    format!("{base}/{db}")
}

#[derive(Debug)]
pub struct TestDb {
    pub name: String,
    pub url: String,
    pub pool: PgPool,
}

impl TestDb {
    /// A fresh database with every migration applied.
    pub async fn new() -> anyhow::Result<TestDb> {
        let db = TestDb::empty().await?;
        crate::migrate::migrate_embedded(&db.pool).await?;
        Ok(db)
    }

    /// A fresh database with no tables (for migration tests).
    pub async fn empty() -> anyhow::Result<TestDb> {
        let name = format!("oh_test_{}", crate::ids::token_hex(6));
        let mut admin = PgConnection::connect(&admin_url())
            .await
            .map_err(|e| anyhow::anyhow!("test Postgres not reachable: run scripts/test-db.sh ({e})"))?;
        sqlx::query(sqlx::AssertSqlSafe(format!("create database \"{name}\"")))
            .execute(&mut admin)
            .await?;
        admin.close().await?;
        let url = url_for(&name);
        let pool = PgPoolOptions::new().max_connections(10).connect(&url).await?;
        Ok(TestDb { name, url, pool })
    }

    /// Settings pointing at this database, with dev auth on, jobs off and blobs under `data_dir`.
    pub fn settings(&self, data_dir: &Path) -> anyhow::Result<Settings> {
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("OWNHEALTH_DATABASE_URL".into(), self.url.clone());
        env.insert("OWNHEALTH_DATA_DIR".into(), data_dir.display().to_string());
        env.insert("OWNHEALTH_DEV_AUTH".into(), "1".into());
        env.insert("OWNHEALTH_RUN_JOBS".into(), "0".into());
        Ok(Settings::from(&env)?)
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let name = self.name.clone();
        // Drop may run on the test runtime thread, which this blocks, so connections still out of the
        // pool cannot come back: `with (force)` ends them instead of waiting for a pool close.
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                return;
            };
            rt.block_on(async move {
                if let Ok(mut admin) = PgConnection::connect(&admin_url()).await {
                    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                        "drop database if exists \"{name}\" with (force)"
                    )))
                    .execute(&mut admin)
                    .await;
                    let _ = admin.close().await;
                }
            });
        })
        .join();
    }
}
