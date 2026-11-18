//! The ClinVar diff (genomics.md 9): which ClinVar records a person carries, what changed since the
//! last release we compared, and which of those changes become a review item.
//!
//! [`diff_release`] reads a GRCh38 ClinVar VCF from a local path (bgzipped or plain) and, for every
//! current genome dataset, merge-joins it by sorted position with the dataset's calls VCF, or with
//! the alt sites its facts cite when it has no VCF. Only records whose exact allele the person carries
//! are kept, as the dataset's `clinvar_carried` snapshot. Both files stream: memory stays flat
//! whatever their size.
//!
//! The baseline is the facts themselves on the first run (a P/LP fact and its `clinvar_stars`; any
//! other carried site counts as not reported), then the previous compared release (its snapshot, empty
//! when the person carried nothing in it; `genome_dataset.clinvar_compared` holds its date). Three things alert, as a
//! `genome_reinterpretation` review item: an upgrade to P/LP with at least 1 star, a downgrade of a
//! P/LP the person was shown, and any change in an ACMG SF gene. Everything else is a silent event.
//! The review item names nothing: it reads "a result you carry changed" (or "a withheld result
//! changed" behind the sensitive switch or the minor rule), and a chip result adds "worth a clinical
//! test". The gene and both classes are read from `GET /genome/events`, where the reader's own gate
//! decides at read time.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::NaiveDate;
use ownhealth_core::access::{get_profile, majority, today};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::review::{ReviewItemIn, create_item};
use ownhealth_core::{Error, Result};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::blocking;
use crate::sensitive::{acmg_sf_genes, carrier_spans_overlapping, is_sensitive_gene, norm_chrom, sensitive_gene_at};
use crate::vcf::{MAX_LINE, Record};

const WORDING: &str = include_str!("../../../../data/genome/clinvar_alerts.json");
/// An upgrade alerts from this many ClinVar review stars.
pub const ALERT_MIN_STARS: i16 = 1;
/// Fact categories that state a P/LP variant.
pub const PLP_CATEGORIES: [&str; 3] = ["monogenic_pathogenic", "acmg_sf", "carrier"];
pub const EVENT_KINDS: [&str; 7] = [
    "upgraded_to_plp",
    "downgraded_from_plp",
    "stars_changed",
    "became_conflicting",
    "vus_resolved",
    "significance_changed",
    "new_assertion_on_carried_variant",
];

static TEMPLATES: LazyLock<Map<String, Value>> = LazyLock::new(|| {
    serde_json::from_str::<Value>(WORDING)
        .ok()
        .and_then(|v| v.get("templates").and_then(Value::as_object).cloned())
        .unwrap_or_default()
});
static SF_GENES: LazyLock<BTreeSet<String>> = LazyLock::new(acmg_sf_genes);

/// Every es/en template of the alerts, as (key, lang, text), for the wording lint.
pub fn templates() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (k, t) in TEMPLATES.iter() {
        for (lang, s) in t.as_object().into_iter().flatten() {
            out.push((k.clone(), lang.clone(), s.as_str().unwrap_or_default().to_string()));
        }
    }
    out
}

fn tpl(key: &str, lang: &str) -> String {
    let lang = if lang == "es" { "es" } else { "en" };
    TEMPLATES
        .get(key)
        .and_then(|t| t.get(lang))
        .and_then(Value::as_str)
        .unwrap_or(key)
        .to_string()
}

// ------------------------------------------------------------- policy ----

/// The ClinVar significance as one of plp, vus, conflicting, blb or other (CLNSIG, first term).
pub fn class_of(significance: &str) -> &'static str {
    let s = significance.trim().to_ascii_lowercase().replace(' ', "_");
    let first = s.split(['|', ',']).next().unwrap_or_default();
    if first.starts_with("pathogenic") || first.starts_with("likely_pathogenic") {
        "plp"
    } else if first.starts_with("conflicting") {
        "conflicting"
    } else if first.starts_with("uncertain") {
        "vus"
    } else if first.starts_with("benign") || first.starts_with("likely_benign") {
        "blb"
    } else {
        "other"
    }
}

/// ClinVar's review stars from CLNREVSTAT
/// (https://www.ncbi.nlm.nih.gov/clinvar/docs/review_status/).
pub fn stars_of(review_status: &str) -> i16 {
    let r = review_status.to_ascii_lowercase().replace(' ', "_");
    if r.contains("practice_guideline") {
        4
    } else if r.contains("expert_panel") {
        3
    } else if r.contains("multiple_submitters") && r.contains("no_conflicts") {
        2
    } else if r.starts_with("criteria_provided") {
        1
    } else {
        0
    }
}

/// What was known about a carried record before this release.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Before {
    /// plp, vus, conflicting, blb or other.
    pub class: &'static str,
    /// None when the facts file did not give the stars.
    pub stars: Option<i16>,
}

/// The event one carried record makes, if any. `old` None means nothing was reported before: on the
/// first run (the facts as baseline) only a P/LP counts, later every new record does.
pub fn event_kind(old: Option<&Before>, new_class: &str, new_stars: i16, first_run: bool) -> Option<&'static str> {
    let Some(o) = old else {
        return if new_class == "plp" {
            Some("upgraded_to_plp")
        } else if first_run {
            None
        } else {
            Some("new_assertion_on_carried_variant")
        };
    };
    Some(match (o.class, new_class) {
        ("plp", "conflicting") => "became_conflicting",
        ("plp", n) if n != "plp" => "downgraded_from_plp",
        (c, "plp") if c != "plp" => "upgraded_to_plp",
        ("vus", "blb") => "vus_resolved",
        (_, "conflicting") if o.class != "conflicting" => "became_conflicting",
        (c, n) if c != n => "significance_changed",
        _ if o.stars.is_some_and(|s| s != new_stars) => "stars_changed",
        _ => return None,
    })
}

/// Whether two CLNSIG values say the same thing: ClinVar's terms, read without case or spacing.
/// Pathogenic and Likely_pathogenic are one class but not the same significance.
pub fn same_significance(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().to_ascii_lowercase().replace(' ', "_");
    norm(a) == norm(b)
}

/// The alert filter: an upgrade to P/LP with at least 1 star, a downgrade of a P/LP the person was
/// shown, any change in an ACMG SF gene.
pub fn alerts(kind: &str, old: Option<&Before>, new_stars: i16, sf_gene: bool, shown_plp: bool) -> bool {
    let was_plp = old.is_some_and(|o| o.class == "plp");
    let class = match kind {
        "upgraded_to_plp" => new_stars >= ALERT_MIN_STARS,
        "downgraded_from_plp" | "became_conflicting" => was_plp && shown_plp,
        _ => false,
    };
    class || sf_gene
}

pub fn is_sf_gene(gene: Option<&str>) -> bool {
    gene.is_some_and(|g| SF_GENES.contains(&g.trim().to_uppercase()))
}

// ------------------------------------------------------------ wording ----

/// What an event says, before the reader's gate decides whether the gene may be named.
#[derive(Debug, Clone, Default)]
pub struct EventText<'a> {
    pub gene: Option<&'a str>,
    pub old: Option<&'a Before>,
    pub new_class: &'a str,
    pub new_stars: i16,
    pub withheld: bool,
    pub unconfirmed: bool,
    /// `old` is None because the previous compared release had no such record, not because the
    /// genome file did not report it (the first run, against the facts).
    pub not_in_previous_release: bool,
}

fn stars_text(n: i16, lang: &str) -> String {
    if n == 1 {
        tpl("stars_one", lang)
    } else {
        tpl("stars_many", lang).replace("{n}", &n.to_string())
    }
}

/// The alert sentence in `lang` (es or en).
pub fn summary(t: &EventText<'_>, lang: &str) -> String {
    let mut s = if t.withheld {
        tpl("withheld", lang)
    } else {
        let old = match t.old {
            None if t.not_in_previous_release => tpl("class_not_in_clinvar", lang),
            None => tpl("class_not_reported", lang),
            Some(o) => {
                let c = tpl(&format!("class_{}", o.class), lang);
                match o.stars {
                    Some(n) => format!("{c} ({})", stars_text(n, lang)),
                    None => c,
                }
            }
        };
        // Same class and stars: ClinVar moved inside the class (Pathogenic to Likely_pathogenic).
        let same = t
            .old
            .is_some_and(|o| o.class == t.new_class && o.stars == Some(t.new_stars));
        let key = match (same, t.gene.is_some()) {
            (false, true) => "named",
            (false, false) => "named_no_gene",
            (true, true) => "named_same_class",
            (true, false) => "named_same_class_no_gene",
        };
        tpl(key, lang)
            .replace("{gene}", t.gene.unwrap_or_default())
            .replace("{new}", &tpl(&format!("class_{}", t.new_class), lang))
            .replace("{stars}", &stars_text(t.new_stars, lang))
            .replace("{old}", &old)
    };
    if t.unconfirmed {
        s.push(' ');
        s.push_str(&tpl("chip", lang));
    }
    s
}

/// Where a reclassified call is made again: OwnGenome, never OwnHealth, which only compares.
pub fn rerun_text(lang: &str) -> String {
    tpl("rerun", lang)
}

/// The review item's sentence in `lang`: it never names the variant, so any reviewer may read it.
pub fn review_summary(withheld: bool, unconfirmed: bool, lang: &str) -> String {
    let mut s = tpl(if withheld { "withheld" } else { "changed" }, lang);
    if unconfirmed {
        s.push(' ');
        s.push_str(&tpl("chip", lang));
    }
    s
}

// -------------------------------------------------------------- files ----

/// (contig rank, POS): chr1-22 are 1-22, X 23, Y 24, M 25. None for any other contig.
pub type Key = (u8, i64);

pub fn contig_rank(chrom: &str) -> Option<u8> {
    let c = norm_chrom(chrom);
    match c.get(3..).unwrap_or_default() {
        "X" => Some(23),
        "Y" => Some(24),
        "M" => Some(25),
        n => n.parse::<u8>().ok().filter(|v| (1..=22).contains(v)),
    }
}

/// REF and ALT upper-cased with their shared trailing bases cut (one base kept), so a multiallelic
/// record split into one ALT each still matches: GT>GTT is G>GT.
pub fn trim_alleles(reference: &str, alt: &str) -> (String, String) {
    let (mut r, mut a): (Vec<u8>, Vec<u8>) = (
        reference.trim().to_ascii_uppercase().into_bytes(),
        alt.trim().to_ascii_uppercase().into_bytes(),
    );
    while r.len() > 1 && a.len() > 1 && r.last() == a.last() {
        r.pop();
        a.pop();
    }
    (
        String::from_utf8_lossy(&r).into_owned(),
        String::from_utf8_lossy(&a).into_owned(),
    )
}

fn bad_file(what: &str, message: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{what}: {message}"))
}

/// A gzipped file or plain text. ClinVar ships `clinvar.vcf.gz` bgzipped, which is a run of gzip
/// members; a copy compressed again with plain `gzip` is one member. The multi-member reader reads both.
fn open_text(path: &Path) -> io::Result<Box<dyn BufRead + Send>> {
    let mut magic = [0u8; 2];
    let n = File::open(path)?.read(&mut magic)?;
    let f = File::open(path)?;
    Ok(if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(io::BufReader::new(flate2::read::MultiGzDecoder::new(
            io::BufReader::new(f),
        )))
    } else {
        Box::new(io::BufReader::new(f))
    })
}

/// One line into `buf`; false at the end. A line longer than 1 MiB is refused.
fn next_line(r: &mut dyn BufRead, buf: &mut String) -> io::Result<bool> {
    buf.clear();
    let n = r.take(MAX_LINE + 1).read_line(buf)?;
    if n as u64 > MAX_LINE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a line longer than 1 MiB"));
    }
    Ok(n > 0)
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%Y%m%d")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
        .ok()
}

/// A ClinVar release, positioned after its header.
struct Release {
    reader: Box<dyn BufRead + Send>,
    date: NaiveDate,
    label: String,
}

fn open_release(path: &Path) -> Result<Release> {
    let label = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut reader = open_text(path).map_err(|e| bad_file(&label, e))?;
    let mut line = String::new();
    let mut date = None;
    loop {
        if !next_line(&mut reader, &mut line).map_err(|e| bad_file(&label, e))? {
            return Err(bad_file(&label, "not a VCF: no #CHROM line"));
        }
        if let Some(d) = line.strip_prefix("##fileDate=") {
            date = parse_date(d);
        }
        if line.starts_with("#CHROM") {
            break;
        }
        if !line.starts_with('#') {
            return Err(bad_file(&label, "not a VCF: a data line before #CHROM"));
        }
    }
    let date = date.ok_or_else(|| bad_file(&label, "no ##fileDate, so the ClinVar release is unknown"))?;
    Ok(Release { reader, date, label })
}

/// The release date (`##fileDate`) of a ClinVar VCF.
pub fn release_date(path: &Path) -> Result<NaiveDate> {
    Ok(open_release(path)?.date)
}

fn info<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    info.split(';').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// A ClinVar record the person carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Carried {
    pub key: Key,
    pub chrom: String,
    pub reference: String,
    pub alt: String,
    pub variation_id: i64,
    pub gene: Option<String>,
    pub significance: String,
    pub stars: i16,
}

/// The person's alt alleles in sorted order: (key, trimmed REF, trimmed ALT).
pub trait Alleles: Send {
    fn next_allele(&mut self) -> Result<Option<(Key, String, String)>>;
}

/// Alt sites a facts file cites, for a dataset without a calls VCF.
pub struct FactAlleles(std::vec::IntoIter<(Key, String, String)>);

impl FactAlleles {
    pub fn new(mut sites: Vec<(Key, String, String)>) -> FactAlleles {
        sites.sort();
        sites.dedup();
        FactAlleles(sites.into_iter())
    }
}

impl Alleles for FactAlleles {
    fn next_allele(&mut self) -> Result<Option<(Key, String, String)>> {
        Ok(self.0.next())
    }
}

/// The alt alleles in a calls VCF's first sample genotype.
pub struct VcfAlleles {
    reader: Box<dyn BufRead + Send>,
    line: String,
    queue: std::collections::VecDeque<(Key, String, String)>,
    last: Key,
    label: String,
}

impl VcfAlleles {
    pub fn open(path: &Path, label: &str) -> Result<VcfAlleles> {
        Ok(VcfAlleles {
            reader: open_text(path).map_err(|e| bad_file(label, e))?,
            line: String::new(),
            queue: Default::default(),
            last: (0, 0),
            label: label.into(),
        })
    }
}

impl Alleles for VcfAlleles {
    fn next_allele(&mut self) -> Result<Option<(Key, String, String)>> {
        while self.queue.is_empty() {
            if !next_line(&mut self.reader, &mut self.line).map_err(|e| bad_file(&self.label, e))? {
                return Ok(None);
            }
            if self.line.starts_with('#') || self.line.trim().is_empty() {
                continue;
            }
            let Some(rec) = Record::parse(&self.line) else {
                return Err(bad_file(&self.label, "a line that is not a VCF record"));
            };
            let Some(rank) = contig_rank(&rec.chrom) else {
                continue;
            };
            let key = (rank, rec.pos);
            if key < self.last {
                return Err(bad_file(
                    &self.label,
                    "records are not sorted by GRCh38 contig order (chr1 to chr22, X, Y, M)",
                ));
            }
            self.last = key;
            let (gt, _) = rec.genotype();
            let mut carried: Vec<usize> = gt.into_iter().flatten().filter(|a| *a > 0).collect();
            carried.sort_unstable();
            carried.dedup();
            for a in carried {
                if let Some(alt) = rec.alts.get(a - 1).filter(|x| !x.starts_with(['<', '*', '.'])) {
                    let (r, x) = trim_alleles(&rec.reference, alt);
                    self.queue.push_back((key, r, x));
                }
            }
        }
        Ok(self.queue.pop_front())
    }
}

/// A person's alleles that remember whether reading them failed, so a broken calls VCF is told apart
/// from a broken release.
struct Watched {
    inner: Box<dyn Alleles>,
    failed: bool,
}

impl Alleles for Watched {
    fn next_allele(&mut self) -> Result<Option<(Key, String, String)>> {
        let next = self.inner.next_allele();
        self.failed |= next.is_err();
        next
    }
}

/// Merge-join a ClinVar release with one person's alleles, both sorted by position. Returns the
/// release date and the records whose exact allele the person carries. Stops reading the release once
/// the person's alleles run out.
pub fn carried_records(release: &Path, person: &mut dyn Alleles) -> Result<(NaiveDate, Vec<Carried>)> {
    let Release {
        mut reader,
        date,
        label,
    } = open_release(release)?;
    let mut out = Vec::new();
    let mut pending = person.next_allele()?;
    let mut group: Vec<(String, String)> = Vec::new();
    let mut group_key: Option<Key> = None;
    let mut fill = |pending: &mut Option<(Key, String, String)>,
                    group: &mut Vec<(String, String)>,
                    group_key: &mut Option<Key>|
     -> Result<()> {
        group.clear();
        *group_key = pending.as_ref().map(|p| p.0);
        while let Some((k, r, a)) = pending.take() {
            if Some(k) != *group_key {
                *pending = Some((k, r, a));
                break;
            }
            group.push((r, a));
            *pending = person.next_allele()?;
        }
        Ok(())
    };
    fill(&mut pending, &mut group, &mut group_key)?;
    let mut line = String::new();
    let mut last: Key = (0, 0);
    while let Some(gk) = group_key {
        if !next_line(&mut reader, &mut line).map_err(|e| bad_file(&label, e))? {
            break;
        }
        let mut cols = line.trim_end_matches(['\n', '\r']).splitn(9, '\t');
        let Some(chrom) = cols.next() else {
            continue;
        };
        let Some(rank) = contig_rank(chrom) else {
            continue;
        };
        let Some(pos) = cols.next() else {
            return Err(bad_file(&label, "a record with fewer than the 8 VCF columns"));
        };
        let pos: i64 = pos
            .parse()
            .map_err(|_| bad_file(&label, format!("a POS that is not a number: {pos}")))?;
        let key = (rank, pos);
        if key < last {
            return Err(bad_file(&label, "records are not sorted by GRCh38 contig order"));
        }
        last = key;
        // Every record needs its eight columns, carried or not, so the full pass over a release
        // refuses one with a cut row instead of letting a later run skip the record unseen.
        let (Some(id), Some(reference), Some(alt), Some(_qual), Some(_filter), Some(info_col)) = (
            cols.next(),
            cols.next(),
            cols.next(),
            cols.next(),
            cols.next(),
            cols.next(),
        ) else {
            return Err(bad_file(&label, "a record with fewer than the 8 VCF columns"));
        };
        let mut gk = gk;
        while gk < key {
            fill(&mut pending, &mut group, &mut group_key)?;
            match group_key {
                Some(k) => gk = k,
                None => break,
            }
        }
        if group_key != Some(key) {
            continue;
        }
        let (Ok(variation_id), Some(sig)) = (id.parse::<i64>(), info(info_col, "CLNSIG")) else {
            continue;
        };
        if group.contains(&trim_alleles(reference, alt)) {
            out.push(Carried {
                key,
                chrom: chrom.to_string(),
                reference: reference.to_string(),
                alt: alt.to_string(),
                variation_id,
                gene: info(info_col, "GENEINFO")
                    .and_then(|g| g.split(['|', ':']).next())
                    .filter(|g| !g.is_empty())
                    .map(str::to_string),
                significance: sig.to_string(),
                stars: stars_of(info(info_col, "CLNREVSTAT").unwrap_or_default()),
            });
        }
    }
    // The release ended first: read the person's remaining calls too, so a calls file out of order
    // past the release's last record is refused rather than read only in part.
    while pending.is_some() {
        pending = person.next_allele()?;
    }
    Ok((date, out))
}

// ----------------------------------------------------------- database ----

/// A variant a current fact cites, with what the fact says about it.
#[derive(Debug, Clone)]
struct FactSite {
    key: Key,
    alleles: (String, String),
    variation_id: Option<i64>,
    fact_id: String,
    plp: bool,
    stars: Option<i16>,
    hidden: bool,
    alt_called: bool,
    sensitive: bool,
    adult_onset: bool,
    carrier: bool,
}

async fn fact_sites(conn: &mut PgConnection, dataset_id: Uuid) -> Result<Vec<FactSite>> {
    let rows: Vec<(String, String, bool, bool, bool, sqlx::types::Json<Value>)> = sqlx::query_as(
        "select fact_id, category, sensitive, adult_onset, carrier, fact from genome_finding \
         where dataset_id = $1 and status = 'current' order by fact_id",
    )
    .bind(dataset_id)
    .fetch_all(conn)
    .await?;
    let mut out = Vec::new();
    for (fact_id, category, sensitive, adult_onset, carrier, fact) in rows {
        let fact = fact.0;
        let stars = fact
            .pointer("/evidence/clinvar_stars")
            .and_then(Value::as_i64)
            .and_then(|s| i16::try_from(s).ok());
        for v in fact.get("variants").and_then(Value::as_array).into_iter().flatten() {
            let (Some(chrom), Some(pos)) = (
                v.get("chrom").and_then(Value::as_str),
                v.get("pos").and_then(Value::as_i64),
            ) else {
                continue;
            };
            let Some(rank) = contig_rank(chrom) else {
                continue;
            };
            let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
            out.push(FactSite {
                key: (rank, pos),
                alleles: trim_alleles(s("ref"), s("alt")),
                variation_id: v.get("clinvar_variation_id").and_then(Value::as_i64),
                fact_id: fact_id.clone(),
                plp: PLP_CATEGORIES.contains(&category.as_str()),
                stars,
                hidden: fact.get("hidden_by_default").and_then(Value::as_bool).unwrap_or(false),
                alt_called: v.pointer("/genotype/call_state").and_then(Value::as_str) == Some("alt"),
                sensitive,
                adult_onset,
                carrier,
            });
        }
    }
    Ok(out)
}

/// One dataset's outcome.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DatasetReport {
    pub dataset_id: Uuid,
    pub profile_id: Uuid,
    /// The profile's slug, so an operator can tell whose genome it is.
    pub profile: String,
    pub assay: String,
    /// Why nothing was compared: `already_compared` (this release or a newer one was compared),
    /// `older_than_genome_file` (the facts were annotated with a newer ClinVar), `unreadable` (the
    /// dataset's calls VCF could not be read in GRCh38 contig order; see `error`), `no_longer_current`
    /// (the dataset was replaced or its genome file retracted while the run went on).
    pub skipped: Option<&'static str>,
    /// What went wrong when `skipped` is `unreadable`: file and dataset ids only, never a genotype.
    pub error: Option<String>,
    pub carried: usize,
    pub events: usize,
    pub alerts: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiffReport {
    pub release: NaiveDate,
    pub datasets: Vec<DatasetReport>,
}

#[derive(sqlx::FromRow)]
struct DatasetRow {
    id: Uuid,
    profile_id: Uuid,
    household_id: Uuid,
    slug: String,
    document_id: Option<Uuid>,
    assay: String,
    db_versions: sqlx::types::Json<Value>,
    vcf: Option<String>,
}

/// Compare every current genome dataset with the ClinVar release at `release` (a GRCh38 VCF), write
/// the events and alerts, and keep the carried records as each dataset's new snapshot. One
/// transaction per dataset. The minor rule counts age with `access::majority` on `access::today`, as
/// every other minor check does.
pub async fn diff_release(pool: &PgPool, blobs: &BlobStore, release: &Path) -> Result<DiffReport> {
    diff_release_in(pool, blobs, release, None).await
}

/// [`diff_release`] for the genomes of one household only (its weekly ClinVar source), or of every
/// household when `household` is None.
pub async fn diff_release_in(
    pool: &PgPool,
    blobs: &BlobStore,
    release: &Path,
    household: Option<Uuid>,
) -> Result<DiffReport> {
    // Read the whole release once first: a release that is truncated, corrupt or out of order stops
    // the run before any dataset is compared, so it is never reported as a broken genome.
    let path = release.to_path_buf();
    let date = blocking(move || {
        let mut past_the_end = FactAlleles::new(vec![((u8::MAX, i64::MAX), String::new(), String::new())]);
        carried_records(&path, &mut past_the_end).map(|(date, _)| date)
    })
    .await?;
    let mut conn = pool.acquire().await?;
    let datasets: Vec<DatasetRow> = sqlx::query_as(
        "select d.id, d.profile_id, p.household_id, p.slug, d.document_id, d.assay, d.db_versions, encode(d.vcf_blob, 'hex') as vcf \
         from genome_dataset d join profile p on p.id = d.profile_id where d.status = 'current' and not exists \
         (select 1 from document doc where doc.id = d.document_id and doc.status = 'retracted') \
         and ($1::uuid is null or p.household_id = $1) \
         order by d.assay in ('chip', 'chip_imputed'), d.imported_at, d.id",
    )
    .bind(household)
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);
    let mut reports = Vec::new();
    for ds in datasets {
        reports.push(diff_dataset(pool, blobs, release, date, &ds).await?);
    }
    Ok(DiffReport {
        release: date,
        datasets: reports,
    })
}

async fn diff_dataset(
    pool: &PgPool,
    blobs: &BlobStore,
    release: &Path,
    date: NaiveDate,
    ds: &DatasetRow,
) -> Result<DatasetReport> {
    let mut report = DatasetReport {
        dataset_id: ds.id,
        profile_id: ds.profile_id,
        profile: ds.slug.clone(),
        assay: ds.assay.clone(),
        skipped: None,
        error: None,
        carried: 0,
        events: 0,
        alerts: 0,
    };
    let mut tx = pool.begin().await?;
    // One run at a time per dataset: a second run waits here, then reads the release the first one
    // compared, so two runs never compare the same release twice.
    let locked: Option<(bool, Option<NaiveDate>)> =
        sqlx::query_as("select status = 'current', clinvar_compared from genome_dataset where id = $1 for update")
            .bind(ds.id)
            .fetch_optional(&mut *tx)
            .await?;
    let current = locked.map(|(c, _)| c);
    // The last release compared, even one in which the person carried nothing.
    let prev_date = locked.and_then(|(_, d)| d);
    // The dataset was listed before this point, so its file may have been retracted since. A
    // retraction locks the document row before it dismisses the file's alerts: it either finished
    // and this read sees it, or it waits for this transaction and then dismisses what it wrote.
    let retracted = match ds.document_id {
        Some(doc) => {
            sqlx::query_scalar::<_, i32>("select 1 from document where id = $1 and status <> 'retracted' for share")
                .bind(doc)
                .fetch_optional(&mut *tx)
                .await?
                .is_none()
        }
        None => false,
    };
    if current != Some(true) || retracted {
        report.skipped = Some("no_longer_current");
        return Ok(report);
    }
    let facts_release = ds
        .db_versions
        .0
        .get("clinvar")
        .and_then(Value::as_str)
        .map(str::to_string);
    if prev_date.is_some_and(|p| p >= date) {
        report.skipped = Some("already_compared");
        return Ok(report);
    }
    if prev_date.is_none() && facts_release.as_deref().and_then(parse_date).is_some_and(|f| f > date) {
        report.skipped = Some("older_than_genome_file");
        return Ok(report);
    }
    let sites = fact_sites(&mut tx, ds.id).await?;
    let label = format!("the calls VCF of dataset {}", ds.id);
    let vcf_path: Result<Option<PathBuf>> = ds
        .vcf
        .as_ref()
        .map(|sha| blobs.path(sha).map_err(|e| bad_file(&label, e)))
        .transpose();
    let fact_alleles: Vec<(Key, String, String)> = sites
        .iter()
        .filter(|s| s.alt_called)
        .map(|s| (s.key, s.alleles.0.clone(), s.alleles.1.clone()))
        .collect();
    let rel = release.to_path_buf();
    // The outer error is the release's and stops the run; the inner one is this dataset's calls VCF,
    // which must not stop the other datasets.
    let read = blocking(move || -> Result<Result<Vec<Carried>>> {
        let person: Box<dyn Alleles> = match vcf_path.and_then(|p| p.map(|p| VcfAlleles::open(&p, &label)).transpose())
        {
            Ok(Some(v)) => Box::new(v),
            Ok(None) => Box::new(FactAlleles::new(fact_alleles)),
            Err(e) => return Ok(Err(e)),
        };
        let mut person = Watched {
            inner: person,
            failed: false,
        };
        match carried_records(&rel, &mut person) {
            Ok((seen, carried)) if seen == date => Ok(Ok(carried)),
            // A local release replaced while the run read it: stop rather than file the new
            // release's records under the old date.
            Ok((seen, _)) => Err(bad_file(
                &rel.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                format!("the release changed during the run ({seen} instead of {date})"),
            )),
            Err(e) if person.failed => Ok(Err(e)),
            Err(e) => Err(e),
        }
    })
    .await?;
    let carried = match read {
        Ok(carried) => carried,
        // Nothing was written yet, and the transaction rolls back when dropped.
        Err(e) => {
            report.skipped = Some("unreadable");
            report.error = Some(e.to_string());
            return Ok(report);
        }
    };
    report.carried = carried.len();

    let prev: HashMap<i64, (String, i16)> = sqlx::query_as::<_, (i64, String, i16)>(
        "select variation_id, significance, stars from clinvar_carried where dataset_id = $1",
    )
    .bind(ds.id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(v, s, n)| (v, (s, n)))
    .collect();
    let first_run = prev_date.is_none();
    let release_from = match prev_date {
        Some(d) => Some(d.to_string()),
        None => facts_release,
    };
    let release_to = date.to_string();
    let profile = get_profile(&mut tx, ds.profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("profile {}", ds.profile_id)))?;
    let aom = majority(&mut tx, ds.household_id).await?;
    let minor = profile.is_minor(today(), aom);
    let unconfirmed = matches!(ds.assay.as_str(), "chip" | "chip_imputed");

    let mut seen = std::collections::HashSet::new();
    let mut snapshot: Vec<&Carried> = Vec::new();
    for c in &carried {
        if !seen.insert(c.variation_id) {
            continue;
        }
        snapshot.push(c);
        let alleles = trim_alleles(&c.reference, &c.alt);
        let mut matches: Vec<&FactSite> = sites
            .iter()
            .filter(|s| s.variation_id == Some(c.variation_id) || (s.key == c.key && s.alleles == alleles))
            .collect();
        matches.sort_by_key(|s| !s.plp);
        let fact = matches.first().copied();
        let old: Option<Before> = if first_run {
            fact.filter(|f| f.plp).map(|f| Before {
                class: "plp",
                stars: f.stars,
            })
        } else {
            prev.get(&c.variation_id).map(|(sig, stars)| Before {
                class: class_of(sig),
                stars: Some(*stars),
            })
        };
        let new_class = class_of(&c.significance);
        // A move inside one class (Pathogenic to Likely_pathogenic, same stars) is still a change.
        let moved = !first_run
            && prev
                .get(&c.variation_id)
                .is_some_and(|(sig, _)| !same_significance(sig, &c.significance));
        let Some(kind) =
            event_kind(old.as_ref(), new_class, c.stars, first_run).or(moved.then_some("significance_changed"))
        else {
            continue;
        };
        let gene = c.gene.as_deref();
        let sensitive = fact.is_some_and(|f| f.sensitive)
            || is_sensitive_gene(gene)
            || sensitive_gene_at(&c.chrom, c.key.1).is_some();
        let adult_onset = fact.is_some_and(|f| f.adult_onset);
        let carrier = match fact {
            Some(f) => f.carrier,
            None => !carrier_spans_overlapping(&c.chrom, c.key.1, c.key.1).is_empty(),
        };
        let minor_gated = minor && (sensitive || adult_onset || carrier);
        let shown_plp = match fact {
            Some(f) if f.plp => !f.hidden && !minor_gated,
            _ => old.as_ref().and_then(|o| o.stars).unwrap_or(0) >= ALERT_MIN_STARS && !minor_gated,
        };
        let alerted = alerts(kind, old.as_ref(), c.stars, is_sf_gene(gene), shown_plp);
        let old_json = match &old {
            None if first_run => json!({}),
            None => json!({"in_previous_release": false}),
            Some(o) => {
                let mut m = json!({"class": o.class, "stars": o.stars});
                if !first_run && let Some((sig, _)) = prev.get(&c.variation_id) {
                    m["significance"] = json!(sig);
                }
                if let Some(f) = fact {
                    m["fact_id"] = json!(f.fact_id);
                }
                m
            }
        };
        let new_json = json!({"class": new_class, "significance": c.significance, "stars": c.stars});
        let event_id: Uuid = sqlx::query_scalar(
            "insert into genome_event (profile_id, dataset_id, variation_id, kind, gene, sensitive, adult_onset, \
             carrier, old, new, release_from, release_to, alerted) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) returning id",
        )
        .bind(ds.profile_id)
        .bind(ds.id)
        .bind(c.variation_id)
        .bind(kind)
        .bind(gene)
        .bind(sensitive)
        .bind(adult_onset)
        .bind(carrier)
        .bind(&old_json)
        .bind(&new_json)
        .bind(&release_from)
        .bind(&release_to)
        .bind(alerted)
        .fetch_one(&mut *tx)
        .await?;
        report.events += 1;
        if !alerted {
            continue;
        }
        let withheld = minor_gated || (sensitive && !profile.genome_sensitive_ui);
        // Nothing that names the variant or the way its class moved goes into the item: a reviewer
        // may lack read:genome or read:sensitive, and the subject's switch may change later. The kind
        // stays on genome_event, read through the gated events route by event_id.
        let payload = json!({
            "event_id": event_id,
            "withheld": withheld,
            "unconfirmed": unconfirmed,
            "summary": review_summary(withheld, unconfirmed, "en"),
            "summary_es": review_summary(withheld, unconfirmed, "es"),
            "release_from": release_from,
            "release_to": release_to,
        });
        // The same change seen through another dataset of this profile already has its item, open or
        // already decided: a dismissed alert is not raised again. An item whose genome file was
        // retracted no longer stands for the change. Datasets run non-chip first within one run; an
        // open chip item found by a later genome file becomes the confirmed item.
        let earlier: Vec<(Uuid, bool, bool)> = sqlx::query_as(
            "select r.id, r.status = 'open', coalesce((r.payload->>'unconfirmed')::boolean, false) \
             from review_item r join genome_event g on g.id = r.ref_id join genome_dataset d on d.id = g.dataset_id \
             where r.kind = 'genome_reinterpretation' and r.profile_id = $1 and g.variation_id = $2 \
             and g.release_to = $3 and d.status <> 'retracted' and not exists \
             (select 1 from document doc where doc.id = d.document_id and doc.status = 'retracted') \
             order by r.created_at, r.id for update of r",
        )
        .bind(ds.profile_id)
        .bind(c.variation_id)
        .bind(&release_to)
        .fetch_all(&mut *tx)
        .await?;
        if !earlier.is_empty() {
            let chip_only = earlier.iter().all(|(_, _, chip)| *chip);
            if let Some((item, _, _)) = earlier.iter().find(|(_, open, _)| *open)
                && chip_only
                && !unconfirmed
            {
                sqlx::query("update review_item set ref_id = $2, document_id = $3, payload = $4 where id = $1")
                    .bind(item)
                    .bind(event_id)
                    .bind(ds.document_id)
                    .bind(&payload)
                    .execute(&mut *tx)
                    .await?;
            }
            continue;
        }
        report.alerts += 1;
        create_item(
            &mut tx,
            &ReviewItemIn {
                kind: "genome_reinterpretation".into(),
                household_id: Some(ds.household_id),
                profile_id: Some(ds.profile_id),
                payload,
                reasons: vec!["clinvar_change".into()],
                ref_table: Some("genome_event".into()),
                ref_id: Some(event_id),
                document_id: ds.document_id,
                ..ReviewItemIn::default()
            },
        )
        .await?;
    }
    sqlx::query("delete from clinvar_carried where dataset_id = $1")
        .bind(ds.id)
        .execute(&mut *tx)
        .await?;
    let ids: Vec<i64> = snapshot.iter().map(|c| c.variation_id).collect();
    let genes: Vec<Option<String>> = snapshot.iter().map(|c| c.gene.clone()).collect();
    let sigs: Vec<String> = snapshot.iter().map(|c| c.significance.clone()).collect();
    let stars: Vec<i16> = snapshot.iter().map(|c| c.stars).collect();
    sqlx::query(
        "insert into clinvar_carried (dataset_id, release_date, variation_id, gene, significance, stars) \
         select $1, $2, * from unnest($3::bigint[], $4::text[], $5::text[], $6::smallint[])",
    )
    .bind(ds.id)
    .bind(date)
    .bind(&ids)
    .bind(&genes)
    .bind(&sigs)
    .bind(&stars)
    .execute(&mut *tx)
    .await?;
    sqlx::query("update genome_dataset set clinvar_compared = $2 where id = $1")
        .bind(ds.id)
        .bind(date)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(report)
}

/// Before a genome file is retracted: each open ClinVar alert raised through that file moves to the
/// alerted event of the same change (same profile, VariationID and release) in another current
/// dataset of the person whose file stands, a genome file before a chip. The alert keeps standing
/// while a file still shows the change. Returns the items moved; the caller dismisses the rest.
pub async fn hand_over_alerts(conn: &mut PgConnection, document_id: Uuid) -> Result<Vec<Uuid>> {
    let open: Vec<(Uuid, Value)> = sqlx::query_as(
        "select id, payload from review_item where document_id = $1 and status = 'open' \
         and kind = 'genome_reinterpretation' order by id for update",
    )
    .bind(document_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut moved = Vec::new();
    for (item, mut payload) in open {
        let other: Option<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
            "select g2.id, d2.assay, d2.document_id from review_item r join genome_event g on g.id = r.ref_id \
             join genome_event g2 on g2.profile_id = g.profile_id and g2.variation_id = g.variation_id \
             and g2.release_to = g.release_to and g2.alerted and g2.id <> g.id \
             join genome_dataset d2 on d2.id = g2.dataset_id \
             where r.id = $1 and d2.status = 'current' and d2.document_id is distinct from $2 \
             and not exists (select 1 from document doc where doc.id = d2.document_id and doc.status = 'retracted') \
             order by d2.assay in ('chip', 'chip_imputed'), d2.imported_at, d2.id limit 1",
        )
        .bind(item)
        .bind(document_id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((event_id, assay, doc)) = other else {
            continue;
        };
        let unconfirmed = matches!(assay.as_str(), "chip" | "chip_imputed");
        let withheld = payload["withheld"].as_bool().unwrap_or(true);
        payload["event_id"] = json!(event_id);
        payload["unconfirmed"] = json!(unconfirmed);
        payload["summary"] = json!(review_summary(withheld, unconfirmed, "en"));
        payload["summary_es"] = json!(review_summary(withheld, unconfirmed, "es"));
        sqlx::query("update review_item set ref_id = $2, document_id = $3, payload = $4 where id = $1")
            .bind(item)
            .bind(event_id)
            .bind(doc)
            .bind(&payload)
            .execute(&mut *conn)
            .await?;
        moved.push(item);
    }
    Ok(moved)
}

/// `clinvar_now` for the findings of one profile: per ClinVar VariationID, the classification in the
/// newest release compared, from the snapshots of current datasets whose file is not retracted. Only
/// a snapshot whose release is on or before `as_of` counts: the store did not know a later one on
/// that date. Only our own class vocabulary is given, never the release's free text; the record's gene
/// rides beside it for the reader's gate and is never shown.
pub async fn now_by_variation(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
) -> Result<HashMap<i64, (Value, Option<String>)>> {
    let rows: Vec<(i64, String, i16, NaiveDate, Option<String>)> = sqlx::query_as(
        "select c.variation_id, c.significance, c.stars, c.release_date, c.gene from clinvar_carried c \
         join genome_dataset d on d.id = c.dataset_id where d.profile_id = $1 and d.status = 'current' \
         and not exists (select 1 from document doc where doc.id = d.document_id and doc.status = 'retracted') \
         and c.release_date <= $2 order by c.release_date, c.variation_id",
    )
    .bind(profile_id)
    .bind(as_of)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(v, sig, stars, date, gene)| {
            let now = json!({"class": class_of(&sig), "stars": stars, "release": date.to_string()});
            (v, (now, gene))
        })
        .collect())
}

/// Add `clinvar_now` to every finding in a `Findings` JSON whose `clinvar.variation_id` is in a
/// snapshot known at `as_of`. Findings with nothing compared yet stay as they are. The record passes
/// the reader's gate as its event does: a sensitive gene (ClinVar's or at the cited position) needs
/// the sensitive switch, and a minor sees no sensitive or carrier-panel record.
pub async fn attach_now(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    gate: &crate::Gate,
    findings: &mut Value,
) -> Result<()> {
    let now = now_by_variation(conn, profile_id, as_of).await?;
    if now.is_empty() {
        return Ok(());
    }
    for f in findings
        .get_mut("findings")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let Some(vid) = f.pointer("/clinvar/variation_id").and_then(Value::as_i64) else {
            continue;
        };
        let Some((value, gene)) = now.get(&vid) else {
            continue;
        };
        let site = f["variants"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|v| v["clinvar_variation_id"].as_i64() == Some(vid));
        let chrom = site.and_then(|v| v["chrom"].as_str()).unwrap_or_default();
        let pos = site.and_then(|v| v["pos"].as_i64()).unwrap_or_default();
        let sensitive = is_sensitive_gene(gene.as_deref()) || sensitive_gene_at(chrom, pos).is_some();
        let carrier = !carrier_spans_overlapping(chrom, pos, pos).is_empty();
        if (sensitive && !gate.sensitive) || (gate.minor && (sensitive || carrier)) {
            continue;
        }
        if let Some(o) = f.as_object_mut() {
            o.insert("clinvar_now".into(), value.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_stars_follow_clinvar_wording() {
        assert_eq!(class_of("Pathogenic/Likely_pathogenic"), "plp");
        assert_eq!(class_of("Likely_pathogenic,_low_penetrance"), "plp");
        assert_eq!(class_of("Conflicting_classifications_of_pathogenicity"), "conflicting");
        assert_eq!(class_of("Uncertain_significance"), "vus");
        assert_eq!(class_of("Benign/Likely_benign"), "blb");
        assert_eq!(class_of("drug_response"), "other");
        assert_eq!(stars_of("practice_guideline"), 4);
        assert_eq!(stars_of("reviewed_by_expert_panel"), 3);
        assert_eq!(stars_of("criteria_provided,_multiple_submitters,_no_conflicts"), 2);
        assert_eq!(stars_of("criteria_provided,_conflicting_classifications"), 1);
        assert_eq!(stars_of("criteria_provided,_single_submitter"), 1);
        assert_eq!(stars_of("no_assertion_criteria_provided"), 0);
        assert_eq!(stars_of("no_classification_provided"), 0);
    }

    #[test]
    fn trimming_matches_split_multiallelic_records() {
        assert_eq!(trim_alleles("GT", "GTT"), ("G".into(), "GT".into()));
        assert_eq!(trim_alleles("atct", "a"), ("ATCT".into(), "A".into()));
        assert_eq!(contig_rank("chrX"), Some(23));
        assert_eq!(contig_rank("MT"), Some(25));
        assert_eq!(contig_rank("7"), Some(7));
        assert_eq!(contig_rank("NW_009646201.1"), None);
    }

    #[test]
    fn every_template_has_es_and_en() {
        let keys: BTreeSet<String> = templates().into_iter().map(|(k, _, _)| k).collect();
        for k in &keys {
            for lang in ["en", "es"] {
                assert!(
                    TEMPLATES[k]
                        .get(lang)
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty()),
                    "{k} {lang}"
                );
            }
        }
        assert!(keys.len() >= 12, "{keys:?}");
    }
}
