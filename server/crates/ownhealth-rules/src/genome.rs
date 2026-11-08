//! The two genome reads lane E makes, through lane D (`ownhealth_genome`). With the `testing`
//! feature, tests can install a stand-in in place of Python's monkeypatched `genome.list_findings` /
//! `genome.pgx_for_drug`; release builds have no seam.

#[cfg(feature = "testing")]
use std::sync::{Arc, RwLock};

use chrono::NaiveDate;
use futures::future::BoxFuture;
use ownhealth_core::Result;
use ownhealth_genome::{FindingsQuery, Gate};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

/// The future a [`GenomeSource`] answers with.
pub type GenomeFuture<'a, T> = BoxFuture<'a, Result<T>>;

/// Where findings come from. The default is lane D; tests install a stand-in.
pub trait GenomeSource: Send + Sync {
    /// The `Findings` JSON: `findings`, `withheld_sensitive`, `withheld_minor`, `assay`.
    fn list_findings<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        minor: bool,
        as_of: NaiveDate,
    ) -> GenomeFuture<'a, Value>;

    fn pgx_for_drug<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        cpic_drug_id: &'a str,
        as_of: NaiveDate,
    ) -> GenomeFuture<'a, Vec<Value>>;
}

#[cfg(feature = "testing")]
static SOURCE: RwLock<Option<Arc<dyn GenomeSource>>> = RwLock::new(None);

/// Test seam: replace lane D's reads (None restores them).
#[cfg(feature = "testing")]
pub fn set_genome_source(source: Option<Arc<dyn GenomeSource>>) {
    if let Ok(mut s) = SOURCE.write() {
        *s = source;
    }
}

#[cfg(feature = "testing")]
fn installed() -> Option<Arc<dyn GenomeSource>> {
    SOURCE.read().ok().and_then(|s| s.clone())
}

/// `genome.list_findings(gate=Gate(include_sensitive=False, minor=minor), min_evidence="E2", as_of=...)`.
pub async fn list_findings(conn: &mut PgConnection, profile_id: Uuid, minor: bool, as_of: NaiveDate) -> Result<Value> {
    #[cfg(feature = "testing")]
    if let Some(s) = installed() {
        return s.list_findings(conn, profile_id, minor, as_of).await;
    }
    let gate = Gate::new(false, minor);
    let q = FindingsQuery {
        min_evidence: Some("E2"),
        as_of: Some(as_of),
        ..FindingsQuery::default()
    };
    ownhealth_genome::list_findings(conn, profile_id, &gate, &q).await
}

pub async fn pgx_for_drug(
    conn: &mut PgConnection,
    profile_id: Uuid,
    cpic_drug_id: &str,
    as_of: NaiveDate,
) -> Result<Vec<Value>> {
    #[cfg(feature = "testing")]
    if let Some(s) = installed() {
        return s.pgx_for_drug(conn, profile_id, cpic_drug_id, as_of).await;
    }
    ownhealth_genome::pgx_for_drug(conn, profile_id, cpic_drug_id, Some(as_of)).await
}
