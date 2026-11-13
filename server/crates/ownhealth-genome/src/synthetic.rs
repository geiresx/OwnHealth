//! Synthetic genome fixtures for the demo seed and the tests (`genome/synthetic.py`). No real
//! genomes, no real people.
//!
//! `alex_facts()`, `sam_facts()` and `lucia_facts()` return `ownhealth.genome-facts/v1` documents for
//! the Rivera family; `write_vcf()` writes a bgzipped, tabix-indexed VCF or gVCF; `alex_raw_files()`
//! builds Alex's calls VCF and gVCF that agree with his facts; `seed_genome()` stores a facts document
//! plus raw files and imports them. Positions are GRCh38 positions of the named rsIDs as the design
//! docs give them; genotypes, guidance text and scores are invented. The recommendation text is a
//! stand-in, never a quote: a real file quotes CPIC verbatim.

use std::io;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::{Error, Result};
use serde::Serialize;
use serde_json::ser::PrettyFormatter;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::FACTS_SCHEMA;
use crate::ingest::import;
use crate::schema::fact_id;

pub const CPIC_URL: &str = "https://cpicpgx.org/guidelines/";
pub const CPIC_GENES: [&str; 11] = [
    "CYP2C19", "CYP2D6", "CYP2C9", "CYP3A5", "SLCO1B1", "DPYD", "TPMT", "NUDT15", "VKORC1", "HLA-B", "UGT1A1",
];

pub fn contigs() -> Vec<String> {
    (1..=22)
        .map(|i| format!("chr{i}"))
        .chain(["chrX", "chrY", "chrM"].map(String::from))
        .collect()
}

/// A synthetic GA4GH VRS-shaped id (not a real VRS computation).
pub fn vrs_id(chrom: &str, pos: i64, reference: &str, alt: &str) -> String {
    let digest = Sha256::digest(format!("{chrom}:{pos}:{reference}:{alt}").as_bytes());
    format!("ga4gh:VA.{}", URL_SAFE_NO_PAD.encode(&digest[..24]))
}

pub fn stand_in(drug: &str, phenotype: &str) -> String {
    format!("Synthetic stand-in for the CPIC recommendation text for {drug} with {phenotype}.")
}

fn set(v: &mut Value, key: &str, value: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(key.into(), value);
    }
}

#[allow(clippy::too_many_arguments)]
fn variant(
    chrom: &str,
    pos: i64,
    reference: &str,
    alt: &str,
    rsid: &str,
    state: &str,
    gt: &str,
    extra: Value,
) -> Value {
    let zyg = match gt {
        "0/1" => "het",
        "1/1" => "hom",
        "0/0" => "ref",
        _ => "unknown",
    };
    let mut v = json!({
        "chrom": chrom, "pos": pos, "ref": reference, "alt": alt, "rsid": rsid,
        "vrs_id": vrs_id(chrom, pos, reference, alt),
    });
    if let Value::Object(extra) = extra {
        for (k, x) in extra {
            set(&mut v, &k, x);
        }
    }
    set(
        &mut v,
        "genotype",
        json!({"call_state": state, "gt": gt, "zygosity": zyg, "dp": 31, "gq": 60}),
    );
    v
}

fn v(chrom: &str, pos: i64, reference: &str, alt: &str, rsid: &str, state: &str, gt: &str) -> Value {
    variant(chrom, pos, reference, alt, rsid, state, gt, json!({}))
}

fn guidance(drug: &str, rxnorm: &str, phenotype: &str) -> Value {
    json!({
        "drug": drug,
        "rxnorm": rxnorm,
        "source": "CPIC",
        "level": "A",
        "phenotype": phenotype,
        "recommendation": stand_in(drug, phenotype),
        "url": CPIC_URL,
    })
}

struct Pgx<'a> {
    score: Option<f64>,
    basis: &'a str,
    conf: &'a str,
    extra: Value,
}

impl Default for Pgx<'_> {
    fn default() -> Self {
        Pgx {
            score: None,
            basis: "two callers agree",
            conf: "high",
            extra: json!({}),
        }
    }
}

fn pgx(
    gene: &str,
    diplotype: &str,
    phenotype: Option<&str>,
    variants: Vec<Value>,
    guidance: Vec<Value>,
    o: Pgx<'_>,
) -> Value {
    let mut value = json!({"diplotype": diplotype, "phenotype": phenotype});
    if let Some(score) = o.score {
        set(&mut value, "activity_score", json!(score));
    }
    let summary = format!("{gene} {diplotype}: {}.", phenotype.unwrap_or("None"));
    let mut fact = json!({
        "id": fact_id("pgx", gene, gene),
        "category": "pgx",
        "gene": gene,
        "variants": variants,
        "value": value,
        "summary": summary,
        "drug_guidance": guidance,
        "evidence": {"level": "E1", "native": "CPIC A", "tools": ["PharmCAT", "pypgx"]},
        "confidence": {"level": o.conf, "basis": o.basis},
        "sensitivity": {"level": "normal"},
        "origin": {"step": "pharmcat", "file": "pharmcat/report.json"},
    });
    if let Value::Object(extra) = o.extra {
        for (k, x) in extra {
            set(&mut fact, &k, x);
        }
    }
    fact
}

fn base(sample: &str, sex: &str, assay: Value, generated: &str, fingerprint: &str) -> Value {
    let mut a = json!({"build": "GRCh38"});
    if let Value::Object(m) = assay {
        for (k, x) in m {
            set(&mut a, &k, x);
        }
    }
    json!({
        "schema": FACTS_SCHEMA,
        "subject": {
            "sample_id": sample,
            "declared_sex": sex,
            "fingerprint": {"panel": "ownhealth-fp-v1", "genotypes": fingerprint},
        },
        "assay": a,
        "provenance": {
            "pipeline": "OwnGenome",
            "version": "1.4.2",
            "commit": "abc1234",
            "generated_utc": generated,
            "db_versions": {
                "clinvar": "2026-09-28",
                "pharmcat": "3.1.0",
                "cpic": "2026-09",
                "ipd_imgt_hla": "3.62",
                "pgs_catalog": "2026-08",
                "acmg_sf": "v3.3",
            },
        },
        "files": [],
        "coverage": [],
        "facts": [],
        "qc": {"sex_check": "pass", "contamination": 0.002},
    })
}

fn apoe(state_429358: &str, gt_429358: &str) -> Value {
    json!({
        "id": fact_id("risk_marker", "APOE", "apoe_haplotype"),
        "category": "risk_marker",
        "gene": "APOE",
        "variants": [
            v("chr19", 44908684, "T", "C", "rs429358", state_429358, gt_429358),
            v("chr19", 44908822, "C", "T", "rs7412", "ref", "0/0"),
        ],
        "value": {"haplotype": "e3/e4"},
        "summary": "APOE e3/e4. Linked to a higher average Alzheimer's disease risk than e3/e3; it is not a diagnosis.",
        "evidence": {"level": "E2", "native": "replicated association with effect size"},
        "confidence": {"level": "high", "basis": "both sites called, GQ > 50"},
        "sensitivity": {"level": "adult_onset", "reason": "APOE e4"},
    })
}

fn cftr_carrier() -> Value {
    let var = variant(
        "chr7",
        117559590,
        "ATCT",
        "A",
        "rs113993960",
        "alt",
        "0/1",
        json!({"hgvs_p": "p.Phe508del", "clinvar_variation_id": 7105}),
    );
    let id = fact_id("carrier", "CFTR", var["vrs_id"].as_str().unwrap_or_default());
    json!({
        "id": id,
        "category": "carrier",
        "gene": "CFTR",
        "variants": [var],
        "condition": {"name": "Cystic fibrosis", "inheritance": "AR"},
        "summary": "Heterozygous carrier of a pathogenic CFTR variant. Carriers are not affected; it matters for a partner's testing.",
        "evidence": {"level": "E1", "native": "ClinVar Pathogenic, 3 stars", "clinvar_stars": 3},
        "confidence": {"level": "high", "basis": "GQ 99, DP 35"},
        "sensitivity": {"level": "normal"},
    })
}

fn prs(percentile: i64) -> Value {
    json!({
        "id": fact_id("prs", "", "PGS000018"),
        "category": "prs",
        "value": {
            "condition": "coronary artery disease",
            "pgs_id": "PGS000018",
            "percentile": percentile,
            "matched_pct": 97.2,
            "reference": "HGDP+1kGP",
            "most_similar_pop": "EUR",
            "evaluated_in": ["EUR"],
        },
        "summary": format!("Higher than {percentile}% of the EUR reference group for this score."),
        "evidence": {"level": "E3", "native": "PGS Catalog score with published evaluation"},
        "confidence": {"level": "medium", "basis": "matched 97% of variants; ancestry inside evaluation set"},
        "limitations": ["Percentile relative to the EUR reference group; not an absolute risk."],
        "sensitivity": {"level": "normal"},
    })
}

fn single_site(category: &str, gene: &str, var: Value, rest: Value) -> Value {
    let id = fact_id(category, gene, var["vrs_id"].as_str().unwrap_or_default());
    let mut fact = json!({"id": id, "category": category, "gene": gene, "variants": [var]});
    if let Value::Object(m) = rest {
        for (k, x) in m {
            set(&mut fact, &k, x);
        }
    }
    fact
}

/// Alex Rivera, sample AR-01, 30x WGS (synthetic).
pub fn alex_facts() -> Value {
    let mut doc = base(
        "AR-01",
        "male",
        json!({"type": "wgs", "platform": "Illumina 30x", "mean_depth": 33.8, "callable_fraction": 0.94}),
        "2026-10-01T09:15:00Z",
        "0121012001201210120120012012102101201",
    );
    set(&mut doc["subject"], "inferred_karyotype", json!("XY"));
    let mut coverage: Vec<Value> = CPIC_GENES
        .iter()
        .map(|g| json!({"scope": "gene", "id": g, "panel": "cpic", "state": "assessed", "fraction_callable": 0.999}))
        .collect();
    set(&mut coverage[1], "note", json!("pypgx and Cyrius agree"));
    coverage.extend([
        json!({"scope": "gene", "id": "BRCA1", "panel": "acmg_sf_v3.3", "state": "assessed", "fraction_callable": 0.997}),
        json!({"scope": "gene", "id": "BRCA2", "panel": "acmg_sf_v3.3", "state": "assessed", "fraction_callable": 0.998}),
        json!({"scope": "gene", "id": "PMS2", "panel": "acmg_sf_v3.3", "state": "partial", "reason": "paralog", "fraction_callable": 0.81}),
        json!({"scope": "gene", "id": "CFTR", "panel": "carrier", "state": "assessed", "fraction_callable": 0.996}),
        json!({"scope": "gene", "id": "FMR1", "panel": "carrier", "state": "not_assessed", "reason": "not_run"}),
        json!({"scope": "gene", "id": "SMN1", "panel": "carrier", "state": "not_assessed", "reason": "paralog"}),
        json!({"scope": "panel", "id": "repeat_expansions", "panel": "other", "state": "not_assessed", "reason": "not_run"}),
    ]);
    doc["coverage"] = json!(coverage);
    let im = "Intermediate Metabolizer";
    let nm = "Normal Metabolizer";
    let df = "Decreased Function";
    doc["facts"] = json!([
        pgx(
            "CYP2C19",
            "*1/*2",
            Some(im),
            vec![v("chr10", 94781859, "G", "A", "rs4244285", "alt", "0/1")],
            vec![guidance("clopidogrel", "32968", im), guidance("omeprazole", "7646", im)],
            Pgx::default(),
        ),
        pgx(
            "CYP2D6",
            "*1/*41",
            Some(nm),
            vec![v("chr22", 42127803, "C", "T", "rs28371725", "alt", "0/1")],
            vec![guidance("codeine", "2670", nm)],
            Pgx { score: Some(1.5), basis: "pypgx and Cyrius from reads agree", ..Pgx::default() },
        ),
        pgx(
            "SLCO1B1",
            "*1/*5",
            Some(df),
            vec![v("chr12", 21178615, "T", "C", "rs4149056", "alt", "0/1")],
            vec![guidance("atorvastatin", "83367", df), guidance("simvastatin", "36567", df)],
            Pgx { basis: "single SNV, GQ 58", extra: json!({"related_loinc": ["2157-6", "1742-6"]}), ..Pgx::default() },
        ),
        pgx(
            "DPYD",
            "*1/*1",
            Some(nm),
            vec![
                v("chr1", 97450058, "C", "T", "rs3918290", "ref", "0/0"),
                v("chr1", 97082391, "T", "A", "rs67376798", "ref", "0/0"),
            ],
            vec![guidance("fluorouracil", "4492", nm), guidance("capecitabine", "194000", nm)],
            Pgx { score: Some(2.0), ..Pgx::default() },
        ),
        pgx(
            "TPMT",
            "*1/*1",
            Some(nm),
            vec![
                v("chr6", 18130687, "T", "C", "rs1142345", "ref", "0/0"),
                v("chr6", 18138997, "C", "T", "rs1800460", "ref", "0/0"),
            ],
            vec![guidance("azathioprine", "1256", nm)],
            Pgx::default(),
        ),
        pgx(
            "VKORC1",
            "-1639G>A het",
            None,
            vec![v("chr16", 31096368, "C", "T", "rs9923231", "alt", "0/1")],
            vec![guidance("warfarin", "11289", "VKORC1 -1639 G/A")],
            Pgx::default(),
        ),
        pgx(
            "CYP2C9",
            "*1/*1",
            Some(nm),
            vec![
                v("chr10", 94942290, "C", "T", "rs1799853", "ref", "0/0"),
                v("chr10", 94981296, "A", "C", "rs1057910", "ref", "0/0"),
            ],
            vec![guidance("warfarin", "11289", nm), guidance("celecoxib", "140587", nm)],
            Pgx { score: Some(2.0), ..Pgx::default() },
        ),
        {
            "id": fact_id("hla", "HLA-B", "HLA-B*58:01"),
            "category": "hla",
            "gene": "HLA-B",
            "value": {"alleles": ["HLA-B*07:02", "HLA-B*44:02"], "marker": "HLA-B*58:01", "present": false},
            "summary": "HLA-B*58:01 not detected. CPIC ties this allele to severe skin reactions with allopurinol.",
            "drug_guidance": [guidance("allopurinol", "519", "HLA-B*58:01 negative")],
            "evidence": {"level": "E1", "native": "CPIC A"},
            "confidence": {"level": "medium", "basis": "T1K 2-field typing from reads"},
            "sensitivity": {"level": "normal"},
        },
        apoe("alt", "0/1"),
        single_site(
            "risk_marker",
            "LPA",
            v("chr6", 160589086, "A", "G", "rs10455872", "alt", "0/1"),
            json!({
                "value": {"kiv2_copies": 18, "kiv2_ci95": [15, 22]},
                "summary": "LPA risk allele and a low KIV-2 copy estimate, both linked to higher Lp(a). A blood Lp(a) value is the measurement that counts.",
                "evidence": {"level": "E2", "native": "replicated association"},
                "confidence": {"level": "medium", "basis": "depth estimate, experimental step"},
                "limitations": ["The SNPs were found in European cohorts."],
                "sensitivity": {"level": "normal"},
                "related_loinc": ["43583-4"],
            }),
        ),
        cftr_carrier(),
        prs(86),
        single_site(
            "trait",
            "LCT",
            v("chr2", 135851076, "G", "A", "rs4988235", "alt", "0/1"),
            json!({
                "value": {"trait": "lactase persistence", "call": "likely"},
                "summary": "LCT -13910 C/T: lactase persistence likely.",
                "evidence": {"level": "E2", "native": "replicated association"},
                "confidence": {"level": "high", "basis": "GQ 60"},
                "sensitivity": {"level": "normal"},
            }),
        ),
        single_site(
            "trait",
            "MTHFR",
            v("chr1", 11796321, "G", "A", "rs1801133", "alt", "0/1"),
            json!({
                "summary": "MTHFR C677T heterozygous: enzyme note only.",
                "evidence": {"level": "E4", "native": "not supported as health advice (PMID 23288205)"},
                "confidence": {"level": "high", "basis": "GQ 60"},
                "sensitivity": {"level": "normal"},
                "hidden_by_default": true,
            }),
        ),
    ]);
    doc
}

/// Sam Rivera, sample SR-01, a variant-only VCF run, facts only (CYP2D6 *1/*1 for the codeine example).
pub fn sam_facts() -> Value {
    let mut doc = base(
        "SR-01",
        "female",
        json!({"type": "vcf_only", "platform": "clinical exome VCF"}),
        "2026-08-19T10:00:00Z",
        "1020120120102012021012012001202102012",
    );
    let nm = "Normal Metabolizer";
    let mut coverage: Vec<Value> = ["CYP2C19", "CYP2D6"]
        .iter()
        .map(|g| json!({"scope": "gene", "id": g, "panel": "cpic", "state": "assessed"}))
        .collect();
    coverage.push(
        json!({"scope": "gene", "id": "CYP2D6-SV", "panel": "cpic", "state": "not_assessed", "reason": "no_bam"}),
    );
    doc["coverage"] = json!(coverage);
    doc["facts"] = json!([
        pgx(
            "CYP2D6",
            "*1/*1",
            Some(nm),
            vec![],
            vec![guidance("codeine", "2670", nm)],
            Pgx {
                score: Some(2.0),
                conf: "medium",
                basis: "single caller; SVs not assessed",
                ..Pgx::default()
            },
        ),
        pgx(
            "CYP2C19",
            "*1/*1",
            Some(nm),
            vec![],
            vec![guidance("clopidogrel", "32968", nm)],
            Pgx {
                conf: "medium",
                basis: "single caller",
                ..Pgx::default()
            },
        ),
    ]);
    doc
}

/// Lucía Rivera (a minor), sample LR-01: PGx shown to guardians, the rest withheld by the minor rule.
pub fn lucia_facts() -> Value {
    let mut doc = base(
        "LR-01",
        "female",
        json!({"type": "wgs", "platform": "Illumina 30x", "mean_depth": 31.0}),
        "2026-09-30T08:00:00Z",
        "0121010120120120120120102012012012010",
    );
    doc["coverage"] = json!(
        ["TPMT", "NUDT15"]
            .iter()
            .map(|g| json!({"scope": "gene", "id": g, "panel": "cpic", "state": "assessed"}))
            .collect::<Vec<_>>()
    );
    let im = "Intermediate Metabolizer";
    doc["facts"] = json!([
        pgx(
            "TPMT",
            "*1/*3A",
            Some(im),
            vec![
                v("chr6", 18130687, "T", "C", "rs1142345", "alt", "0/1"),
                v("chr6", 18138997, "C", "T", "rs1800460", "alt", "0/1"),
            ],
            vec![
                guidance("mercaptopurine", "103", im),
                guidance("azathioprine", "1256", im)
            ],
            Pgx::default(),
        ),
        cftr_carrier(),
        apoe("alt", "0/1"),
        prs(64),
    ]);
    doc
}

// ------------------------------------------------------------------ VCFs ----

/// One synthetic VCF row; `alt` "<NON_REF>" with `end` is a gVCF reference block.
#[derive(Debug, Clone, PartialEq)]
pub struct VcfRow {
    pub chrom: String,
    pub pos: i64,
    pub reference: String,
    pub alt: String,
    pub gt: String,
    pub id: String,
    pub dp: i64,
    pub gq: i64,
    pub end: Option<i64>,
}

impl VcfRow {
    pub fn new(chrom: &str, pos: i64, reference: &str, alt: &str, gt: &str) -> VcfRow {
        VcfRow {
            chrom: chrom.into(),
            pos,
            reference: reference.into(),
            alt: alt.into(),
            gt: gt.into(),
            id: ".".into(),
            dp: 30,
            gq: 60,
            end: None,
        }
    }

    pub fn id(mut self, id: &str) -> VcfRow {
        self.id = id.into();
        self
    }

    pub fn quality(mut self, gq: i64, dp: i64) -> VcfRow {
        self.gq = gq;
        self.dp = dp;
        self
    }

    pub fn gq(mut self, gq: i64) -> VcfRow {
        self.gq = gq;
        self
    }

    pub fn block_end(mut self, end: i64) -> VcfRow {
        self.end = Some(end);
        self
    }
}

/// A bgzipped VCF and its tabix index, in memory, named as `write_vcf` named the files.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFile {
    /// `AR-01.vcf.gz`; the index is `<name>.tbi`.
    pub name: String,
    pub data: Vec<u8>,
    pub index: Vec<u8>,
}

impl RawFile {
    pub fn index_name(&self) -> String {
        format!("{}.tbi", self.name)
    }
}

/// Rows as a bgzipped VCF with a tabix index (`name` ends in `.vcf.gz`).
pub fn write_vcf(name: &str, rows: &[VcfRow], sample: &str) -> io::Result<RawFile> {
    let order = contigs();
    let mut header = vec![
        "##fileformat=VCFv4.2".to_string(),
        "##INFO=<ID=END,Number=1,Type=Integer,Description=\"End of the reference block\">".into(),
        "##FORMAT=<ID=GT,Number=1,Type=String,Description=\"Genotype\">".into(),
        "##FORMAT=<ID=DP,Number=1,Type=Integer,Description=\"Depth\">".into(),
        "##FORMAT=<ID=GQ,Number=1,Type=Integer,Description=\"Genotype quality\">".into(),
        "##FORMAT=<ID=MIN_DP,Number=1,Type=Integer,Description=\"Minimum depth in the block\">".into(),
    ];
    header.extend(order.iter().map(|c| format!("##contig=<ID={c}>")));
    header.push(
        [
            "#CHROM", "POS", "ID", "REF", "ALT", "QUAL", "FILTER", "INFO", "FORMAT", sample,
        ]
        .join("\t"),
    );
    let mut sorted: Vec<&VcfRow> = rows.iter().collect();
    sorted.sort_by_key(|r| (order.iter().position(|c| *c == r.chrom).unwrap_or(usize::MAX), r.pos));
    let lines: Vec<String> = sorted
        .iter()
        .map(|r| {
            let (info, fmt, val) = match r.end {
                Some(end) => (
                    format!("END={end}"),
                    "GT:DP:GQ:MIN_DP",
                    format!("{}:{}:{}:{}", r.gt, r.dp, r.gq, r.dp),
                ),
                None => (".".to_string(), "GT:DP:GQ", format!("{}:{}:{}", r.gt, r.dp, r.gq)),
            };
            [
                r.chrom.as_str(),
                &r.pos.to_string(),
                &r.id,
                &r.reference,
                &r.alt,
                "50",
                "PASS",
                &info,
                fmt,
                &val,
            ]
            .join("\t")
        })
        .collect();
    let (data, index) = crate::vcf::write_indexed(&header, &lines)?;
    Ok(RawFile {
        name: name.into(),
        data,
        index,
    })
}

/// Alex's calls VCF (every alt site of his facts plus a no-call) and gVCF (reference blocks around
/// every ref site, one block below the GQ threshold). Agrees with `alex_facts()`.
pub fn alex_raw_files() -> io::Result<Vec<(String, RawFile)>> {
    let mut calls = Vec::new();
    let mut blocks = Vec::new();
    let facts = alex_facts();
    for fact in facts["facts"].as_array().into_iter().flatten() {
        for var in fact.get("variants").and_then(Value::as_array).into_iter().flatten() {
            let g = &var["genotype"];
            let chrom = var["chrom"].as_str().unwrap_or_default();
            let pos = var["pos"].as_i64().unwrap_or_default();
            if g["call_state"] == "alt" {
                calls.push(
                    VcfRow::new(
                        chrom,
                        pos,
                        var["ref"].as_str().unwrap_or_default(),
                        var["alt"].as_str().unwrap_or_default(),
                        g["gt"].as_str().unwrap_or_default(),
                    )
                    .id(var.get("rsid").and_then(Value::as_str).unwrap_or(".")),
                );
            } else {
                blocks.push(VcfRow::new(chrom, pos - 40, "N", "<NON_REF>", "0/0").block_end(pos + 40));
            }
        }
    }
    calls.push(VcfRow::new("chr3", 1_000_500, "A", "G", "./.").quality(0, 2)); // a site that failed calling
    blocks.push(
        VcfRow::new("chr3", 2_000_000, "N", "<NON_REF>", "0/0")
            .quality(8, 4)
            .block_end(2_000_200),
    ); // low quality
    let gvcf_rows: Vec<VcfRow> = blocks.into_iter().chain(calls.iter().cloned()).collect();
    Ok(vec![
        ("calls_vcf".into(), write_vcf("AR-01.vcf.gz", &calls, "AR-01")?),
        ("gvcf".into(), write_vcf("AR-01.g.vcf.gz", &gvcf_rows, "AR-01")?),
    ])
}

/// Put raw files into the blob store and list them in a copy of the facts. Returns (facts, attachments).
pub fn with_files(doc: &Value, blobs: &BlobStore, raw: &[(String, RawFile)]) -> Result<(Value, Vec<(String, String)>)> {
    let mut doc = doc.clone();
    let mut attachments = Vec::new();
    for (role, file) in raw {
        let sha = blobs.put_bytes(&file.data)?;
        let isha = blobs.put_bytes(&file.index)?;
        if let Some(files) = doc.get_mut("files").and_then(Value::as_array_mut) {
            files.push(json!({
                "role": role,
                "path": format!("vcf/{}", file.name),
                "sha256": sha,
                "index_path": format!("vcf/{}", file.index_name()),
                "index_sha256": isha,
                "bytes": file.data.len(),
            }));
        }
        attachments.push((file.name.clone(), sha));
        attachments.push((file.index_name(), isha));
    }
    Ok((doc, attachments))
}

/// `json.dumps(facts, indent=1, ensure_ascii=False)`.
pub fn facts_bytes(facts: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PrettyFormatter::with_indent(b" "));
    facts.serialize(&mut ser).map_err(Error::Json)?;
    Ok(out)
}

/// Store a facts document as an uploaded document and import it. Returns (document_id, summary).
#[allow(clippy::too_many_arguments)]
pub async fn seed_genome(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    household_id: Uuid,
    profile_id: Uuid,
    facts: &Value,
    attachments: &[(String, String)],
    filename: &str,
    imported_at: Option<DateTime<Utc>>,
) -> Result<(Uuid, ImportSummary)> {
    let data = facts_bytes(facts)?;
    let sha = blobs.put_bytes(&data)?;
    let doc = NewDocument {
        filename: Some(filename),
        channel: "genome",
        doc_type: Some("genomics"),
        ..NewDocument::upload(
            household_id,
            Some(profile_id),
            &sha,
            "application/json",
            data.len() as i64,
        )
    };
    let r = create_document(&mut *conn, &doc).await?;
    let summary = import(conn, blobs, r.id, attachments, None, imported_at).await?;
    Ok((r.id, summary))
}

/// Sam's facts as a consumer chip with one planted rare BRCA2 P/LP (synthetic), unconfirmed as
/// importer rule 6 requires: the Genome screen's chip PPV note (genomics.md 4).
pub fn chip_with_rare_brca2() -> Value {
    let mut doc = sam_facts();
    doc["assay"] = json!({"type": "chip", "build": "GRCh38", "platform": "consumer array (synthetic)"});
    for f in doc["facts"].as_array_mut().into_iter().flatten() {
        let low = f["gene"] == "CYP2D6";
        f["confidence"]["level"] = json!(if low { "low" } else { "medium" });
    }
    let var = json!({
        "chrom": "chr13", "pos": 32340300, "ref": "GT", "alt": "G", "rsid": "rs80359550",
        "hgvs_c": "c.5946del", "hgvs_p": "p.Ser1982fs", "clinvar_variation_id": 9322,
        "vrs_id": vrs_id("chr13", 32340300, "GT", "G"),
        "genotype": {"call_state": "alt", "gt": "0/1", "zygosity": "het"},
    });
    let id = fact_id("acmg_sf", "BRCA2", var["vrs_id"].as_str().unwrap_or_default());
    if let Some(facts) = doc["facts"].as_array_mut() {
        facts.push(json!({
            "id": id,
            "category": "acmg_sf",
            "gene": "BRCA2",
            "variants": [var],
            "condition": {"name": "Hereditary breast and ovarian cancer", "inheritance": "AD"},
            "summary": "A rare BRCA2 variant classified pathogenic in ClinVar, typed on a chip (synthetic).",
            "evidence": {"level": "E1", "native": "ClinVar Pathogenic, 3 stars", "clinvar_stars": 3},
            "confidence": {"level": "unconfirmed", "basis": "rare variant typed on an array"},
            "sensitivity": {"level": "sensitive", "reason": "ACMG SF gene"},
            "confirm_clinically": true,
        }));
    }
    doc
}
