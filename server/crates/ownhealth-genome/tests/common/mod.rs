//! Genome test fixtures (`tests/genome/conftest.py`): a throwaway database with the Rivera family,
//! a blob store in a temp dir, and Alex's synthetic WGS facts with a calls VCF and gVCF.
#![allow(dead_code)]

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::testing::TestDb;
use ownhealth_genome::synthetic;
use serde_json::Value;
use sqlx::PgConnection;
use sqlx::Postgres;
use sqlx::pool::PoolConnection;
use uuid::Uuid;

pub type R = anyhow::Result<()>;

pub struct Env {
    pub db: TestDb,
    pub dir: tempfile::TempDir,
    pub blobs: BlobStore,
    pub fam: Family,
}

pub async fn env() -> anyhow::Result<Env> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path().join("blobs"))?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    drop(c);
    Ok(Env { db, dir, blobs, fam })
}

impl Env {
    pub async fn conn(&self) -> anyhow::Result<PoolConnection<Postgres>> {
        Ok(self.db.pool.acquire().await?)
    }

    pub fn profile(&self, slug: &str) -> Uuid {
        self.fam.profile(slug)
    }

    /// `synthetic.seed_genome` with the default file name.
    pub async fn seed(
        &self,
        conn: &mut PgConnection,
        slug: &str,
        facts: &Value,
        attachments: &[(String, String)],
    ) -> ownhealth_core::Result<(Uuid, ImportSummary)> {
        synthetic::seed_genome(
            conn,
            &self.blobs,
            self.fam.household_id,
            self.profile(slug),
            facts,
            attachments,
            "genome-facts.json",
            None,
        )
        .await
    }

    /// Alex's facts and raw files imported. Returns (document_id, facts, summary).
    pub async fn alex_genome(&self, conn: &mut PgConnection) -> anyhow::Result<(Uuid, Value, ImportSummary)> {
        let raw = synthetic::alex_raw_files()?;
        let (facts, attachments) = synthetic::with_files(&synthetic::alex_facts(), &self.blobs, &raw)?;
        let (doc, summary) = self.seed(conn, "alex", &facts, &attachments).await?;
        Ok((doc, facts, summary))
    }
}

pub async fn count(conn: &mut PgConnection, table: &str, profile: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select count(*) from {table} where profile_id = $1"
    )))
    .bind(profile)
    .fetch_one(conn)
    .await?)
}

/// The fact of `doc` whose gene is `gene`.
pub fn fact_mut<'a>(doc: &'a mut Value, gene: &str) -> &'a mut Value {
    let facts = doc["facts"].as_array_mut().into_iter().flatten();
    let mut hit = None;
    for f in facts {
        if f.get("gene").and_then(Value::as_str) == Some(gene) {
            hit = Some(f);
            break;
        }
    }
    hit.unwrap_or_else(|| panic!("no {gene} fact"))
}
