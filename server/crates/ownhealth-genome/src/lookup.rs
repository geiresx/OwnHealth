//! What the person's genome says at one position (genomics.md 3, 5; mcp.md 5.5; `genome/lookup.py`).
//!
//! Every answer carries a `call_state` per source, one of exactly `CALL_STATES`:
//!
//! | state        | WGS / WES                                              | chip                               |
//! |--------------|--------------------------------------------------------|------------------------------------|
//! | alt / ref    | genotype in the calls VCF, or a gVCF block passing GQ  | genotype in the chip VCF           |
//! | no_call      | `./.`; or a variant-only VCF with no record (reason)   | probed (site list) but not called  |
//! | not_on_array | never                                                  | not probed by this array           |
//! | not_covered  | no gVCF block, a block below GQ/DP, or a not_assessed  | never                              |
//! |              | gene in the coverage map                               |                                    |
//!
//! The VCFs are read from the blob store with noodles, indexes passed explicitly (blob names are
//! hashes). Sensitive genes are gated here too, so a raw lookup cannot bypass the opt-in (mcp.md 9.7).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::LazyLock;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::pyjson::py_isoformat_utc;
use ownhealth_core::{Error, Result};
use regex::Regex;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::findings::{
    CoverageRow, DatasetRow, FindingRow, coverage_rows, coverage_short, current_datasets, truthy, visible_rows,
    withheld_reason,
};
use crate::schema::{int_of, py_repr};
use crate::sensitive::{GeneSpan, carrier_spans_overlapping, norm_chrom, spans_overlapping};
use crate::vcf::{Record, VcfFile};
use crate::{CALL_STATES, Gate, VariantQuery, blocking};

pub const MAX_REGION_BP: i64 = 1_000_000;
pub const PAGE: usize = 50;
pub const MIN_GQ: f64 = 20.0;
pub const MIN_DP: f64 = 8.0;
pub const UNKNOWN_NOTE: &str = "not_on_array and not_covered mean unknown, not negative.";
const CHIP: [&str; 2] = ["chip", "chip_imputed"];
const BLOCK_ALTS: [&str; 2] = ["<NON_REF>", "<*>"];

#[allow(clippy::expect_used)]
fn re(p: &str) -> Regex {
    Regex::new(p).expect("a constant regex")
}

// Python's `$` also matches before one final newline; `\n?\z` keeps that.
static RSID: LazyLock<Regex> = LazyLock::new(|| re(r"^rs\d+\n?\z"));
static REGION: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^(chr[0-9XYM]+):(\d+)-(\d+)\n?\z"));
static HGVS_G: LazyLock<Regex> = LazyLock::new(|| re(r"^NC_0*(\d+)\.\d+:g\.(\d+)([ACGT])>([ACGT])\n?\z"));

/// A lookup that cannot be answered: `code` is stable (rsid_unknown, hgvs_unresolved,
/// region_too_large, bad_input, no_genome).
#[derive(Debug, Clone, PartialEq)]
pub struct GenomeQueryError {
    pub code: &'static str,
    pub message: String,
    pub fix: Option<String>,
}

impl fmt::Display for GenomeQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GenomeQueryError {}

impl GenomeQueryError {
    pub fn new(code: &'static str, message: impl Into<String>, fix: Option<&str>) -> GenomeQueryError {
        GenomeQueryError {
            code,
            message: message.into(),
            fix: fix.map(str::to_string),
        }
    }

    /// `{"code", "message", "fix"}` as the API and MCP answer it.
    pub fn as_value(&self) -> Value {
        json!({"code": self.code, "message": self.message, "fix": self.fix})
    }
}

/// Core errors outside, query errors inside: `lookup(...).await?` then match the inner result.
pub type Answer = std::result::Result<Value, GenomeQueryError>;

fn bad(message: impl Into<String>) -> GenomeQueryError {
    GenomeQueryError::new("bad_input", message, None)
}

fn contig_names(chrom: &str) -> Vec<String> {
    let c = norm_chrom(chrom);
    let bare = c[3..].to_string();
    let mut out = vec![c, bare.clone()];
    if bare == "M" {
        out.push("MT".into());
        out.push("chrMT".into());
    }
    out
}

fn open(blobs: &BlobStore, data: &str, index: &str) -> Result<VcfFile> {
    Ok(VcfFile::open(&blobs.path(data)?, &blobs.path(index)?)?)
}

fn fetch(vf: &mut VcfFile, chrom: &str, start: i64, end: i64) -> Result<Vec<Record>> {
    Ok(vf.fetch(&contig_names(chrom), start, end)?)
}

fn is_block(rec: &Record) -> bool {
    rec.alts.is_empty() || rec.alts.iter().all(|a| BLOCK_ALTS.contains(&a.as_str()))
}

/// A FORMAT value as pysam typed it: an integer, else a float, else null.
fn sample_number(rec: &Record, key: &str) -> Value {
    let Some(raw) = rec.sample_value(key) else {
        return Value::Null;
    };
    let first = raw.split(',').next().unwrap_or_default();
    if let Ok(i) = first.parse::<i64>() {
        return json!(i);
    }
    first
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .map_or(Value::Null, |f| json!(f))
}

fn insert(map: &mut Map<String, Value>, key: &str, value: Value) {
    map.insert(key.into(), value);
}

/// call_state, genotype and depth from the first sample of one VCF record.
fn from_record(rec: &Record, basis: &str) -> Map<String, Value> {
    let mut out = Map::new();
    if rec.sample.is_none() {
        insert(&mut out, "call_state", json!("no_call"));
        insert(&mut out, "basis", json!(basis));
        insert(&mut out, "reason", json!("no_sample_in_vcf"));
        return out;
    }
    let (gt, phased) = rec.genotype();
    let state = if gt.iter().any(Option::is_none) {
        "no_call"
    } else if gt.iter().all(|a| *a == Some(0)) {
        "ref"
    } else {
        "alt"
    };
    let alleles = rec.alleles();
    let sep = if phased { "|" } else { "/" };
    let genotype = gt
        .iter()
        .map(|a| match a {
            Some(i) if *i < alleles.len() => alleles[*i],
            _ => ".",
        })
        .collect::<Vec<_>>()
        .join(sep);
    let zygosity = if state != "alt" {
        if state == "ref" { "ref" } else { "unknown" }
    } else if gt.len() == 1 {
        "hemi"
    } else if gt.iter().collect::<HashSet<_>>().len() == 1 {
        "hom"
    } else {
        "het"
    };
    let gt_text = gt
        .iter()
        .map(|a| a.map_or_else(|| ".".to_string(), |i| i.to_string()))
        .collect::<Vec<_>>()
        .join(sep);
    insert(&mut out, "call_state", json!(state));
    insert(&mut out, "genotype", json!(genotype));
    insert(&mut out, "gt", json!(gt_text));
    insert(&mut out, "zygosity", json!(zygosity));
    insert(&mut out, "dp", sample_number(rec, "DP"));
    insert(&mut out, "gq", sample_number(rec, "GQ"));
    insert(&mut out, "basis", json!(basis));
    out
}

fn pick<'a>(records: &'a [Record], pos: i64, alt: Option<&str>) -> Option<&'a Record> {
    let at: Vec<&Record> = records.iter().filter(|r| r.pos == pos && !is_block(r)).collect();
    if let Some(alt) = alt.filter(|a| !a.is_empty())
        && let Some(exact) = at.iter().find(|r| r.alts.iter().any(|a| a == alt))
    {
        return Some(exact);
    }
    at.first().copied()
}

/// A resolved site.
#[derive(Debug, Clone, Default)]
struct Site {
    chrom: String,
    pos: i64,
    reference: Value,
    alt: Value,
    rsid: Value,
    hgvs_c: Value,
    hgvs_p: Value,
    gene: Option<String>,
}

impl Site {
    fn alt_str(&self) -> Option<&str> {
        self.alt.as_str()
    }

    fn variant(&self, gene: Option<&str>) -> Value {
        json!({
            "chrom": self.chrom,
            "pos": self.pos,
            "ref": self.reference,
            "alt": self.alt,
            "rsid": self.rsid,
            "hgvs_c": self.hgvs_c,
            "hgvs_p": self.hgvs_p,
            "gene": gene,
        })
    }
}

fn status(state: &str, basis: &str, reason: &str) -> Map<String, Value> {
    let mut out = Map::new();
    insert(&mut out, "call_state", json!(state));
    insert(&mut out, "basis", json!(basis));
    insert(&mut out, "reason", json!(reason));
    out
}

fn as_f64(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// One source's answer at one site. Always returns a call_state in CALL_STATES.
fn call_at(
    blobs: &BlobStore,
    ds: &DatasetRow,
    site: &Site,
    cited: Option<&Value>,
    cov: Option<&CoverageRow>,
) -> Result<Map<String, Value>> {
    let (chrom, pos, alt) = (site.chrom.as_str(), site.pos, site.alt_str());
    let (Some(vcf), Some(tbi)) = (ds.vcf.as_deref(), ds.tbi.as_deref()) else {
        if let Some(v) = cited {
            let g = v.get("genotype").cloned().unwrap_or_else(|| json!({}));
            let mut out = Map::new();
            for k in ["gt", "zygosity", "dp", "gq"] {
                insert(&mut out, k, g.get(k).cloned().unwrap_or(Value::Null));
            }
            insert(
                &mut out,
                "call_state",
                g.get("call_state").cloned().unwrap_or(Value::Null),
            );
            insert(&mut out, "basis", json!("facts_file"));
            return Ok(out);
        }
        return Ok(status("not_covered", "absent", "raw_genome_not_on_file"));
    };
    let calls = fetch(&mut open(blobs, vcf, tbi)?, chrom, pos, pos)?;
    if let Some(rec) = pick(&calls, pos, alt) {
        return Ok(from_record(rec, "vcf"));
    }
    if CHIP.contains(&ds.assay.as_str()) {
        let (Some(sites), Some(sites_tbi)) = (ds.sites.as_deref(), ds.sites_tbi.as_deref()) else {
            return Ok(status("not_on_array", "absent", "not_in_array_calls_no_site_list"));
        };
        let probed = fetch(&mut open(blobs, sites, sites_tbi)?, chrom, pos, pos)?;
        if probed.iter().any(|r| r.pos == pos) {
            return Ok(status("no_call", "chip_sites", "probed_not_called"));
        }
        return Ok(status("not_on_array", "chip_sites", "not_probed"));
    }
    if let Some(c) = cov
        && c.state == "not_assessed"
    {
        let reason = c.reason.as_deref().filter(|r| !r.is_empty()).unwrap_or("not_assessed");
        return Ok(status("not_covered", "coverage_map", reason));
    }
    let (Some(gvcf), Some(gvcf_tbi)) = (ds.gvcf.as_deref(), ds.gvcf_tbi.as_deref()) else {
        return Ok(status("no_call", "absent", "variant_only_vcf_absence_unknown"));
    };
    let records = fetch(&mut open(blobs, gvcf, gvcf_tbi)?, chrom, pos, pos)?;
    if let Some(rec) = pick(&records, pos, alt) {
        return Ok(from_record(rec, "gvcf"));
    }
    let Some(block) = records.iter().find(|r| is_block(r) && r.pos <= pos && pos <= r.end) else {
        return Ok(status("not_covered", "gvcf_block", "no_reference_block"));
    };
    let mut out = from_record(block, "gvcf_block");
    let dp = if block.sample.is_some() {
        let min_dp = sample_number(block, "MIN_DP");
        if truthy(&min_dp) {
            min_dp
        } else {
            sample_number(block, "DP")
        }
    } else {
        Value::Null
    };
    let is_ref = out.get("call_state") == Some(&json!("ref"));
    let gq = as_f64(out.get("gq")).unwrap_or(0.0);
    let low_dp = dp.as_f64().is_some_and(|d| d < MIN_DP);
    if is_ref && (gq < MIN_GQ || low_dp) {
        insert(&mut out, "call_state", json!("not_covered"));
        insert(&mut out, "reason", json!("block_below_quality"));
        insert(&mut out, "dp", dp);
        return Ok(out);
    }
    if is_ref {
        let r = site.reference.as_str().filter(|r| !r.is_empty()).unwrap_or("N");
        insert(&mut out, "genotype", json!(format!("{r}/{r}")));
    }
    insert(&mut out, "dp", dp);
    Ok(out)
}

fn source_label(ds: &DatasetRow) -> String {
    let generated = ds
        .producer
        .0
        .get("generated_utc")
        .and_then(Value::as_str)
        .filter(|g| !g.is_empty())
        .map_or_else(|| py_isoformat_utc(&ds.imported_at), str::to_string);
    format!("{}-{}", ds.assay, generated.chars().take(10).collect::<String>())
}

fn variants(r: &FindingRow) -> &[Value] {
    r.fact
        .0
        .get("variants")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// chrom, pos, ref, alts, gene of a `genome_ref_site` row.
type RefRow = (String, i32, String, Vec<String>, Option<String>);

/// A site: the person's own facts first, then the curated rsID index. Never a remote lookup.
async fn resolve(
    conn: &mut PgConnection,
    rows: &[FindingRow],
    rsid: Option<&str>,
    hgvs: Option<&str>,
) -> Result<std::result::Result<Site, GenomeQueryError>> {
    for r in rows {
        for v in variants(r) {
            let rs_hit = rsid.is_some_and(|q| !q.is_empty() && v.get("rsid").and_then(Value::as_str) == Some(q));
            let hgvs_hit = hgvs.is_some_and(|q| {
                !q.is_empty()
                    && (v.get("hgvs_c").and_then(Value::as_str) == Some(q)
                        || v.get("hgvs_p").and_then(Value::as_str) == Some(q))
            });
            if rs_hit || hgvs_hit {
                let f = |k: &str| v.get(k).cloned().unwrap_or(Value::Null);
                return Ok(Ok(Site {
                    chrom: norm_chrom(v.get("chrom").and_then(Value::as_str).unwrap_or_default()),
                    pos: int_of(v.get("pos")),
                    reference: f("ref"),
                    alt: f("alt"),
                    rsid: f("rsid"),
                    hgvs_c: f("hgvs_c"),
                    hgvs_p: f("hgvs_p"),
                    gene: r.gene.clone(),
                }));
            }
        }
    }
    if let Some(rsid) = rsid.filter(|r| !r.is_empty()) {
        let row: Option<RefRow> = sqlx::query_as(
            "select chrom, pos, ref, alts, gene from genome_ref_site where rsid = $1 order by chrom, pos limit 1",
        )
        .bind(rsid)
        .fetch_optional(conn)
        .await?;
        let Some((chrom, pos, reference, alts, gene)) = row else {
            return Ok(Err(GenomeQueryError::new(
                "rsid_unknown",
                format!("{rsid} is not in the curated rsID index, and remote lookups are off (they leak interest)."),
                Some("Ask by position instead: region chrN:pos-pos (GRCh38)."),
            )));
        };
        return Ok(Ok(Site {
            chrom: norm_chrom(&chrom),
            pos: i64::from(pos),
            reference: json!(reference),
            alt: if alts.len() == 1 { json!(alts[0]) } else { Value::Null },
            rsid: json!(rsid),
            gene,
            ..Default::default()
        }));
    }
    if let Some(m) = HGVS_G.captures(hgvs.unwrap_or_default()) {
        let n: Option<u64> = m[1].parse().ok();
        let chrom = match n {
            Some(23) => Some("chrX".to_string()),
            Some(24) => Some("chrY".to_string()),
            Some(12920) => Some("chrM".to_string()),
            Some(n) if (1..=22).contains(&n) => Some(format!("chr{n}")),
            _ => None,
        };
        if let (Some(chrom), Ok(pos)) = (chrom, m[2].parse::<i64>()) {
            return Ok(Ok(Site {
                chrom,
                pos,
                reference: json!(&m[3]),
                alt: json!(&m[4]),
                ..Default::default()
            }));
        }
    }
    Ok(Err(GenomeQueryError::new(
        "hgvs_unresolved",
        "Only GRCh38 genomic HGVS (NC_...:g.<pos><ref>>(alt)) or HGVS your own findings cite resolves in v0.1.",
        Some("Ask by rsid or by region chrN:pos-pos."),
    )))
}

fn span_reason(gate: &Gate) -> Option<&'static str> {
    if gate.minor {
        return Some("minor");
    }
    (!gate.sensitive).then_some("sensitive")
}

/// The fixed spans this gate withholds around [start, end]: the sensitive genes, and for a minor the
/// carrier-panel genes too. Never the person's own findings (genomics.md 6, 7).
fn gated_spans(gate: &Gate, chrom: &str, start: i64, end: i64) -> Vec<&'static GeneSpan> {
    let Some(why) = span_reason(gate) else {
        return Vec::new();
    };
    let mut hits = spans_overlapping(chrom, start, end);
    if why == "minor" {
        hits.extend(carrier_spans_overlapping(chrom, start, end));
    }
    hits
}

/// The contract: exactly five values, always.
pub(crate) fn check_call_state(call: &Map<String, Value>) -> Result<()> {
    let state = call.get("call_state").and_then(Value::as_str).unwrap_or_default();
    if CALL_STATES.contains(&state) {
        return Ok(());
    }
    // Python raised RuntimeError: an internal error, HTTP 500
    Err(Error::Io(std::io::Error::other(format!(
        "call_state outside the contract: {}",
        call.get("call_state")
            .map_or_else(|| "None".to_string(), Value::to_string)
    ))))
}

/// Calls per source with `call_state` always set; sensitive genes return `withheld_sensitive` (or
/// `withheld_minor`) and no genotype. Reads only; the caller writes the audit row.
pub async fn lookup(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile_id: Uuid,
    gate: &Gate,
    q: &VariantQuery<'_>,
) -> Result<Answer> {
    let VariantQuery {
        rsid,
        hgvs,
        region,
        cursor,
    } = *q;
    if [rsid, hgvs, region].iter().filter(|x| x.is_some()).count() != 1 {
        return Ok(Err(bad("Give exactly one of rsid, hgvs or region.")));
    }
    if let Some(r) = rsid
        && !RSID.is_match(r)
    {
        return Ok(Err(bad(format!(
            "{} is not an rsID (rs followed by digits).",
            py_repr(r)
        ))));
    }
    let datasets = current_datasets(&mut *conn, profile_id, None).await?;
    if datasets.is_empty() {
        return Ok(Err(GenomeQueryError::new(
            "no_genome",
            "No genome data on file for this profile.",
            Some("Import OwnGenome output first."),
        )));
    }
    let current: HashSet<Uuid> = datasets.iter().map(|d| d.id).collect();
    // findings the gate withholds never shape an answer: not the site resolution, not cited_by_findings
    let rows: Vec<FindingRow> = visible_rows(&mut *conn, profile_id, None)
        .await?
        .into_iter()
        .filter(|r| current.contains(&r.dataset_id) && withheld_reason(r, gate).is_none())
        .collect();
    if let Some(region) = region {
        let (store, gate, region, cursor) = (
            blobs.clone(),
            gate.clone(),
            region.to_string(),
            cursor.map(str::to_string),
        );
        return blocking(move || region_answer(&store, &datasets, &gate, &region, cursor.as_deref())).await;
    }
    let site = match resolve(&mut *conn, &rows, rsid, hgvs).await? {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let query = rsid.or(hgvs).unwrap_or_default();
    let span_hits = spans_overlapping(&site.chrom, site.pos, site.pos);
    let gene = site
        .gene
        .clone()
        .filter(|g| !g.is_empty())
        .or_else(|| span_hits.first().map(|s| s.gene.clone()));
    let variant = site.variant(gene.as_deref());
    let why = if gated_spans(gate, &site.chrom, site.pos, site.pos).is_empty() {
        None
    } else {
        span_reason(gate)
    };
    if let Some(why) = why {
        let limitation = if why == "minor" {
            "Withheld: the minor rule keeps this site back until the person is an adult."
        } else {
            "Withheld: this site is in a sensitive gene and the profile's opt-in for this surface is off."
        };
        return Ok(Ok(json!({
            "query": query,
            "variant": variant,
            "calls": [],
            "withheld": format!("withheld_{why}"),
            "annotations": {},
            "cited_by_findings": [],
            "coverage": null,
            "limitations": [limitation],
        })));
    }

    let cited_rows: Vec<(&FindingRow, &Value)> = rows
        .iter()
        .flat_map(|r| variants(r).iter().map(move |v| (r, v)))
        .filter(|(_, v)| {
            norm_chrom(v.get("chrom").and_then(Value::as_str).unwrap_or_default()) == site.chrom
                && int_of(v.get("pos")) == site.pos
        })
        .collect();
    let mut cov_by_ds: HashMap<Uuid, CoverageRow> = HashMap::new();
    if let Some(g) = &gene {
        let ids: Vec<Uuid> = current.iter().copied().collect();
        for c in coverage_rows(&mut *conn, &ids).await? {
            if c.target.to_uppercase() == g.to_uppercase() {
                cov_by_ds.insert(c.dataset_id, c);
            }
        }
    }
    let per_source: Vec<(DatasetRow, Option<Value>, Option<CoverageRow>)> = datasets
        .iter()
        .map(|ds| {
            let cited = cited_rows
                .iter()
                .find(|(r, _)| r.dataset_id == ds.id)
                .map(|(_, v)| (*v).clone());
            (ds.clone(), cited, cov_by_ds.get(&ds.id).cloned())
        })
        .collect();
    let (store, at) = (blobs.clone(), site.clone());
    let answers = blocking(move || {
        per_source
            .iter()
            .map(|(ds, cited, cov)| call_at(&store, ds, &at, cited.as_ref(), cov.as_ref()))
            .collect::<Result<Vec<_>>>()
    })
    .await?;
    let mut calls = Vec::new();
    for (ds, call) in datasets.iter().zip(answers) {
        check_call_state(&call)?;
        let mut entry = Map::new();
        insert(&mut entry, "source", json!(source_label(ds)));
        insert(&mut entry, "assay", json!(ds.assay));
        entry.extend(call);
        insert(&mut entry, "src", json!(ds.src_id));
        calls.push(Value::Object(entry));
    }
    let clinvar = cited_rows
        .iter()
        .find(|(_, v)| v.get("clinvar_variation_id").is_some_and(truthy))
        .map(|(r, v)| {
            let ev = r.fact.0.get("evidence").cloned().unwrap_or_else(|| json!({}));
            json!({
                "variation_id": v["clinvar_variation_id"],
                "stars": ev.get("clinvar_stars").cloned().unwrap_or(Value::Null),
                "native": ev.get("native").cloned().unwrap_or(Value::Null),
            })
        });
    let newest_cov = datasets.first().and_then(|d| cov_by_ds.get(&d.id));
    let mut limitations = Vec::new();
    if calls
        .iter()
        .any(|c| matches!(c["call_state"].as_str(), Some("not_on_array" | "not_covered")))
    {
        limitations.push(UNKNOWN_NOTE.to_string());
    }
    limitations.push("This lookup covers one position; it does not test other variants of the gene.".into());
    let cited: BTreeSet<&str> = cited_rows.iter().map(|(r, _)| r.fact_id.as_str()).collect();
    Ok(Ok(json!({
        "query": query,
        "variant": variant,
        "calls": calls,
        "withheld": null,
        "annotations": clinvar.map_or_else(|| json!({}), |c| json!({"clinvar": c})),
        "cited_by_findings": cited.into_iter().collect::<Vec<_>>(),
        "coverage": newest_cov.map_or(Value::Null, coverage_short),
        "limitations": limitations,
    })))
}

fn subtract(start: i64, end: i64, cuts: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut sorted = cuts.to_vec();
    sorted.sort_unstable();
    let mut parts = vec![(start, end)];
    for (cs, ce) in sorted {
        let mut next = Vec::new();
        for (s, e) in parts {
            if ce < s || cs > e {
                next.push((s, e));
                continue;
            }
            if s < cs {
                next.push((s, cs - 1));
            }
            if ce < e {
                next.push((ce + 1, e));
            }
        }
        parts = next;
    }
    parts
}

/// Python's `int(text)`: surrounding whitespace, a sign and `_` between digits.
fn py_int(text: &str) -> Option<i64> {
    let t = text.trim();
    let (sign, digits) = match t.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    clean.parse::<i64>().ok().map(|v| sign * v)
}

/// Python's `records[offset : offset + PAGE]`.
fn py_page<T: Clone>(items: &[T], offset: i64) -> Vec<T> {
    let n = items.len() as i64;
    let norm = |i: i64| if i < 0 { (n + i).max(0) } else { i.min(n) };
    let (a, b) = (norm(offset), norm(offset + PAGE as i64));
    if a >= b {
        return Vec::new();
    }
    items[a as usize..b as usize].to_vec()
}

fn region_answer(
    blobs: &BlobStore,
    datasets: &[DatasetRow],
    gate: &Gate,
    region: &str,
    cursor: Option<&str>,
) -> Result<Answer> {
    let Some(m) = REGION.captures(region) else {
        return Ok(Err(bad(
            "A region is chrN:start-end on GRCh38, for example chr19:44905000-44910000.",
        )));
    };
    let chrom = norm_chrom(&m[1]);
    let (Ok(start), Ok(end)) = (m[2].parse::<i64>(), m[3].parse::<i64>()) else {
        return Ok(Err(bad("The region end must not be before its start.")));
    };
    if start < 1 || end < start {
        return Ok(Err(bad("The region end must not be before its start.")));
    }
    if end - start + 1 > MAX_REGION_BP {
        return Ok(Err(GenomeQueryError::new(
            "region_too_large",
            "A region spans at most 1 Mb.",
            Some("Split it into smaller regions."),
        )));
    }
    let offset = match cursor.filter(|c| !c.is_empty()) {
        None => 0,
        Some(c) => match py_int(c) {
            Some(o) => o,
            None => return Ok(Err(bad("Unknown cursor."))),
        },
    };
    let mut clipped = Vec::new();
    let mut cuts = Vec::new();
    if let Some(why) = span_reason(gate) {
        for s in gated_spans(gate, &chrom, start, end) {
            let (cs, ce) = (start.max(s.start), end.min(s.end));
            cuts.push((cs, ce));
            clipped.push(json!({"gene": s.gene, "start": cs, "end": ce, "reason": format!("withheld_{why}")}));
        }
    }
    let parts = subtract(start, end, &cuts);
    let mut records: Vec<(i64, String, Value)> = Vec::new();
    for ds in datasets {
        let (Some(vcf), Some(tbi)) = (ds.vcf.as_deref(), ds.tbi.as_deref()) else {
            continue;
        };
        let mut vf = open(blobs, vcf, tbi)?;
        let label = source_label(ds);
        for (s, e) in &parts {
            for rec in fetch(&mut vf, &chrom, *s, *e)? {
                if is_block(&rec) || !(*s <= rec.pos && rec.pos <= *e) {
                    continue;
                }
                let mut entry = Map::new();
                insert(&mut entry, "chrom", json!(chrom));
                insert(&mut entry, "pos", json!(rec.pos));
                insert(&mut entry, "id", json!(rec.id));
                insert(&mut entry, "ref", json!(rec.reference));
                insert(&mut entry, "alts", json!(rec.alts));
                insert(&mut entry, "source", json!(label));
                insert(&mut entry, "assay", json!(ds.assay));
                entry.extend(from_record(&rec, "vcf"));
                insert(&mut entry, "src", json!(ds.src_id));
                records.push((rec.pos, label.clone(), Value::Object(entry)));
            }
        }
    }
    records.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let all: Vec<Value> = records.into_iter().map(|(_, _, v)| v).collect();
    let page = py_page(&all, offset);
    let next = offset + PAGE as i64;
    let mut limitations =
        vec!["Region answers list the records in the VCF; a position with no record is not listed.".to_string()];
    if datasets.iter().any(|d| d.vcf.is_none()) {
        limitations.push("A source without a VCF on file is not part of region answers.".into());
    }
    Ok(Ok(json!({
        "query": region,
        "region": {"chrom": chrom, "start": start, "end": end},
        "records": page,
        "clipped": clipped,
        "next_cursor": if next < all.len() as i64 { json!(next.to_string()) } else { Value::Null },
        "limitations": limitations,
    })))
}

/// A curated rsID row (dbSNP, public data).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefSite {
    pub rsid: String,
    pub chrom: String,
    pub pos: i32,
    pub reference: String,
    pub alts: Vec<String>,
    pub gene: Option<String>,
}

/// Load curated rsID rows into the index. Returns the count.
pub async fn load_ref_sites(conn: &mut PgConnection, rows: &[RefSite], source: &str) -> Result<usize> {
    for r in rows {
        sqlx::query(
            "insert into genome_ref_site (rsid, chrom, pos, ref, alts, gene, source) values ($1, $2, $3, $4, $5, \
             $6, $7) on conflict (rsid, chrom, pos) do update set ref = excluded.ref, alts = excluded.alts, \
             gene = excluded.gene, source = excluded.source",
        )
        .bind(&r.rsid)
        .bind(norm_chrom(&r.chrom))
        .bind(r.pos)
        .bind(&r.reference)
        .bind(&r.alts)
        .bind(&r.gene)
        .bind(source)
        .execute(&mut *conn)
        .await?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_state_outside_the_contract_is_refused() {
        let mut call = Map::new();
        call.insert("call_state".into(), json!("het"));
        let err = check_call_state(&call).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("outside the contract"), "{err}");
        call.insert("call_state".into(), json!("alt"));
        assert!(check_call_state(&call).is_ok());
    }

    #[test]
    fn records_parse_like_pysam() {
        let rec = Record::parse("chr10\t94781859\trs4244285\tG\tA\t50\tPASS\t.\tGT:DP:GQ\t0/1:30:60");
        let rec = rec.unwrap_or_else(|| Record::parse("x\t1\t.\tA\t.\t.\t.\t.").unwrap_or_else(|| unreachable!()));
        let out = from_record(&rec, "vcf");
        assert_eq!(out["genotype"], "G/A");
        assert_eq!(out["zygosity"], "het");
        assert_eq!(out["gq"], 60);
        let block = Record::parse("chr1\t100\t.\tN\t<NON_REF>\t50\tPASS\tEND=180\tGT:DP:GQ:MIN_DP\t0/0:4:8:4");
        let block = block.unwrap_or_else(|| unreachable!());
        assert!(is_block(&block) && block.end == 180);
        let hemi = Record::parse("chrX\t5\t.\tA\tG\t.\t.\t.\tGT\t1").unwrap_or_else(|| unreachable!());
        assert_eq!(from_record(&hemi, "vcf")["zygosity"], "hemi");
        let nocall = Record::parse("chr3\t5\t.\tA\tG\t.\t.\t.\tGT\t./.").unwrap_or_else(|| unreachable!());
        assert_eq!(from_record(&nocall, "vcf")["call_state"], "no_call");
        assert_eq!(from_record(&nocall, "vcf")["genotype"], "./.");
    }

    #[test]
    fn subtraction_and_paging() {
        assert_eq!(subtract(1, 100, &[(10, 20), (50, 200)]), vec![(1, 9), (21, 49)]);
        let items: Vec<i32> = (0..120).collect();
        assert_eq!(py_page(&items, 100).len(), 20);
        assert_eq!(py_int(" 50 "), Some(50));
        assert_eq!(py_int("x"), None);
    }
}
