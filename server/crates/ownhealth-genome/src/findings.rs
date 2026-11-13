//! Reading genome findings: gating, coverage first, evidence filter, PGx for one drug
//! (genomics.md 3, 6, 7; `genome/findings.py`).
//!
//! Gating happens here, in the server, never in a prompt:
//! - sensitive findings (APOE e4, SF cancer and cardiac genes, producer-tagged sensitive or adult-onset)
//!   need `gate.sensitive`; otherwise they are counted in `withheld_sensitive` and not described;
//! - for a minor, sensitive, adult-onset, carrier and PRS findings are counted in `withheld_minor`,
//!   whatever the switches say.
//!
//! Three readings protect the person from a number that no longer holds (v0.3): a PGx call made
//! against an older CPIC change log or PharmCAT minor carries `stale` with the OwnGenome steps to
//! re-run; a PRS outside its validated coverage or ancestry loses its percentile (`prs` says why);
//! a rare P/LP typed on a chip carries `chip_note` with the PPV figure and its source.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use ownhealth_core::access::Permission;
use ownhealth_core::lifecycle::visible_sql;
use ownhealth_core::pyjson::{f32_json, py_isoformat_utc};
use ownhealth_core::{Error, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::types::Json;
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::schema::{CHIP_ASSAYS, UNCONFIRMED_ON_CHIP, py_str};
use crate::{CATEGORIES, CONFIDENCE, EVIDENCE, Gate};

const CPIC_SNAPSHOT: &str = include_str!("../../../../data/rules/cpic/snapshot.json");
const PGX_RELEASES: &str = include_str!("../../../../data/genome/pgx_releases.json");

/// Positive predictive value of a very rare P/LP call on a genotyping array in UK Biobank (about 16%),
/// Weedon et al. 2021, BMJ (PMID 33589468).
pub const CHIP_PPV_PCT: i64 = 16;
pub const CHIP_PPV_CITE: &str = "https://pubmed.ncbi.nlm.nih.gov/33589468/";
/// genomics.md 12: no percentile for a PRS that matched fewer of its variants than this.
pub const PRS_MIN_MATCHED_PCT: f64 = 75.0;
/// The keys of a PRS value that place the person in the distribution.
const PRS_POSITION_KEYS: [&str; 4] = ["percentile", "z_norm1", "z_norm2", "z_score"];

#[allow(clippy::expect_used)]
static NORMAL_PHENOTYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(normal|normal metabolizer|normal function|typical|extensive)").expect("a constant regex")
});

/// The gate for one caller on one profile. `surface` is "ui" (the profile's UI switch; "api" too) or
/// "agents" (the agents switch plus read:genome; "mcp" too, mcp.md 9.7). Both also need read:sensitive.
pub fn gate_for(perm: &Permission, surface: &str, on: NaiveDate, age_of_majority: i32) -> Result<Gate> {
    let p = &perm.profile;
    let (surface, open) = match surface {
        "ui" | "api" => ("ui", p.genome_sensitive_ui && perm.has("read:sensitive")),
        "agents" | "mcp" => (
            "agents",
            p.genome_sensitive_agents && perm.has("read:genome") && perm.has("read:sensitive"),
        ),
        other => return Err(Error::Invalid(format!("unknown surface '{other}'"))),
    };
    Ok(Gate {
        surface: surface.into(),
        genome: perm.has("read:genome"),
        raw: perm.has("read:genome_raw"),
        sensitive: open,
        minor: p.is_minor(on, age_of_majority),
    })
}

/// End of the as_of day (UTC) for the lifecycle predicate.
pub fn as_of_bound(as_of: Option<NaiveDate>) -> Option<DateTime<Utc>> {
    as_of.map(|d| (d + Duration::days(1)).and_time(NaiveTime::MIN).and_utc() - Duration::microseconds(1))
}

/// One `genome_finding` row with its dataset's columns (`visible_rows`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FindingRow {
    pub id: Uuid,
    pub dataset_id: Uuid,
    pub fact_id: String,
    pub category: String,
    pub gene: Option<String>,
    pub rsid: Option<String>,
    pub hgvs: Option<String>,
    pub diplotype: Option<String>,
    pub phenotype: Option<String>,
    pub activity_score: Option<f64>,
    pub zygosity: Option<String>,
    pub evidence: String,
    pub confidence: String,
    pub call_state: Option<String>,
    pub verification_status: String,
    pub sensitive: bool,
    pub adult_onset: bool,
    pub carrier: bool,
    pub status: String,
    pub src_id: String,
    pub fact: Json<Value>,
    pub assay: String,
    pub owngenome_version: String,
    pub db_versions: Json<Value>,
    pub sample_id: String,
    pub imported_at: DateTime<Utc>,
    /// Set by the dedupe: how many sources hold this fact id.
    #[sqlx(skip)]
    pub n_sources: usize,
    /// Set by the dedupe: the CPIC version the quoted guidance comes from.
    #[sqlx(skip)]
    pub guidance_cpic_version: Option<String>,
}

impl FindingRow {
    fn fact_get(&self, key: &str) -> Option<&Value> {
        self.fact.0.get(key).filter(|v| !v.is_null())
    }

    fn cpic_version(&self) -> String {
        match self.db_versions.0.get("cpic") {
            Some(v) if truthy(v) => py_str(Some(v)),
            _ => String::new(),
        }
    }
}

/// Python truthiness of a JSON value.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|v| truthy(v))
}

/// 'minor', 'sensitive' or None for one finding row.
pub fn withheld_reason(row: &FindingRow, gate: &Gate) -> Option<&'static str> {
    if gate.minor && (row.sensitive || row.adult_onset || row.carrier) {
        return Some("minor");
    }
    if row.sensitive && !gate.sensitive {
        return Some("sensitive");
    }
    None
}

/// Every finding row of the profile visible at as_of, with its dataset's columns, ungated.
pub async fn visible_rows(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: Option<NaiveDate>,
) -> Result<Vec<FindingRow>> {
    let sql = format!(
        "select f.id, f.dataset_id, f.fact_id, f.category, f.gene, f.rsid, f.hgvs, f.diplotype, f.phenotype, \
         cast(f.activity_score as float8) as activity_score, f.zygosity, f.evidence, f.confidence, f.call_state, \
         f.verification_status, f.sensitive, f.adult_onset, f.carrier, f.status, f.src_id, f.fact, d.assay, \
         d.owngenome_version, d.db_versions, d.sample_id, d.imported_at from genome_finding f \
         join genome_dataset d on d.id = f.dataset_id where f.profile_id = $1 and d.status <> 'retracted' and {}",
        visible_sql("f", "$2")
    );
    Ok(sqlx::query_as::<_, FindingRow>(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(as_of_bound(as_of))
        .fetch_all(conn)
        .await?)
}

/// One `genome_dataset` row with its raw-file pointers as hex.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DatasetRow {
    pub id: Uuid,
    pub sample_id: String,
    pub assay: String,
    pub build: String,
    pub owngenome_version: String,
    pub db_versions: Json<Value>,
    pub src_id: String,
    pub imported_at: DateTime<Utc>,
    pub assay_detail: Json<Value>,
    pub producer: Json<Value>,
    pub vcf: Option<String>,
    pub tbi: Option<String>,
    pub gvcf: Option<String>,
    pub gvcf_tbi: Option<String>,
    pub sites: Option<String>,
    pub sites_tbi: Option<String>,
}

/// The profile's datasets visible at as_of, newest first.
/// The datasets on file, newest first: current ones (or those known at `as_of`), never one whose
/// genome-facts.json document was retracted.
pub async fn current_datasets(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: Option<NaiveDate>,
) -> Result<Vec<DatasetRow>> {
    Ok(sqlx::query_as::<_, DatasetRow>(
        "select id, sample_id, assay, build, owngenome_version, db_versions, src_id, imported_at, assay_detail, \
         producer, encode(vcf_blob, 'hex') as vcf, encode(tbi_blob, 'hex') as tbi, encode(gvcf_blob, 'hex') as gvcf, \
         encode(gvcf_tbi_blob, 'hex') as gvcf_tbi, encode(sites_blob, 'hex') as sites, \
         encode(sites_tbi_blob, 'hex') as sites_tbi from genome_dataset where profile_id = $1 and \
         not exists (select 1 from document doc where doc.id = genome_dataset.document_id \
         and doc.status = 'retracted') and \
         (case when cast($2 as timestamptz) is null then status = 'current' else imported_at <= \
         cast($2 as timestamptz) and status <> 'retracted' and (superseded_at is null or superseded_at > \
         cast($2 as timestamptz)) end) order by imported_at desc, id",
    )
    .bind(profile_id)
    .bind(as_of_bound(as_of))
    .fetch_all(conn)
    .await?)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CoverageRow {
    pub dataset_id: Uuid,
    pub scope: String,
    pub target: String,
    pub panel: Option<String>,
    pub state: String,
    pub reason: Option<String>,
    pub fraction_callable: Option<f32>,
    pub note: Option<String>,
}

pub async fn coverage_rows(conn: &mut PgConnection, dataset_ids: &[Uuid]) -> Result<Vec<CoverageRow>> {
    if dataset_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as::<_, CoverageRow>(
        "select dataset_id, scope, target, panel, state, reason, fraction_callable, note from genome_coverage \
         where dataset_id = any($1) order by panel nulls last, target",
    )
    .bind(dataset_ids)
    .fetch_all(conn)
    .await?)
}

/// `{"trust": "untrusted_text", "origin": "genome_facts", "text": ...}`, or null.
pub fn untrusted_fact_text(text: Option<&Value>) -> Value {
    match text {
        None | Some(Value::Null) => Value::Null,
        Some(t) => json!({"trust": "untrusted_text", "origin": "genome_facts", "text": t}),
    }
}

/// Python's `round(x, 4)`.
fn round4(x: f64) -> f64 {
    format!("{x:.4}").parse().unwrap_or(x)
}

// ---------------------------------------------------------------- stale ----

/// One CPIC change log row that can change a call or its guidance (gene, recommendation or guideline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpicChange {
    pub date: String,
    pub genes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub released: String,
}

/// What a PGx call is measured against: the CPIC change log (`rules/cpic/snapshot.json`), the latest
/// PharmCAT release and the OwnGenome steps that re-make a call (`genome/pgx_releases.json`). Both are
/// written by `data/rules/cpic/fetch.py`; nothing is fetched at runtime.
#[derive(Debug, Clone, Default)]
pub struct PgxReleases {
    pub cpic_changes: Vec<CpicChange>,
    pub pharmcat: Option<Release>,
    pub rerun_steps: Vec<i64>,
}

static SHIPPED: LazyLock<PgxReleases> = LazyLock::new(|| {
    let snap: Value = serde_json::from_str(CPIC_SNAPSHOT).unwrap_or_default();
    let rel: Value = serde_json::from_str(PGX_RELEASES).unwrap_or_default();
    let rows: &[Value] = rel.get("rows").and_then(Value::as_array).map_or(&[], Vec::as_slice);
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    PgxReleases {
        cpic_changes: snap
            .get("change_log")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| CpicChange {
                date: str_of(c, "date"),
                genes: c
                    .get("genes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_uppercase)
                    .collect(),
            })
            .collect(),
        pharmcat: rows.iter().find(|r| r["id"] == "pharmcat").map(|r| Release {
            version: str_of(r, "latest"),
            released: str_of(r, "released"),
        }),
        rerun_steps: rows
            .iter()
            .filter_map(|r| r.get("step").and_then(Value::as_i64))
            .collect(),
    }
});

/// `YYYY-MM` or `YYYY-MM-DD`.
fn is_cpic_version(v: &str) -> bool {
    let b = v.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    (b.len() == 7 || b.len() == 10)
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && (b.len() == 7 || (b[7] == b'-' && digits(8..10)))
}

/// `3.4.0` or `v3.4` to (3, 4).
fn major_minor(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().trim_start_matches('v').split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

impl PgxReleases {
    pub fn shipped() -> &'static PgxReleases {
        &SHIPPED
    }

    /// `{since, by, rerun_steps}` when a call of `gene` made with these `db_versions` is older than a
    /// CPIC change to that gene or the PharmCAT minor known on `on`; None when current or when the
    /// versions cannot be read (we never guess a call stale).
    pub fn stale(&self, gene: &str, db_versions: &Value, on: NaiveDate) -> Option<Value> {
        let on = on.format("%Y-%m-%d").to_string();
        let gene = gene.to_uppercase();
        let version = |k: &str| db_versions.get(k).and_then(Value::as_str).unwrap_or_default();
        let mut since: Vec<&str> = Vec::new();
        let mut by: Vec<String> = Vec::new();
        let cpic = version("cpic");
        if is_cpic_version(cpic) {
            let newer: Vec<&str> = self
                .cpic_changes
                .iter()
                .filter(|c| c.date.len() >= cpic.len() && c.date.as_str() <= on.as_str())
                .filter(|c| &c.date[..cpic.len()] > cpic && c.genes.contains(&gene))
                .map(|c| c.date.as_str())
                .collect();
            if let (Some(&first), Some(last)) = (newer.iter().min(), newer.iter().max()) {
                since.push(first);
                by.push(format!("CPIC {last}"));
            }
        }
        if let (Some(have), Some(p)) = (major_minor(version("pharmcat")), &self.pharmcat)
            && let Some(latest) = major_minor(&p.version)
            && have < latest
            && !p.released.is_empty()
            && p.released.as_str() <= on.as_str()
        {
            since.push(&p.released);
            by.push(format!("PharmCAT {}", p.version));
        }
        let since = since.into_iter().min()?;
        Some(json!({"since": since, "by": by, "rerun_steps": self.rerun_steps}))
    }
}

/// A PGx call's `stale` on `on` (null when current, or for any other category).
fn stale_of(row: &FindingRow, on: NaiveDate) -> Value {
    if row.category != "pgx" {
        return Value::Null;
    }
    let gene = row.gene.as_deref().unwrap_or_default();
    PgxReleases::shipped()
        .stale(gene, &row.db_versions.0, on)
        .unwrap_or(Value::Null)
}

fn day_of(as_of: Option<NaiveDate>) -> NaiveDate {
    as_of.unwrap_or_else(|| Utc::now().date_naive())
}

// ------------------------------------------------------------ PRS, chip ----

/// The reason a PRS percentile is not interpretable for this person (genomics.md 12), else None.
/// A missing match rate or ancestry counts as outside: we never assume the score applies.
fn prs_refusal(value: &Value) -> Option<&'static str> {
    let matched = value.get("matched_pct").and_then(Value::as_f64);
    if matched.is_none_or(|m| m < PRS_MIN_MATCHED_PCT) {
        return Some("low_variant_match");
    }
    let pop = value.get("most_similar_pop").and_then(Value::as_str);
    let inside = value
        .get("evaluated_in")
        .and_then(Value::as_array)
        .is_some_and(|e| pop.is_some_and(|p| e.iter().any(|x| x.as_str() == Some(p))));
    (!inside).then_some("ancestry_outside_evaluation")
}

/// The note a rare P/LP typed on a chip carries: unconfirmed, with the PPV figure and its source.
fn chip_note(row: &FindingRow) -> Value {
    if CHIP_ASSAYS.contains(&row.assay.as_str()) && UNCONFIRMED_ON_CHIP.contains(&row.category.as_str()) {
        json!({"code": "chip_rare_unconfirmed", "ppv_pct": CHIP_PPV_PCT, "cite": CHIP_PPV_CITE})
    } else {
        Value::Null
    }
}

fn coverage_out(c: &CoverageRow) -> Value {
    let mut out = Map::new();
    out.insert("scope".into(), json!(c.scope));
    out.insert("target".into(), json!(c.target));
    out.insert("panel".into(), json!(c.panel));
    out.insert("state".into(), json!(c.state));
    out.insert("reason".into(), json!(c.reason));
    if let Some(fc) = c.fraction_callable
        && let Some(f) = f32_json(fc).as_f64()
    {
        out.insert("fraction_callable".into(), json!(round4(f)));
    }
    if let Some(note) = c.note.as_deref().filter(|n| !n.is_empty()) {
        out.insert("note".into(), untrusted_fact_text(Some(&json!(note))));
    }
    Value::Object(out)
}

pub(crate) fn coverage_short(c: &CoverageRow) -> Value {
    json!({
        "state": c.state,
        "reason": c.reason,
        "fraction_callable": c.fraction_callable.map_or(Value::Null, f32_json),
    })
}

/// {"gene": "FMR1", "panel": "carrier", "reason": "not_run"}; a panel entry is {"panel": id, "reason"}.
fn not_assessed(c: &CoverageRow) -> Value {
    let reason = c
        .reason
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| "other".into());
    if c.scope == "panel" {
        return json!({"panel": c.target, "reason": reason});
    }
    let mut out = Map::new();
    out.insert(c.scope.clone(), json!(c.target));
    out.insert("panel".into(), json!(c.panel));
    out.insert("reason".into(), json!(reason));
    Value::Object(out)
}

fn title(row: &FindingRow) -> String {
    let value = row.fact_get("value").cloned().unwrap_or_else(|| json!({}));
    let head = row
        .gene
        .clone()
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| row.category.clone());
    let mut parts: Vec<String> = [&row.diplotype, &row.phenotype]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();
    if parts.is_empty()
        && let Some(pgs) = get(&value, "pgs_id")
    {
        let pct = value
            .get("percentile")
            .map(|p| format!("{}th percentile", py_str(Some(p))))
            .unwrap_or_default();
        parts = vec![py_str(Some(pgs)), pct];
    }
    if parts.is_empty()
        && let Some(alleles) = get(&value, "alleles").and_then(Value::as_array)
    {
        parts = vec![alleles.iter().map(|a| py_str(Some(a))).collect::<Vec<_>>().join(", ")];
    }
    if parts.is_empty()
        && let Some(rsid) = row.rsid.as_deref().filter(|r| !r.is_empty())
    {
        parts = vec![rsid.to_string()];
    }
    std::iter::once(head)
        .chain(parts.into_iter().filter(|p| !p.is_empty()))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// One finding as every surface returns it. Free text is wrapped as untrusted_text. A PRS outside
/// its validated coverage or ancestry goes out without its position and without the producer's
/// sentence, which quotes it.
pub fn finding_out(row: &FindingRow) -> Value {
    let refusal = (row.category == "prs")
        .then(|| prs_refusal(row.fact.0.get("value").unwrap_or(&Value::Null)))
        .flatten();
    let gated;
    let row = if refusal.is_some() {
        let mut r = row.clone();
        if let Some(fact) = r.fact.0.as_object_mut() {
            fact.shift_remove("summary");
            if let Some(Value::Object(v)) = fact.get_mut("value") {
                for k in PRS_POSITION_KEYS {
                    v.shift_remove(k);
                }
            }
        }
        gated = r;
        &gated
    } else {
        row
    };
    let prs = if row.category == "prs" {
        json!({"status": (if refusal.is_some() { "not_interpretable" } else { "interpretable" }), "reason_code": refusal})
    } else {
        Value::Null
    };
    let fact = &row.fact.0;
    let ev = fact
        .get("evidence")
        .filter(|v| truthy(v))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let conf = fact
        .get("confidence")
        .filter(|v| truthy(v))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let raw_variants: &[Value] = fact
        .get("variants")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let variants: Vec<Value> = raw_variants
        .iter()
        .map(|v| {
            let mut out = Map::new();
            for k in [
                "chrom",
                "pos",
                "ref",
                "alt",
                "rsid",
                "hgvs_c",
                "hgvs_p",
                "clinvar_variation_id",
            ] {
                if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
                    out.insert(k.into(), x.clone());
                }
            }
            let g = get(v, "genotype").cloned().unwrap_or_else(|| json!({}));
            for k in ["call_state", "gt", "zygosity"] {
                out.insert(k.into(), g.get(k).cloned().unwrap_or(Value::Null));
            }
            Value::Object(out)
        })
        .collect();
    let guidance: Vec<Value> = fact
        .get("drug_guidance")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|g| {
            json!({
                "drug": g.get("drug").cloned().unwrap_or(Value::Null),
                "rxnorm": g.get("rxnorm").cloned().unwrap_or(Value::Null),
                "source": g.get("source").cloned().unwrap_or(Value::Null),
                "level": g.get("level").cloned().unwrap_or(Value::Null),
                "phenotype": g.get("phenotype").cloned().unwrap_or(Value::Null),
                "recommendation": untrusted_fact_text(g.get("recommendation")),
                "url": g.get("url").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let clinvar = raw_variants
        .iter()
        .find(|v| get(v, "clinvar_variation_id").is_some())
        .map_or(Value::Null, |first| {
            json!({
                "variation_id": first.get("clinvar_variation_id").cloned().unwrap_or(Value::Null),
                "stars": ev.get("clinvar_stars").cloned().unwrap_or(Value::Null),
                "native": ev.get("native").cloned().unwrap_or(Value::Null),
            })
        });
    let drugs: Vec<Value> = guidance.iter().map(|g| g["drug"].clone()).collect();
    let limitations: Vec<Value> = fact
        .get("limitations")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|t| untrusted_fact_text(Some(t)))
        .collect();
    json!({
        "id": row.fact_id,
        "category": row.category,
        "gene": row.gene,
        "title": title(row),
        "diplotype": row.diplotype,
        "phenotype": row.phenotype,
        "activity_score": row.activity_score,
        "value": get(fact, "value").cloned().unwrap_or_else(|| json!({})),
        "rsid": row.rsid,
        "hgvs": row.hgvs,
        "zygosity": row.zygosity,
        "variants": variants,
        "clinvar": clinvar,
        "condition": fact.get("condition").cloned().unwrap_or(Value::Null),
        "evidence": row.evidence,
        "evidence_native": ev.get("native").cloned().unwrap_or(Value::Null),
        "confidence": row.confidence,
        "confidence_basis": conf.get("basis").cloned().unwrap_or(Value::Null),
        "call_state": row.call_state,
        "status": row.status,
        "verification": row.verification_status,
        "sensitive": row.sensitive,
        "hidden_by_default": fact.get("hidden_by_default").is_some_and(truthy),
        "confirm_clinically": fact.get("confirm_clinically").is_some_and(truthy),
        "summary": untrusted_fact_text(fact.get("summary")),
        "limitations": limitations,
        "drugs": drugs,
        "drug_guidance": guidance,
        "guidance_cpic_version": row.guidance_cpic_version,
        "related_loinc": get(fact, "related_loinc").cloned().unwrap_or_else(|| json!([])),
        "source": {"assay": row.assay, "tool": row.owngenome_version, "sample_id": row.sample_id},
        "prs": prs,
        "chip_note": chip_note(row),
        "src": row.src_id,
    })
}

fn rank(list: &[&str], v: &str) -> usize {
    list.iter().position(|x| *x == v).unwrap_or(usize::MAX)
}

fn same_phenotype(a: &FindingRow, b: &FindingRow) -> bool {
    a.phenotype.as_deref().unwrap_or_default().to_lowercase()
        == b.phenotype.as_deref().unwrap_or_default().to_lowercase()
}

/// One row per fact id across sources: the higher confidence wins the call, the count of sources is
/// kept. Drug guidance is quoted from the source with the newest CPIC version that carries guidance
/// for the winning phenotype (genomics.md 4), so an older call never brings older guidance with it.
pub fn dedupe(rows: Vec<FindingRow>) -> Vec<FindingRow> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<FindingRow>> = HashMap::new();
    let mut best: HashMap<String, usize> = HashMap::new();
    for r in rows {
        let fid = r.fact_id.clone();
        let group = groups.entry(fid.clone()).or_insert_with(|| {
            order.push(fid.clone());
            Vec::new()
        });
        let key = (rank(&CONFIDENCE, &r.confidence), -r.imported_at.timestamp_micros());
        let better = match best.get(&fid) {
            None => true,
            Some(&i) => {
                let cur = &group[i];
                key < (rank(&CONFIDENCE, &cur.confidence), -cur.imported_at.timestamp_micros())
            }
        };
        group.push(r);
        if better {
            best.insert(fid, group.len() - 1);
        }
    }
    let mut out = Vec::new();
    for fid in order {
        let (Some(group), Some(&bi)) = (groups.get(&fid), best.get(&fid)) else {
            continue;
        };
        let winner = &group[bi];
        let mut r = winner.clone();
        r.n_sources = group.len();
        // newest CPIC version first; on a tie the winning call's own guidance (Python's max keeps the
        // first maximal element)
        let mut source: Option<(usize, (String, bool))> = None;
        for (i, g) in group.iter().enumerate() {
            if !(g.fact.0.get("drug_guidance").is_some_and(truthy) && same_phenotype(g, winner)) {
                continue;
            }
            let k = (g.cpic_version(), i == bi);
            if source.as_ref().is_none_or(|(_, cur)| k > *cur) {
                source = Some((i, k));
            }
        }
        if let Some((si, _)) = &source {
            if *si != bi
                && let (Some(obj), Some(dg)) = (r.fact.0.as_object_mut(), group[*si].fact.0.get("drug_guidance"))
            {
                obj.insert("drug_guidance".into(), dg.clone());
            }
            r.guidance_cpic_version = Some(group[*si].cpic_version()).filter(|v| !v.is_empty());
        } else {
            r.guidance_cpic_version = None;
        }
        out.push(r);
    }
    out
}

fn sort_key(r: &FindingRow) -> (String, usize, String, String) {
    (
        r.evidence.clone(),
        CATEGORIES.iter().position(|c| *c == r.category).unwrap_or(99),
        r.gene.as_deref().unwrap_or_default().to_uppercase(),
        r.fact_id.clone(),
    )
}

pub fn assay_out(ds: &DatasetRow) -> Value {
    let detail = &ds.assay_detail.0;
    let producer = &ds.producer.0;
    let f = |v: &Value, k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "type": ds.assay,
        "platform": f(detail, "platform"),
        "build": ds.build,
        "mean_depth": f(detail, "mean_depth"),
        "callable_fraction": f(detail, "callable_fraction"),
        "source_tool": ds.owngenome_version,
        "generated_utc": f(producer, "generated_utc"),
        "imported_at": py_isoformat_utc(&ds.imported_at),
        "raw_files": {
            "vcf": ds.vcf.is_some(),
            "gvcf": ds.gvcf.is_some(),
            "chip_sites": ds.sites.is_some(),
        },
        "src": ds.src_id,
    })
}

/// The coverage notes of the datasets on file: `(code, assay, English text)`. The code and assay let
/// the UI say the note in its language; MCP sends the English text.
pub fn dataset_notes(datasets: &[DatasetRow]) -> Vec<(&'static str, String, String)> {
    let mut out = Vec::new();
    for ds in datasets {
        let kind = ds.assay.as_str();
        if ds.vcf.is_none() {
            out.push((
                "no_vcf",
                kind.to_string(),
                format!("{kind}: no VCF on file, so variant lookups can only answer sites the findings cite."),
            ));
        } else if ["wgs", "wes", "long_read"].contains(&kind) && ds.gvcf.is_none() {
            out.push((
                "variant_only_vcf",
                kind.to_string(),
                format!("{kind}: variant-only VCF; a site with no record cannot be told from reference (no_call)."),
            ));
        }
        if ["chip", "chip_imputed"].contains(&kind) {
            out.push((
                "chip_unconfirmed",
                kind.to_string(),
                format!(
                    "Chip: rare pathogenic calls are unconfirmed (about {CHIP_PPV_PCT}% of very rare array calls \
                     were real in UK Biobank, PMID 33589468) and a negative is not reassurance."
                ),
            ));
        }
    }
    out.push((
        "no_cram",
        String::new(),
        "CRAM/BAM not on file: structural variant, CYP2D6 and HLA re-calls are not possible here.".into(),
    ));
    out
}

pub fn dataset_limitations(datasets: &[DatasetRow]) -> Vec<String> {
    dataset_notes(datasets).into_iter().map(|(_, _, text)| text).collect()
}

/// What `list_findings` answers (Python's `Findings` dataclass, same field order).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Findings {
    pub findings: Vec<Value>,
    pub not_assessed: Vec<Value>,
    pub withheld_sensitive: i64,
    pub withheld_minor: i64,
    pub db_versions: Value,
    pub assay: Option<Value>,
    pub coverage: Vec<Value>,
    pub limitations: Vec<String>,
    /// `{code, assay}` per limitation, in the same order (the UI's words for them).
    #[serde(skip)]
    pub limitation_codes: Vec<Value>,
    /// Upper-case genes with a withheld finding, so the gene grid never reads them as "no finding".
    #[serde(skip)]
    pub withheld_genes: Vec<String>,
}

/// Gated findings, strongest evidence first. `min_evidence` is the weakest level included (E2 shows
/// E1 and E2; E4 also shows hidden-by-default facts). With `gene`, the coverage entry comes first and
/// a gene with no finding answers its coverage state, never an empty silence.
pub async fn collect_findings(
    conn: &mut PgConnection,
    profile_id: Uuid,
    gate: &Gate,
    category: Option<&str>,
    gene: Option<&str>,
    min_evidence: &str,
    as_of: Option<NaiveDate>,
) -> Result<Findings> {
    if !EVIDENCE.contains(&min_evidence) {
        return Err(Error::Invalid(format!(
            "min_evidence must be one of {}",
            py_tuple(&EVIDENCE)
        )));
    }
    if let Some(c) = category
        && !CATEGORIES.contains(&c)
    {
        return Err(Error::Invalid(format!(
            "category must be one of {}",
            py_tuple(&CATEGORIES)
        )));
    }
    let datasets = current_datasets(&mut *conn, profile_id, as_of).await?;
    let mut result = Findings {
        db_versions: json!({}),
        ..Default::default()
    };
    let Some(newest) = datasets.first() else {
        result.limitations =
            vec!["No genome data on file for this profile. OwnGenome produces genome-facts.json.".into()];
        result.limitation_codes = vec![json!({"code": "no_genome", "assay": null})];
        return Ok(result);
    };
    result.assay = Some(assay_out(newest));
    result.db_versions = match &newest.db_versions.0 {
        Value::Object(m) => Value::Object(m.clone()),
        _ => json!({}),
    };
    let notes = dataset_notes(&datasets);
    result.limitation_codes = notes
        .iter()
        .map(|(code, assay, _)| json!({"code": code, "assay": (!assay.is_empty()).then_some(assay)}))
        .collect();
    result.limitations = notes.into_iter().map(|(_, _, text)| text).collect();
    let want_gene = gene.map(|g| g.trim().to_uppercase()).filter(|g| !g.is_empty());
    let ids: HashSet<Uuid> = datasets.iter().map(|d| d.id).collect();

    let mut shown = Vec::new();
    for r in dedupe(visible_rows(&mut *conn, profile_id, as_of).await?) {
        if !ids.contains(&r.dataset_id) {
            continue;
        }
        if category.is_some_and(|c| r.category != c) {
            continue;
        }
        if let Some(g) = &want_gene
            && r.gene.as_deref().unwrap_or_default().to_uppercase() != *g
        {
            continue;
        }
        let why = withheld_reason(&r, gate);
        if why.is_some()
            && let Some(g) = r.gene.as_deref().map(str::to_uppercase)
            && !result.withheld_genes.contains(&g)
        {
            result.withheld_genes.push(g);
        }
        match why {
            Some("minor") => {
                result.withheld_minor += 1;
                continue;
            }
            Some(_) => {
                result.withheld_sensitive += 1;
                continue;
            }
            None => {}
        }
        if r.evidence.as_str() > min_evidence {
            continue;
        }
        if r.fact.0.get("hidden_by_default").is_some_and(truthy) && min_evidence != "E4" {
            continue;
        }
        shown.push(r);
    }
    shown.sort_by_key(sort_key);
    let on = day_of(as_of);
    result.findings = shown
        .iter()
        .map(|r| {
            let mut f = finding_out(r);
            if let Some(o) = f.as_object_mut() {
                o.insert("n_sources".into(), json!(r.n_sources));
                o.insert("stale".into(), stale_of(r, on));
            }
            f
        })
        .collect();

    let ds_ids: Vec<Uuid> = datasets.iter().map(|d| d.id).collect();
    let cov = coverage_rows(&mut *conn, &ds_ids).await?;
    let newest_cov: Vec<&CoverageRow> = cov.iter().filter(|c| c.dataset_id == newest.id).collect();
    result.not_assessed = newest_cov
        .iter()
        .filter(|c| c.state == "not_assessed")
        .map(|c| not_assessed(c))
        .collect();
    if let Some(g) = &want_gene {
        result.coverage = newest_cov
            .iter()
            .filter(|c| c.target.to_uppercase() == *g)
            .map(|c| coverage_out(c))
            .collect();
        result.not_assessed.retain(|n| {
            n.get("gene")
                .map(|x| py_str(Some(x)))
                .unwrap_or_default()
                .to_uppercase()
                == *g
        });
        if result.coverage.is_empty()
            && result.findings.is_empty()
            && result.withheld_minor == 0
            && result.withheld_sensitive == 0
        {
            result.not_assessed.push(json!({
                "gene": gene.unwrap_or_default().trim(),
                "panel": null,
                "reason": "not_in_coverage_map",
            }));
        }
    } else {
        result.coverage = newest_cov.iter().map(|c| coverage_out(c)).collect();
    }
    Ok(result)
}

/// Python's repr of a tuple of strings: `('E1', 'E2', 'E3', 'E4')`.
fn py_tuple(items: &[&str]) -> String {
    format!(
        "({})",
        items.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ")
    )
}

// ----------------------------------------------------------------- drug ----

struct CpicDrug {
    name: Option<String>,
    genes: Vec<String>,
}

/// CPIC drug id -> {name, genes} from the CPIC API snapshot lane E ships (CC0).
static CPIC_PAIRS: LazyLock<BTreeMap<String, CpicDrug>> = LazyLock::new(|| {
    let mut out: BTreeMap<String, CpicDrug> = BTreeMap::new();
    let Ok(doc) = serde_json::from_str::<Value>(CPIC_SNAPSHOT) else {
        return out;
    };
    for p in doc.get("pairs").and_then(Value::as_array).into_iter().flatten() {
        let (Some(drug), Some(gene)) = (
            p.get("drugid").and_then(Value::as_str),
            p.get("genesymbol").and_then(Value::as_str),
        ) else {
            continue;
        };
        let entry = out.entry(drug.to_string()).or_insert_with(|| CpicDrug {
            name: p.get("drugname").and_then(Value::as_str).map(str::to_string),
            genes: Vec::new(),
        });
        if !entry.genes.iter().any(|g| g == gene) {
            entry.genes.push(gene.to_string());
        }
    }
    out
});

pub fn drug_genes(cpic_drug_id: &str) -> (Option<String>, Vec<String>) {
    CPIC_PAIRS
        .get(cpic_drug_id)
        .map_or((None, Vec::new()), |e| (e.name.clone(), e.genes.clone()))
}

fn guidance_matches(g: &Value, cpic_drug_id: &str, name: Option<&str>) -> bool {
    let rx = cpic_drug_id.split_once(':').map(|(_, r)| r).filter(|r| !r.is_empty());
    let rxnorm = g.get("rxnorm").and_then(Value::as_str);
    let drug = g
        .get("drug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    rx.is_some_and(|rx| rxnorm.is_some_and(|r| r == rx || r == cpic_drug_id))
        || name.is_some_and(|n| !n.is_empty() && drug == n.to_lowercase())
        || drug == cpic_drug_id.to_lowercase()
}

/// A result that changes the guidance: a positive HLA allele; a phenotype (the finding's, else the
/// matched guidance's) that is not normal; with neither, a non-reference call.
fn actionable(f: &Value, guidance: &[Value]) -> bool {
    if f["category"] == "hla" {
        return f["value"].get("present") == Some(&Value::Bool(true));
    }
    let phenotype = f["phenotype"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| {
            guidance
                .iter()
                .find_map(|g| g.get("phenotype").and_then(Value::as_str).filter(|p| !p.is_empty()))
                .map(str::to_string)
        });
    match phenotype {
        Some(p) => !NORMAL_PHENOTYPE.is_match(&p),
        None => f["call_state"] == "alt",
    }
}

/// PGx (and HLA) findings relevant to one drug: one entry per gene CPIC ties to the drug (from
/// `genes`, else the CPIC snapshot pairs) plus any gene whose facts carry guidance for it.
/// Sensitive findings are never returned here.
pub async fn pgx_for_drug_with(
    conn: &mut PgConnection,
    profile_id: Uuid,
    cpic_drug_id: &str,
    as_of: Option<NaiveDate>,
    genes: Option<&[String]>,
    drug_name: Option<&str>,
) -> Result<Vec<Value>> {
    let (snap_name, snap_genes) = drug_genes(cpic_drug_id);
    let name = drug_name.map(str::to_string).or(snap_name);
    let mut wanted: Vec<String> = genes
        .map_or(snap_genes, <[String]>::to_vec)
        .iter()
        .map(|g| g.to_uppercase())
        .collect();
    let datasets = current_datasets(&mut *conn, profile_id, as_of).await?;
    let ids: HashSet<Uuid> = datasets.iter().map(|d| d.id).collect();
    let rows: Vec<FindingRow> = dedupe(visible_rows(&mut *conn, profile_id, as_of).await?)
        .into_iter()
        .filter(|r| ids.contains(&r.dataset_id) && (r.category == "pgx" || r.category == "hla") && !r.sensitive)
        .collect();
    for r in &rows {
        let guidance = r.fact.0.get("drug_guidance").and_then(Value::as_array);
        if guidance
            .into_iter()
            .flatten()
            .any(|g| guidance_matches(g, cpic_drug_id, name.as_deref()))
        {
            let gene = r.gene.as_deref().unwrap_or_default().to_uppercase();
            if !gene.is_empty() && !wanted.contains(&gene) {
                wanted.push(gene);
            }
        }
    }
    let newest: Vec<Uuid> = datasets.first().map(|d| d.id).into_iter().collect();
    let mut cov: HashMap<String, CoverageRow> = HashMap::new();
    for c in coverage_rows(&mut *conn, &newest).await? {
        cov.insert(c.target.to_uppercase(), c);
    }
    let mut out = Vec::new();
    for gene in wanted {
        let mut hits: Vec<&FindingRow> = rows
            .iter()
            .filter(|r| r.gene.as_deref().unwrap_or_default().to_uppercase() == gene)
            .collect();
        hits.sort_by_key(|r| sort_key(r));
        let c = cov.get(&gene);
        let coverage = c.map_or(Value::Null, |c| json!({"state": c.state, "reason": c.reason}));
        if hits.is_empty() {
            let status = if c.is_some_and(|c| c.state != "not_assessed") {
                "no_finding"
            } else {
                "not_assessed"
            };
            out.push(json!({"gene": gene, "status": status, "coverage": coverage, "call_state": null, "guidance": []}));
            continue;
        }
        for r in hits {
            let f = finding_out(r);
            let guidance: Vec<Value> = f["drug_guidance"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|g| guidance_matches(g, cpic_drug_id, name.as_deref()))
                .cloned()
                .collect();
            out.push(json!({
                "gene": r.gene,
                "status": "found",
                "fact_id": f["id"],
                "category": f["category"],
                "diplotype": f["diplotype"],
                "phenotype": f["phenotype"],
                "activity_score": f["activity_score"],
                "value": f["value"],
                "evidence": f["evidence"],
                "evidence_native": f["evidence_native"],
                "confidence": f["confidence"],
                "confidence_basis": f["confidence_basis"],
                "call_state": f["call_state"],
                "actionable": actionable(&f, &guidance),
                "guidance": guidance,
                "guidance_cpic_version": f["guidance_cpic_version"],
                "coverage": coverage,
                "stale": stale_of(r, day_of(as_of)),
                "src": f["src"],
            }));
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------- grid ----

/// The genes CPIC ties to a drug in the shipped snapshot, upper-case and sorted.
pub fn cpic_genes() -> Vec<String> {
    let set: std::collections::BTreeSet<String> = CPIC_PAIRS
        .values()
        .flat_map(|d| d.genes.iter().map(|g| g.to_uppercase()))
        .collect();
    set.into_iter().collect()
}

/// The genes CPIC ties to a drug, by the drug's CPIC name (`atorvastatin`).
pub fn genes_for_drug_name(name: &str) -> Vec<String> {
    let name = name.trim().to_lowercase();
    CPIC_PAIRS
        .values()
        .find(|d| d.name.as_deref().is_some_and(|n| n.to_lowercase() == name))
        .map(|d| d.genes.iter().map(|g| g.to_uppercase()).collect())
        .unwrap_or_default()
}

/// One row per CPIC gene (the snapshot's, the coverage map's `cpic` panel, and any other gene with a
/// PGx or HLA finding), alphabetical: the call, or
/// why there is none (`no_finding` on an assessed gene, `not_assessed` with the coverage reason or
/// `not_in_coverage_map`, `withheld`), whether it is stale, and the current medicines it touches.
/// `meds` is gene -> medicines from the person's records; None when the reader may not see records.
pub fn pgx_grid(found: &Findings, meds: Option<&BTreeMap<String, Vec<Value>>>) -> Vec<Value> {
    let upper = |v: &Value| v.as_str().unwrap_or_default().to_uppercase();
    let calls: Vec<&Value> = found
        .findings
        .iter()
        .filter(|f| f["category"] == "pgx" || f["category"] == "hla")
        .collect();
    let mut genes = cpic_genes();
    let cpic_panel = found
        .coverage
        .iter()
        .filter(|c| c["scope"] == "gene" && c["panel"] == "cpic")
        .map(|c| &c["target"]);
    for g in calls.iter().map(|f| &f["gene"]).chain(cpic_panel).map(upper) {
        if !g.is_empty() && !genes.contains(&g) {
            genes.push(g);
        }
    }
    genes.sort();
    genes
        .into_iter()
        .map(|gene| {
            let hits: Vec<&&Value> = calls.iter().filter(|f| upper(&f["gene"]) == gene).collect();
            let cov = found.coverage.iter().find(|c| upper(&c["target"]) == gene);
            let coverage = match cov {
                Some(c) => json!({"state": c["state"], "reason": c["reason"]}),
                None => json!({"state": "not_assessed", "reason": "not_in_coverage_map"}),
            };
            let meds = meds.map_or(Value::Null, |m| json!(m.get(&gene).cloned().unwrap_or_default()));
            let mut row = json!({
                "gene": gene, "status": "", "diplotype": null, "phenotype": null, "activity_score": null,
                "call_state": null, "markers": [], "evidence": null, "confidence": null, "actionable": false,
                "stale": null, "coverage": coverage, "meds": meds, "fact_ids": [], "src": null,
            });
            let status = if let Some(first) = hits.iter().find(|f| f["category"] == "pgx").or(hits.first()) {
                for k in [
                    "diplotype",
                    "phenotype",
                    "activity_score",
                    "call_state",
                    "evidence",
                    "confidence",
                    "stale",
                    "src",
                ] {
                    row[k] = first[k].clone();
                }
                if first["category"] == "hla" && first["diplotype"].is_null() {
                    row["diplotype"] = first["value"]["alleles"].as_array().map_or(Value::Null, |a| {
                        json!(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("/"))
                    });
                }
                row["markers"] = json!(
                    hits.iter()
                        .filter(|f| f["category"] == "hla" && f["value"]["marker"].is_string())
                        .map(|f| json!({"marker": f["value"]["marker"], "present": f["value"]["present"]}))
                        .collect::<Vec<_>>()
                );
                row["actionable"] = json!(hits.iter().any(|f| actionable(f, &[])));
                row["fact_ids"] = json!(hits.iter().map(|f| f["id"].clone()).collect::<Vec<_>>());
                "found"
            } else if found.withheld_genes.contains(&gene) {
                row["coverage"] = Value::Null;
                "withheld"
            } else if cov.is_some_and(|c| c["state"] != "not_assessed") {
                "no_finding"
            } else {
                "not_assessed"
            };
            row["status"] = json!(status);
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_of_is_the_end_of_the_day() {
        let b = as_of_bound(NaiveDate::from_ymd_opt(2026, 10, 2));
        assert!(py_isoformat_utc(&b.unwrap_or_default()).starts_with("2026-10-02T23:59:59"));
    }

    #[test]
    fn cpic_pairs_load() {
        let (name, genes) = drug_genes("RxNorm:32968");
        assert_eq!(name.as_deref(), Some("clopidogrel"));
        assert!(genes.iter().any(|g| g == "CYP2C19"));
    }

    #[test]
    fn rounding() {
        assert_eq!(round4(0.9990000128746033), 0.999);
        assert_eq!(round4(0.81), 0.81);
    }
}
