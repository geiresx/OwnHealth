//! The lane services the tools call (state, genome, normalizer, PDF word layer), behind one trait
//! so tests can put careless stand-ins in their place (the Python tests monkeypatched the same
//! functions). `Lanes` is what ships: it calls lanes A, B, D and E.

use chrono::{DateTime, FixedOffset, NaiveDate};
use futures::future::BoxFuture;
use ownhealth_core::access::Scopes;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::{Error, Result};
use ownhealth_normalize::{Normalized, ResolveQuery, RowIn};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

/// The arguments of `get_genome_findings` lane D sees.
#[derive(Debug, Clone, Default)]
pub struct FindingsArgs {
    pub category: Option<String>,
    pub gene: Option<String>,
    pub min_evidence: String,
    pub as_of: NaiveDate,
}

#[derive(Debug, Clone, Default)]
pub struct VariantArgs {
    pub rsid: Option<String>,
    pub hgvs: Option<String>,
    pub region: Option<String>,
    pub cursor: Option<String>,
}

/// A lookup lane D cannot answer, with a stable code (`GenomeQueryError`: rsid_unknown,
/// hgvs_unresolved, region_too_large, bad_input, no_genome).
#[derive(Debug)]
pub enum LookupError {
    Query {
        code: String,
        message: String,
        fix: Option<String>,
    },
    Other(Error),
}

impl From<Error> for LookupError {
    fn from(e: Error) -> Self {
        LookupError::Other(e)
    }
}

/// One row of `propose` for the normalizer.
#[derive(Debug, Clone)]
pub struct ProposedRow {
    pub profile_id: Uuid,
    pub printed_name: String,
    pub printed_value: String,
    pub printed_unit: Option<String>,
    pub printed_range: Option<String>,
    pub effective_at: DateTime<FixedOffset>,
    pub lang: String,
}

pub trait Services: Send + Sync {
    /// `state.rules_version()`.
    fn rules_version(&self) -> Result<String>;
    /// `{version, sha256, files}` of the rule pack in force (the rules resource).
    fn rule_pack(&self) -> Result<Value>;
    fn build_state<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>>;
    fn render(&self, body: &Value, lang: &str, units: &str, identity_mode: &str) -> Result<Value>;
    fn check_drug<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        drug: &'a str,
        scopes: &'a Scopes,
        lang: &'a str,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>>;
    fn resolve_drug<'a>(&'a self, conn: &'a mut PgConnection, name: &'a str) -> BoxFuture<'a, Result<Option<Value>>>;
    /// The `Findings` of lane D as JSON: findings, not_assessed, withheld_sensitive, withheld_minor,
    /// db_versions, assay.
    fn list_findings<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        gate: &'a ownhealth_genome::Gate,
        q: &'a FindingsArgs,
    ) -> BoxFuture<'a, Result<Value>>;
    fn lookup_variant<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        profile_id: Uuid,
        gate: &'a ownhealth_genome::Gate,
        q: &'a VariantArgs,
    ) -> BoxFuture<'a, Result<Value, LookupError>>;
    /// The alias group a name resolves to, None when it does not (or the normalizer refused it).
    fn resolve_analyte<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        name: &'a str,
        lang: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>>;
    fn normalize_row<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        row: &'a ProposedRow,
    ) -> BoxFuture<'a, Result<Normalized>>;
    #[allow(clippy::too_many_arguments)]
    fn ground_value<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        document_id: Uuid,
        page_no: i32,
        name: &'a str,
        value: &'a str,
        unit: Option<&'a str>,
        near: Option<[f64; 4]>,
    ) -> BoxFuture<'a, Result<Option<[f64; 4]>>>;
    /// The words of one page, in reading order.
    fn page_words<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        document_id: Uuid,
        page_no: i32,
    ) -> BoxFuture<'a, Result<Vec<String>>>;
}

/// The services as they ship.
#[derive(Debug, Clone, Copy, Default)]
pub struct Lanes;

impl Services for Lanes {
    fn rules_version(&self) -> Result<String> {
        ownhealth_rules::rules_version()
    }

    fn rule_pack(&self) -> Result<Value> {
        let pack = ownhealth_rules::load_pack().map_err(|e| Error::Other(anyhow::anyhow!("rule pack: {e}")))?;
        Ok(serde_json::json!({"version": pack.version, "sha256": pack.sha256, "files": pack.files}))
    }

    fn build_state<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(ownhealth_rules::build_state(conn, profile_id, as_of, None))
    }

    fn render(&self, body: &Value, lang: &str, units: &str, identity_mode: &str) -> Result<Value> {
        ownhealth_rules::render(body, lang, units, identity_mode, None)
    }

    fn check_drug<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        drug: &'a str,
        scopes: &'a Scopes,
        lang: &'a str,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(ownhealth_rules::check_drug(
            conn,
            profile_id,
            drug,
            scopes,
            lang,
            Some(as_of),
        ))
    }

    fn resolve_drug<'a>(&'a self, conn: &'a mut PgConnection, name: &'a str) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(ownhealth_rules::resolve_drug(conn, name))
    }

    fn list_findings<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        gate: &'a ownhealth_genome::Gate,
        q: &'a FindingsArgs,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let lane_q = ownhealth_genome::FindingsQuery {
                category: q.category.as_deref(),
                gene: q.gene.as_deref(),
                min_evidence: Some(q.min_evidence.as_str()),
                as_of: Some(q.as_of),
            };
            ownhealth_genome::list_findings(conn, profile_id, gate, &lane_q).await
        })
    }

    fn lookup_variant<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        profile_id: Uuid,
        gate: &'a ownhealth_genome::Gate,
        q: &'a VariantArgs,
    ) -> BoxFuture<'a, Result<Value, LookupError>> {
        Box::pin(async move {
            let lane_q = ownhealth_genome::VariantQuery {
                rsid: q.rsid.as_deref(),
                hgvs: q.hgvs.as_deref(),
                region: q.region.as_deref(),
                cursor: q.cursor.as_deref(),
            };
            match ownhealth_genome::lookup_variant(conn, blobs, profile_id, gate, &lane_q).await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(LookupError::Query {
                    code: e.code.to_string(),
                    message: e.message,
                    fix: e.fix,
                }),
                Err(e) => Err(LookupError::Other(e)),
            }
        })
    }

    fn resolve_analyte<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        name: &'a str,
        lang: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let q = ResolveQuery {
                lang,
                ..Default::default()
            };
            match ownhealth_normalize::resolve_analyte(Some(conn), name, &q).await {
                Ok(r) if r.refused || r.alias_group.is_empty() => Ok(None),
                Ok(r) => Ok(Some(r.alias_group)),
                Err(Error::NotFound(_)) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    fn normalize_row<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        row: &'a ProposedRow,
    ) -> BoxFuture<'a, Result<Normalized>> {
        Box::pin(async move {
            let r = RowIn {
                profile_id: row.profile_id,
                printed_name: &row.printed_name,
                printed_value: &row.printed_value,
                printed_unit: row.printed_unit.as_deref(),
                printed_range: row.printed_range.as_deref(),
                printed_flag: None,
                effective_at: row.effective_at,
                lang: &row.lang,
                specimen: None,
                provider: None,
                loinc: None,
                method: None,
                document_id: None,
            };
            ownhealth_normalize::normalize_row(conn, &r).await
        })
    }

    fn ground_value<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        document_id: Uuid,
        page_no: i32,
        name: &'a str,
        value: &'a str,
        unit: Option<&'a str>,
        near: Option<[f64; 4]>,
    ) -> BoxFuture<'a, Result<Option<[f64; 4]>>> {
        Box::pin(async move {
            let words = ownhealth_ingest_pdf::page_words(conn, blobs, document_id, page_no).await?;
            Ok(crate::grounding::ground_row(&words, name, value, unit, near))
        })
    }

    fn page_words<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        document_id: Uuid,
        page_no: i32,
    ) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let words = ownhealth_ingest_pdf::page_words(conn, blobs, document_id, page_no).await?;
            Ok(words.into_iter().map(|w| w.text).collect())
        })
    }
}
