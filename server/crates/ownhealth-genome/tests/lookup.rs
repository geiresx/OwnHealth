//! lookup_variant: call_state for every site, sensitive gating of raw lookups, regions, the
//! 1,000-rsID exit criterion of ROADMAP v0.1 (`tests/genome/test_genome_lookup.py`).

mod common;

use std::collections::BTreeMap;

use common::{Env, R, env};
use ownhealth_genome::lookup::UNKNOWN_NOTE;
use ownhealth_genome::sensitive::spans_overlapping;
use ownhealth_genome::synthetic::{self, VcfRow, write_vcf};
use ownhealth_genome::{CALL_STATES, Gate, GenomeQueryError, RefSite, VariantQuery, load_ref_sites, lookup_variant};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

fn closed() -> Gate {
    Gate::new(false, false)
}

fn open() -> Gate {
    Gate::new(true, false)
}

fn rsid(r: &str) -> VariantQuery<'_> {
    VariantQuery {
        rsid: Some(r),
        ..Default::default()
    }
}

fn hgvs(h: &str) -> VariantQuery<'_> {
    VariantQuery {
        hgvs: Some(h),
        ..Default::default()
    }
}

fn region(r: &str) -> VariantQuery<'_> {
    VariantQuery {
        region: Some(r),
        ..Default::default()
    }
}

async fn try_look(
    e: &Env,
    c: &mut PgConnection,
    p: Uuid,
    gate: &Gate,
    q: VariantQuery<'_>,
) -> anyhow::Result<Result<Value, GenomeQueryError>> {
    Ok(lookup_variant(c, &e.blobs, p, gate, &q).await?)
}

async fn look(e: &Env, c: &mut PgConnection, p: Uuid, gate: &Gate, q: VariantQuery<'_>) -> anyhow::Result<Value> {
    try_look(e, c, p, gate, q)
        .await?
        .map_err(|err| anyhow::anyhow!("{}: {}", err.code, err.message))
}

fn state(out: &Value) -> String {
    let calls = out["calls"].as_array().cloned().unwrap_or_default();
    assert_eq!(calls.len(), 1, "{out}");
    calls[0]["call_state"].as_str().unwrap_or_default().to_string()
}

fn site(rsid: &str, chrom: &str, pos: i32, reference: &str) -> RefSite {
    RefSite {
        rsid: rsid.into(),
        chrom: chrom.into(),
        pos,
        reference: reference.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn alt_call_from_the_vcf() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let out = look(&e, &mut c, e.profile("alex"), &closed(), rsid("rs4244285")).await?;
    let call = &out["calls"][0];
    assert!(call["call_state"] == "alt" && call["genotype"] == "G/A" && call["zygosity"] == "het");
    assert!(call["basis"] == "vcf" && call["gq"] == 60);
    assert!(call["src"].as_str().unwrap_or_default().starts_with("src_gd_"));
    assert!(out["variant"]["gene"] == "CYP2C19" && out["variant"]["pos"] == 94781859);
    assert_eq!(out["cited_by_findings"].as_array().map(Vec::len), Some(1));
    assert!(out["withheld"].is_null());
    Ok(())
}

#[tokio::test]
async fn ref_inside_a_passing_gvcf_block() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let out = look(&e, &mut c, e.profile("alex"), &closed(), rsid("rs3918290")).await?;
    assert_eq!(state(&out), "ref");
    assert!(out["calls"][0]["basis"] == "gvcf_block" && out["calls"][0]["genotype"] == "C/C");
    Ok(())
}

#[tokio::test]
async fn unknown_states_by_position() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let low = look(&e, &mut c, alex, &closed(), hgvs("NC_000003.12:g.2000100A>G")).await?;
    assert!(state(&low) == "not_covered" && low["calls"][0]["reason"] == "block_below_quality");
    let none = look(&e, &mut c, alex, &closed(), hgvs("NC_000003.12:g.5000000A>G")).await?;
    assert!(state(&none) == "not_covered" && none["calls"][0]["reason"] == "no_reference_block");
    let failed = look(&e, &mut c, alex, &closed(), hgvs("NC_000003.12:g.1000500A>G")).await?;
    assert_eq!(state(&failed), "no_call");
    assert!(
        none["limitations"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|l| l == UNKNOWN_NOTE)
    );
    Ok(())
}

#[tokio::test]
async fn not_assessed_gene_answers_not_covered() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let fmr1 = RefSite {
        gene: Some("FMR1".into()),
        ..site("rs999000001", "chrX", 147912100, "C")
    };
    load_ref_sites(&mut c, &[fmr1], "test").await?;
    let out = look(&e, &mut c, e.profile("alex"), &closed(), rsid("rs999000001")).await?;
    assert!(state(&out) == "not_covered" && out["calls"][0]["reason"] == "not_run");
    assert_eq!(out["coverage"]["state"], "not_assessed");
    Ok(())
}

#[tokio::test]
async fn sensitive_gene_lookup_is_gated() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    // a withheld finding never resolves an rsID (that would say it exists), so the curated index does
    let sites = [
        site("rs429358", "chr19", 44908684, "T"),
        site("rs113993960", "chr7", 117559590, "ATCT"),
    ];
    load_ref_sites(&mut c, &sites, "t").await?;
    let shut = look(&e, &mut c, alex, &closed(), rsid("rs429358")).await?;
    assert!(shut["withheld"] == "withheld_sensitive" && shut["calls"] == json!([]));
    let text = shut.to_string();
    assert!(!text.contains("0/1") && !text.contains("T/C"));
    let opened = look(&e, &mut c, alex, &open(), rsid("rs429358")).await?;
    assert!(state(&opened) == "alt" && opened["variant"]["gene"] == "APOE");
    // a position in APOE that no finding cites is gated by the gene list, not by the facts
    let by_pos = look(&e, &mut c, alex, &closed(), hgvs("NC_000019.10:g.44906000A>G")).await?;
    assert_eq!(by_pos["withheld"], "withheld_sensitive");
    // the minor rule wins over an open switch, and covers carrier sites too
    let minor = Gate::new(true, true);
    assert_eq!(
        look(&e, &mut c, alex, &minor, rsid("rs429358")).await?["withheld"],
        "withheld_minor"
    );
    assert_eq!(
        look(&e, &mut c, alex, &minor, rsid("rs113993960")).await?["withheld"],
        "withheld_minor"
    );
    assert_eq!(state(&look(&e, &mut c, alex, &minor, rsid("rs4244285")).await?), "alt"); // PGx stays visible
    Ok(())
}

/// A position in a carrier-panel gene that no finding cites is withheld for a minor, and a region over
/// it is clipped: the fixed list gates, not the person's own findings (genomics.md 7).
#[tokio::test]
async fn minor_lookups_are_gated_by_the_carrier_panel_spans() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let minor = Gate::new(true, true);
    let uncited = look(&e, &mut c, alex, &minor, hgvs("NC_000007.14:g.117560000A>G")).await?;
    assert!(uncited["withheld"] == "withheld_minor" && uncited["calls"] == json!([]));
    let adult = look(&e, &mut c, alex, &open(), hgvs("NC_000007.14:g.117560000A>G")).await?;
    assert!(adult["withheld"].is_null());
    let r = look(&e, &mut c, alex, &minor, region("chr7:117559000-117560000")).await?;
    assert_eq!(r["records"], json!([]));
    let genes: Vec<&Value> = r["clipped"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| &c["gene"])
        .collect();
    assert_eq!(genes, vec![&json!("CFTR")]);
    assert_eq!(r["clipped"][0]["reason"], "withheld_minor");
    Ok(())
}

/// Lucía's CFTR carrier finding is withheld by the minor rule. Asking for its rsID must answer the way
/// it does for Sam, who has no such finding, and a region answer carries no count of hidden rows.
#[tokio::test]
async fn a_withheld_answer_does_not_depend_on_the_persons_findings() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    e.seed(&mut c, "lucia", &synthetic::lucia_facts(), &[]).await?;
    e.seed(&mut c, "sam", &synthetic::sam_facts(), &[]).await?;
    let minor = Gate::new(false, true);
    let mut codes = Vec::new();
    for slug in ["lucia", "sam"] {
        let err = try_look(&e, &mut c, e.profile(slug), &minor, rsid("rs113993960"))
            .await?
            .err()
            .map(|e| e.code);
        codes.push(err);
    }
    assert_eq!(codes, vec![Some("rsid_unknown"), Some("rsid_unknown")]);
    let out = look(&e, &mut c, e.profile("alex"), &minor, region("chr10:94781000-94782000")).await?;
    assert!(out.get("withheld_records").is_none());
    let pos: Vec<&Value> = out["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| &r["pos"])
        .collect();
    assert_eq!(pos, vec![&json!(94781859)]);
    Ok(())
}

#[tokio::test]
async fn region_clips_sensitive_genes_and_pages() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let shut = look(&e, &mut c, alex, &closed(), region("chr19:44900000-44910000")).await?;
    assert!(shut["records"] == json!([]) && shut["clipped"][0]["gene"] == "APOE");
    let opened = look(&e, &mut c, alex, &open(), region("chr19:44900000-44910000")).await?;
    let pos: Vec<&Value> = opened["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| &r["pos"])
        .collect();
    assert_eq!(pos, vec![&json!(44908684)]);
    assert_eq!(opened["clipped"], json!([]));
    let err = try_look(&e, &mut c, alex, &closed(), region("chr1:1-1000001"))
        .await?
        .err();
    assert_eq!(err.map(|e| e.code), Some("region_too_large"));
    Ok(())
}

#[tokio::test]
async fn bad_queries() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let cases = [
        (rsid("rs123456789012"), "rsid_unknown"),
        (rsid("4244285"), "bad_input"),
        (hgvs("NM_000410.4:c.845G>A"), "hgvs_unresolved"),
        (
            VariantQuery {
                rsid: Some("rs1"),
                region: Some("chr1:1-2"),
                ..Default::default()
            },
            "bad_input",
        ),
        (VariantQuery::default(), "bad_input"),
    ];
    for (q, code) in cases {
        let err = try_look(&e, &mut c, alex, &closed(), q).await?.err();
        assert_eq!(err.map(|e| e.code), Some(code), "{q:?}");
    }
    let err = try_look(&e, &mut c, e.profile("carmen"), &closed(), rsid("rs4244285"))
        .await?
        .err();
    assert_eq!(err.map(|e| e.code), Some("no_genome"));
    Ok(())
}

#[tokio::test]
async fn facts_only_source_answers_cited_sites() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.seed(&mut c, "lucia", &synthetic::lucia_facts(), &[]).await?;
    let out = look(
        &e,
        &mut c,
        e.profile("lucia"),
        &Gate::new(false, true),
        rsid("rs1142345"),
    )
    .await?;
    assert!(state(&out) == "alt" && out["calls"][0]["basis"] == "facts_file");
    Ok(())
}

// --------------------------------------------- exit criterion: 1,000 rsIDs ----

const N: usize = 1000;

/// N synthetic positions outside every sensitive gene (those are gated, tested above).
fn sites() -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let mut j: i64 = 0;
    while out.len() < N {
        let (chrom, pos) = (format!("chr{}", j % 22 + 1), 1_000_000 + j * 1000);
        if spans_overlapping(&chrom, pos, pos).is_empty() {
            out.push((chrom, pos));
        }
        j += 1;
    }
    out
}

fn facts(sample: &str, assay: &str, fp: &str) -> Value {
    let mut doc = synthetic::sam_facts();
    doc["subject"]["sample_id"] = json!(sample);
    doc["subject"]["fingerprint"]["genotypes"] = json!(fp);
    doc["assay"] = json!({"type": assay, "build": "GRCh38"});
    if assay == "chip" {
        // the chip confidence caps (importer rule 6)
        for f in doc["facts"].as_array_mut().into_iter().flatten() {
            let low = f["gene"] == "CYP2D6";
            f["confidence"]["level"] = json!(if low { "low" } else { "medium" });
        }
    }
    doc
}

/// 1,000 curated rsIDs; a WGS source (calls + gVCF) on Carmen and a chip (calls + site list) on Sam.
async fn thousand(e: &Env, c: &mut PgConnection) -> anyhow::Result<Vec<String>> {
    let sites = sites();
    let rows: Vec<RefSite> = sites
        .iter()
        .enumerate()
        .map(|(i, (chrom, pos))| RefSite {
            alts: vec!["G".into()],
            ..site(
                &format!("rs9{i:08}"),
                chrom,
                i32::try_from(*pos).unwrap_or_default(),
                "A",
            )
        })
        .collect();
    load_ref_sites(c, &rows, "test").await?;
    let (mut calls, mut gvcf, mut chip, mut probed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, (chrom, p)) in sites.iter().enumerate() {
        let (chrom, p) = (chrom.as_str(), *p);
        let k = i % 10;
        if k <= 2 {
            calls.push(VcfRow::new(chrom, p, "A", "G", "0/1"));
        } else if k == 3 {
            calls.push(VcfRow::new(chrom, p, "A", "G", "./.").gq(0));
        } else if k <= 7 {
            gvcf.push(
                VcfRow::new(chrom, p - 10, "A", "<NON_REF>", "0/0")
                    .gq(60)
                    .block_end(p + 10),
            );
        } else if k == 8 {
            gvcf.push(
                VcfRow::new(chrom, p - 10, "A", "<NON_REF>", "0/0")
                    .gq(5)
                    .block_end(p + 10),
            );
        }
        if k <= 5 {
            let gt = if k <= 1 {
                "0/1"
            } else if k <= 4 {
                "0/0"
            } else {
                "./."
            };
            chip.push(VcfRow::new(chrom, p, "A", "G", gt));
        }
        if k <= 6 {
            probed.push(VcfRow::new(chrom, p, "A", "G", "0/0"));
        }
    }
    let gvcf_rows: Vec<VcfRow> = gvcf.into_iter().chain(calls.iter().cloned()).collect();
    let wgs_raw = vec![
        ("calls_vcf".to_string(), write_vcf("w.vcf.gz", &calls, "SAMPLE")?),
        ("gvcf".to_string(), write_vcf("w.g.vcf.gz", &gvcf_rows, "SAMPLE")?),
    ];
    let chip_raw = vec![
        ("calls_vcf".to_string(), write_vcf("c.vcf.gz", &chip, "SAMPLE")?),
        ("chip_sites".to_string(), write_vcf("s.vcf.gz", &probed, "SAMPLE")?),
    ];
    for (slug, sample, assay, raw, fp) in [
        ("carmen", "CO-01", "wgs", wgs_raw, "0".repeat(37)),
        ("sam", "SR-CHIP", "chip", chip_raw, "1".repeat(37)),
    ] {
        let (doc, att) = synthetic::with_files(&facts(sample, assay, &fp), &e.blobs, &raw)?;
        e.seed(c, slug, &doc, &att).await?;
    }
    Ok(rows.into_iter().map(|r| r.rsid).collect())
}

#[tokio::test]
async fn thousand_rsids_all_get_a_call_state() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let rsids = thousand(&e, &mut c).await?;
    let expected = [
        (
            "carmen",
            [("alt", 300), ("no_call", 100), ("ref", 400), ("not_covered", 200)].as_slice(),
        ),
        (
            "sam",
            [("alt", 200), ("ref", 300), ("no_call", 200), ("not_on_array", 300)].as_slice(),
        ),
    ];
    for (slug, want) in expected {
        let mut states: BTreeMap<String, usize> = BTreeMap::new();
        for r in &rsids {
            let out = look(&e, &mut c, e.profile(slug), &closed(), rsid(r)).await?;
            let calls = out["calls"].as_array().cloned().unwrap_or_default();
            assert!(!calls.is_empty(), "{r}");
            for call in calls {
                let s = call["call_state"].as_str().unwrap_or_default().to_string();
                assert!(CALL_STATES.contains(&s.as_str()), "{s}");
                *states.entry(s).or_default() += 1;
            }
        }
        assert_eq!(states.values().sum::<usize>(), N);
        let want: BTreeMap<String, usize> = want.iter().map(|(k, v)| ((*k).to_string(), *v)).collect();
        assert_eq!(states, want, "{slug}");
    }
    Ok(())
}

/// The same lookups from raw files htslib wrote (pysam's bgzip and tabix, as every real import
/// brings), so noodles is checked against htslib's own `.tbi`, not only against itself.
#[tokio::test]
async fn htslib_made_files_answer_the_same() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/genome/htslib"));
    let read = |name: &str| std::fs::read(dir.join(name));
    let raw = vec![
        (
            "calls_vcf".to_string(),
            synthetic::RawFile {
                name: "AR-01.vcf.gz".into(),
                data: read("AR-01.vcf.gz")?,
                index: read("AR-01.vcf.gz.tbi")?,
            },
        ),
        (
            "gvcf".to_string(),
            synthetic::RawFile {
                name: "AR-01.g.vcf.gz".into(),
                data: read("AR-01.g.vcf.gz")?,
                index: read("AR-01.g.vcf.gz.tbi")?,
            },
        ),
    ];
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &e.blobs, &raw)?;
    let (_, summary) = e.seed(&mut c, "alex", &facts, &att).await?;
    assert!(summary.reasons.is_empty(), "{:?}", summary.reasons);
    let alex = e.profile("alex");
    let alt = look(&e, &mut c, alex, &closed(), rsid("rs4244285")).await?;
    assert!(alt["calls"][0]["basis"] == "vcf" && alt["calls"][0]["genotype"] == "G/A");
    let block = look(&e, &mut c, alex, &closed(), rsid("rs3918290")).await?;
    assert!(state(&block) == "ref" && block["calls"][0]["basis"] == "gvcf_block");
    let low = look(&e, &mut c, alex, &closed(), hgvs("NC_000003.12:g.2000100A>G")).await?;
    assert_eq!(low["calls"][0]["reason"], "block_below_quality");
    let none = look(&e, &mut c, alex, &closed(), hgvs("NC_000003.12:g.5000000A>G")).await?;
    assert_eq!(none["calls"][0]["reason"], "no_reference_block");
    assert_eq!(
        state(&look(&e, &mut c, alex, &closed(), hgvs("NC_000003.12:g.1000500A>G")).await?),
        "no_call"
    );
    let r = look(&e, &mut c, alex, &open(), region("chr19:44900000-44910000")).await?;
    let pos: Vec<&Value> = r["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| &x["pos"])
        .collect();
    assert_eq!(pos, vec![&json!(44908684)]);
    Ok(())
}
