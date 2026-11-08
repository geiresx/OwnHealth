//! Lane E test fixtures (0.1.1 tests/state/conftest.py): a database of its own loaded with the rows
//! the Python base demo seed wrote (`fixtures/seed.sql`, synthetic Rivera family), the answers the
//! Python code gave for them (`fixtures/expected.json`), and a stand-in for lane D's genome reads with
//! the synthetic findings of architecture.md section 16. The generator of both files left with the Python backend in 0.2.0 (see git history).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Once};

use chrono::NaiveDate;
use ownhealth_core::Result;
use ownhealth_core::testing::TestDb;
use ownhealth_rules::genome::{GenomeFuture, GenomeSource, set_genome_source};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Beside this file; the api tests include it from their own crate.
pub const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ownhealth-rules/tests/fixtures");

pub fn as_of() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default()
}

pub static EXPECTED: LazyLock<Value> = LazyLock::new(|| {
    let text = std::fs::read_to_string(format!("{FIXTURES}/expected.json")).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
});

/// Synthetic genome findings per profile, in the shape lane D returns (genomics.md 2.3, mcp.md 5.4).
fn findings_for(slug: &str) -> Vec<Value> {
    match slug {
        "alex" => vec![
            json!({"id": "f_1a2b3c4d5e6f7081", "category": "pgx", "gene": "CYP2C19", "diplotype": "*1/*2",
                   "phenotype": "Intermediate Metabolizer", "evidence": "E1", "confidence": "high", "call_state": "alt",
                   "src": "src_g07"}),
            json!({"id": "f_2b3c4d5e6f708192", "category": "pgx", "gene": "SLCO1B1", "diplotype": "*1/*5",
                   "phenotype": "Decreased Function", "evidence": "E1", "confidence": "high", "call_state": "alt",
                   "src": "src_g31"}),
            json!({"id": "f_5e6f708192031425", "category": "risk_marker", "gene": "LPA", "rsid": "rs10455872",
                   "evidence": "E2", "confidence": "medium", "call_state": "alt", "src": "src_g44"}),
        ],
        "sam" => vec![
            json!({"id": "f_9d1e000000000001", "category": "pgx", "gene": "CYP2D6", "diplotype": "*1/*1",
                             "activity_score": "2.0", "phenotype": "Normal Metabolizer", "evidence": "E1",
                             "confidence": "medium", "call_state": "ref", "src": "src_g27"}),
        ],
        _ => Vec::new(),
    }
}

/// (database, profile) -> findings: every test has its own database, so tests may edit theirs.
type Findings = Mutex<HashMap<(String, Uuid), Vec<Value>>>;
static FINDINGS: LazyLock<Findings> = LazyLock::new(Mutex::default);

struct FakeGenome;

async fn db_name(conn: &mut PgConnection) -> Result<String> {
    Ok(sqlx::query_scalar("select current_database()::text")
        .fetch_one(conn)
        .await?)
}

fn findings(db: &str, pid: Uuid) -> Vec<Value> {
    FINDINGS
        .lock()
        .map(|m| m.get(&(db.to_string(), pid)).cloned().unwrap_or_default())
        .unwrap_or_default()
}

impl GenomeSource for FakeGenome {
    fn list_findings<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        _minor: bool,
        _as_of: NaiveDate,
    ) -> GenomeFuture<'a, Value> {
        Box::pin(async move {
            let db = db_name(conn).await?;
            Ok(
                json!({"findings": findings(&db, profile_id), "withheld_sensitive": 0, "withheld_minor": 0, "assay": null}),
            )
        })
    }

    fn pgx_for_drug<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        _cpic_drug_id: &'a str,
        _as_of: NaiveDate,
    ) -> GenomeFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let db = db_name(conn).await?;
            Ok(findings(&db, profile_id)
                .into_iter()
                .filter(|f| f["category"] == "pgx")
                .collect())
        })
    }
}

pub struct Seeded {
    pub db: TestDb,
}

impl Seeded {
    pub fn pool(&self) -> &PgPool {
        &self.db.pool
    }

    pub fn pid(&self, slug: &str) -> Uuid {
        EXPECTED["profiles"][slug]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_default()
    }

    pub fn user(&self, slug: &str) -> Uuid {
        EXPECTED["users"][slug]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_default()
    }

    /// The findings the stand-in answers for one profile of this database (tests may replace them).
    pub fn set_findings(&self, slug: &str, f: Vec<Value>) {
        if let Ok(mut m) = FINDINGS.lock() {
            m.insert((self.db.name.clone(), self.pid(slug)), f);
        }
    }

    pub fn findings(&self, slug: &str) -> Vec<Value> {
        findings(&self.db.name, self.pid(slug))
    }
}

/// Answer genome reads from the stand-in (no findings unless a test sets them).
pub fn install_fake_genome() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| set_genome_source(Some(Arc::new(FakeGenome))));
}

/// A migrated database with the Python seed rows and the stand-in findings.
pub async fn seeded() -> anyhow::Result<Seeded> {
    install_fake_genome();
    let db = TestDb::new().await?;
    let rows = std::fs::read_to_string(format!("{FIXTURES}/seed.sql"))?;
    let script = format!("begin; set local session_replication_role = replica;\n{rows}\ncommit;");
    sqlx::raw_sql(sqlx::AssertSqlSafe(script)).execute(&db.pool).await?;
    let s = Seeded { db };
    for slug in ["alex", "sam", "lucia", "carmen"] {
        s.set_findings(slug, findings_for(slug));
    }
    Ok(s)
}
