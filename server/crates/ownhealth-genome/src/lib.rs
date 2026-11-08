//! Lane D: genome (`genome/`, genomics.md). OwnHealth ingests OwnGenome's
//! `ownhealth.genome-facts/v1`; it never calls variants itself. Imports facts files (schema in
//! `data/genome/`, read unchanged), lists findings behind the gate, and reads
//! the uploaded .vcf.gz/.tbi with noodles for `lookup_variant`. Job kind `import.genome_facts`, demo
//! hook `genome` (order 400). The constants are final.

pub mod findings;
pub mod ingest;
pub mod lookup;
pub mod schema;
pub mod sensitive;
pub mod synthetic;
pub mod vcf;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use futures::future::BoxFuture;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{self, DemoContext};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::{Error, Result};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub use findings::{Findings, gate_for, withheld_reason};
pub use lookup::{GenomeQueryError, RefSite, load_ref_sites};
pub use schema::{ChipSites, fact_id};

/// Runs file work (hashing, tabix reads) on a blocking thread, off the runtime HTTP and MCP share.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(std::io::Error::other)?
}

pub const FACTS_SCHEMA: &str = "ownhealth.genome-facts/v1";
/// Exactly five values, everywhere (mcp.md 5.5). A contract test rejects any other.
pub const CALL_STATES: [&str; 5] = ["ref", "alt", "no_call", "not_on_array", "not_covered"];
pub const CATEGORIES: [&str; 9] = [
    "pgx",
    "monogenic_pathogenic",
    "acmg_sf",
    "carrier",
    "hla",
    "trait",
    "risk_marker",
    "prs",
    "ancestry",
];
pub const EVIDENCE: [&str; 4] = ["E1", "E2", "E3", "E4"];
pub const CONFIDENCE: [&str; 4] = ["high", "medium", "low", "unconfirmed"];
pub const NOTICE: &str = "Information for personal records, not a diagnosis.";

/// What a caller may see of a profile's genome on one surface (`genome/model.py` `Gate`).
///
/// `sensitive` is Python's `include_sensitive` (the surface's switch on and read:sensitive, plus
/// read:genome for agents) and `minor` the minor rule; those two decide everything. `surface`
/// ("ui" or "agents"), `genome` and `raw` (read:genome, read:genome_raw) say how `gate_for` built it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Gate {
    pub surface: String,
    pub genome: bool,
    pub raw: bool,
    pub sensitive: bool,
    pub minor: bool,
}

impl Gate {
    /// Python's `Gate(include_sensitive=..., minor=...)`.
    pub fn new(include_sensitive: bool, minor: bool) -> Gate {
        Gate {
            sensitive: include_sensitive,
            minor,
            ..Gate::default()
        }
    }
}

/// JSON Schema errors plus the importer's own rules for a genome-facts document; empty when valid.
/// An unknown `schema` version gives one clear error and nothing else. The chip's site list is not
/// on hand here; `schema::validate_facts` takes one.
pub fn validate_facts(doc: &Value) -> Result<Vec<String>> {
    Ok(schema::validate_facts(doc, ChipSites::Missing))
}

/// Import a stored genome-facts.json; `attachments`: uploaded file name -> blob sha256 (the .vcf.gz
/// and .tbi). An invalid file or a raw file whose sha256 differs is `Error::Invalid` (Python's
/// GenomeImportError) and nothing is written once the caller rolls back.
pub async fn import_genome_facts(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    attachments: &[(String, String)],
    actor_user: Option<Uuid>,
) -> Result<ImportSummary> {
    ingest::import(conn, blobs, document_id, attachments, actor_user, None).await
}

#[derive(Debug, Clone, Default)]
pub struct FindingsQuery<'a> {
    pub category: Option<&'a str>,
    pub gene: Option<&'a str>,
    /// Default E2.
    pub min_evidence: Option<&'a str>,
    pub as_of: Option<NaiveDate>,
}

/// Gated findings with evidence, confidence, call_state and src; coverage first for a gene query.
pub async fn list_findings_typed(
    conn: &mut PgConnection,
    profile_id: Uuid,
    gate: &Gate,
    q: &FindingsQuery<'_>,
) -> Result<Findings> {
    findings::collect_findings(
        conn,
        profile_id,
        gate,
        q.category,
        q.gene,
        q.min_evidence.unwrap_or("E2"),
        q.as_of,
    )
    .await
}

/// The `Findings` JSON: findings, not_assessed, withheld_sensitive, withheld_minor, db_versions,
/// assay, coverage, limitations. A bad `min_evidence` or `category` is `Error::Invalid`.
pub async fn list_findings(
    conn: &mut PgConnection,
    profile_id: Uuid,
    gate: &Gate,
    q: &FindingsQuery<'_>,
) -> Result<Value> {
    Ok(serde_json::to_value(
        list_findings_typed(conn, profile_id, gate, q).await?,
    )?)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VariantQuery<'a> {
    pub rsid: Option<&'a str>,
    pub hgvs: Option<&'a str>,
    pub region: Option<&'a str>,
    pub cursor: Option<&'a str>,
}

/// Calls per source with `call_state` always set; sensitive genes return `withheld_sensitive`. The
/// outer error is a core failure; the inner one a query this cannot answer (`code`: rsid_unknown,
/// hgvs_unresolved, region_too_large, bad_input, no_genome).
pub async fn lookup_variant(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile_id: Uuid,
    gate: &Gate,
    q: &VariantQuery<'_>,
) -> Result<std::result::Result<Value, GenomeQueryError>> {
    lookup::lookup(conn, blobs, profile_id, gate, q).await
}

/// PGx findings relevant to one drug, each with gene, diplotype, phenotype, activity_score,
/// call_state and src; genes CPIC ties to the drug with no finding come back as not_assessed or
/// no_finding. Used by the drug check (lane E). `findings::pgx_for_drug_with` takes a gene list and a
/// drug name too.
pub async fn pgx_for_drug(
    conn: &mut PgConnection,
    profile_id: Uuid,
    cpic_drug_id: &str,
    as_of: Option<NaiveDate>,
) -> Result<Vec<Value>> {
    findings::pgx_for_drug_with(conn, profile_id, cpic_drug_id, as_of, None, None).await
}

/// True for APOE, the ACMG SF cancer and cardiovascular genes, HTT and other predictive repeat loci.
pub fn is_sensitive_gene(gene: &str) -> bool {
    sensitive::is_sensitive_gene(Some(gene))
}

/// Job `import.genome_facts`: a genome-facts.json uploaded with its .vcf.gz and .tbi. The document row
/// and every fact are written in one transaction; a bad file rolls all of it back and becomes a
/// job_failed review item with the reason.
async fn import_job(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let sha = job.str("blob_sha256").unwrap_or_default().to_string();
    let filename = job.str("filename").map(str::to_string);
    if !ctx.blobs.exists(&sha) {
        return Err(JobError::permanent(format!(
            "{}: the uploaded file is not in the blob store",
            filename.as_deref().unwrap_or("None")
        )));
    }
    let household_id = job.uuid("household_id")?;
    let profile_id = job.uuid("profile_id")?;
    let attachments: Vec<(String, String)> = job
        .payload
        .get("files")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let size = ctx.blobs.size(&sha).map_err(Error::from)? as i64;
    let mut tx = ctx.pool.begin().await?;
    let doc = NewDocument {
        filename: filename.as_deref(),
        doc_type: Some("genomics"),
        ..NewDocument::upload(household_id, Some(profile_id), &sha, "application/json", size)
    };
    let r = create_document(&mut tx, &doc).await?;
    let summary = match import_genome_facts(&mut tx, &ctx.blobs, r.id, &attachments, None).await {
        Ok(s) => s,
        Err(Error::Invalid(msg)) => return Err(JobError::permanent(msg)),
        Err(e) => return Err(e.into()),
    };
    tx.commit().await?;
    Ok(Some(summary.to_value()))
}

fn import_job_handler(ctx: JobContext, job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(import_job(ctx, job))
}

/// "ingested 2026-10-02" in the mockup.
fn demo_imported_at() -> Option<DateTime<Utc>> {
    Utc.with_ymd_and_hms(2026, 10, 2, 8, 0, 0).single()
}

/// Demo genome data: Alex's WGS facts with a calls VCF and gVCF, Sam's facts for the codeine
/// example, and Lucía's facts to show the minor rule. All synthetic.
async fn demo_seed(conn: &mut PgConnection, ctx: &mut DemoContext) -> Result<()> {
    let raw = synthetic::alex_raw_files()?;
    let (alex, attachments) = synthetic::with_files(&synthetic::alex_facts(), &ctx.blobs, &raw)?;
    let people = [
        ("alex", alex, attachments),
        ("sam", synthetic::sam_facts(), Vec::new()),
        ("lucia", synthetic::lucia_facts(), Vec::new()),
    ];
    let mut ids = Vec::new();
    for (slug, doc, att) in &people {
        let profile = ctx.family.profile(slug);
        ids.push(profile);
        let sample = doc["subject"]["sample_id"].as_str().unwrap_or_default();
        synthetic::seed_genome(
            &mut *conn,
            &ctx.blobs,
            ctx.family.household_id,
            profile,
            doc,
            att,
            &format!("{sample}_genome_facts.json"),
            demo_imported_at(),
        )
        .await?;
    }
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "select p.slug, f.gene, f.src_id from genome_finding f join profile p on p.id = f.profile_id \
         where f.profile_id = any($1) and f.gene is not null",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    for (slug, gene, src) in rows {
        ctx.src_ids
            .insert(format!("{slug}_genome_{}", gene.to_lowercase().replace('-', "_")), src);
    }
    let alex_src: String = sqlx::query_scalar("select src_id from genome_dataset where profile_id = $1")
        .bind(ctx.family.profile("alex"))
        .fetch_one(&mut *conn)
        .await?;
    ctx.src_ids.insert("alex_genome".into(), alex_src);
    Ok(())
}

fn demo_hook<'c>(conn: &'c mut PgConnection, ctx: &'c mut DemoContext) -> BoxFuture<'c, Result<()>> {
    Box::pin(demo_seed(conn, ctx))
}

/// Register lane D's job handler (`import.genome_facts`) and demo hook (`genome`, order 400).
/// Python has no accept handler for `genome_reinterpretation` items, so neither does this.
pub fn register() {
    jobs::register_handler("import.genome_facts", import_job_handler);
    demo::register_demo_hook("genome", 400, demo_hook);
}
