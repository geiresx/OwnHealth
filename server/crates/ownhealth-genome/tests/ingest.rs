//! Import of genome-facts.json and its raw files: sha256 checks, nothing half-imported, re-runs
//! (`tests/genome/test_genome_ingest.py`).

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use common::{R, count, env};
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::documents::{StoredFile, dispatch_upload};
use ownhealth_core::jobs::{JobContext, run_one};
use ownhealth_core::provenance::resolve_src;
use ownhealth_core::{Error, ids};
use ownhealth_genome::synthetic::{self, VcfRow};
use ownhealth_genome::{Gate, VariantQuery, import_genome_facts, lookup_variant};
use serde_json::{Value, json};
use sqlx::Connection;

fn import_err<T: std::fmt::Debug>(r: ownhealth_core::Result<T>) -> String {
    match r {
        Err(Error::Invalid(m)) => m,
        other => panic!("expected an import error, got {other:?}"),
    }
}

#[tokio::test]
async fn import_stores_facts_coverage_and_raw_file_pointers() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let (_doc, facts, summary) = e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    assert_eq!(summary.accepted as usize, facts["facts"].as_array().map_or(0, Vec::len));
    assert_eq!(summary.reasons, BTreeMap::new());
    let (ds_id, vcf, tbi, gvcf, dbv, ver, src): (uuid::Uuid, String, String, String, Value, String, String) =
        sqlx::query_as(
            "select id, encode(vcf_blob, 'hex'), encode(tbi_blob, 'hex'), encode(gvcf_blob, 'hex'), db_versions, \
             owngenome_version, src_id from genome_dataset where profile_id = $1",
        )
        .bind(alex)
        .fetch_one(&mut *c)
        .await?;
    let files: Vec<&Value> = facts["files"].as_array().into_iter().flatten().collect();
    let by_role = |r: &str| {
        files
            .iter()
            .find(|f| f["role"] == r)
            .copied()
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(vcf, by_role("calls_vcf")["sha256"].as_str().unwrap_or_default());
    assert_eq!(tbi, by_role("calls_vcf")["index_sha256"].as_str().unwrap_or_default());
    assert_eq!(gvcf, by_role("gvcf")["sha256"].as_str().unwrap_or_default());
    assert_eq!(
        (dbv["cpic"].as_str(), ver.as_str()),
        (Some("2026-09"), "OwnGenome 1.4.2")
    );
    assert!(src.starts_with("src_gd_"));
    assert!(e.blobs.exists(&vcf) && e.blobs.exists(&tbi));
    let n_cov: i64 = sqlx::query_scalar("select count(*) from genome_coverage where dataset_id = $1")
        .bind(ds_id)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(n_cov as usize, facts["coverage"].as_array().map_or(0, Vec::len));
    type Row = (
        String,
        Option<String>,
        String,
        String,
        Option<String>,
        bool,
        bool,
        bool,
        String,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select fact_id, gene, evidence, confidence, call_state, sensitive, adult_onset, carrier, src_id \
         from genome_finding where profile_id = $1",
    )
    .bind(alex)
    .fetch_all(&mut *c)
    .await?;
    assert_eq!(rows.len(), facts["facts"].as_array().map_or(0, Vec::len));
    let gene = |g: &str| {
        rows.iter()
            .find(|r| r.1.as_deref() == Some(g))
            .cloned()
            .unwrap_or_else(|| panic!("{g}"))
    };
    assert_eq!(
        (gene("CYP2C19").4.as_deref(), gene("CYP2C19").2.as_str()),
        (Some("alt"), "E1")
    );
    assert_eq!(gene("DPYD").4.as_deref(), Some("ref"));
    assert!(gene("APOE").5 && gene("APOE").6);
    assert!(gene("CFTR").7 && !gene("CFTR").5);
    assert!(rows.iter().all(|r| r.8.starts_with("src_g_")));
    // raw genome files never enter the database: no column holds VCF bytes
    let big: i32 = sqlx::query_scalar("select max(pg_column_size(fact)) from genome_finding where profile_id = $1")
        .bind(alex)
        .fetch_one(&mut *c)
        .await?;
    assert!(big < 4000, "{big}");
    Ok(())
}

#[tokio::test]
async fn src_resolves_to_the_facts_file() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let sid: String =
        sqlx::query_scalar("select src_id from genome_finding where gene = 'SLCO1B1' and profile_id = $1")
            .bind(e.profile("alex"))
            .fetch_one(&mut *c)
            .await?;
    let resolved = resolve_src(&mut c, &sid).await?.unwrap_or_else(|| panic!("unresolved"));
    assert_eq!(resolved.src["kind"], "genome_facts");
    assert_eq!(resolved.facts[0]["table"], "genome_finding");
    assert_eq!(resolved.profile_id, Some(e.profile("alex")));
    assert!(resolved.src["import_ref"].as_str().unwrap_or_default().contains("#f_"));
    Ok(())
}

#[tokio::test]
async fn reimporting_the_same_bytes_is_a_no_op() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let (doc, facts, _) = e.alex_genome(&mut c).await?;
    let again = import_genome_facts(&mut c, &e.blobs, doc, &[], None).await?;
    assert_eq!(again.accepted, 0);
    assert_eq!(again.skipped as usize, facts["facts"].as_array().map_or(0, Vec::len));
    assert_eq!(count(&mut c, "genome_dataset", e.profile("alex")).await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_vcf_whose_sha256_differs_is_refused_and_nothing_is_written() -> R {
    let e = env().await?;
    let mut tx = e.db.pool.begin().await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, mut att) = synthetic::with_files(&synthetic::alex_facts(), &e.blobs, &raw)?;
    let stranger = e.blobs.put_bytes(b"\x1f\x8b not the file the facts describe")?;
    for (name, sha) in att.iter_mut() {
        if name == "AR-01.vcf.gz" {
            *sha = stranger.clone();
        }
    }
    let err = import_err(e.seed(&mut tx, "alex", &facts, &att).await);
    assert!(err.contains("matches no file listed"), "{err}");
    tx.rollback().await?;
    let mut c = e.conn().await?;
    assert_eq!(count(&mut c, "genome_finding", e.profile("alex")).await?, 0);
    Ok(())
}

#[tokio::test]
async fn corrupted_blob_bytes_are_refused() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &e.blobs, &raw)?;
    let sha = facts["files"][0]["sha256"].as_str().unwrap_or_default().to_string();
    let path = e.blobs.path(&sha)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
    std::fs::write(&path, b"tampered")?;
    let err = import_err(e.seed(&mut c, "alex", &facts, &att).await);
    assert!(err.contains("do not match sha256"), "{err}");
    Ok(())
}

#[tokio::test]
async fn missing_raw_files_import_the_facts_and_say_so() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let mut facts = synthetic::alex_facts();
    facts["files"] = json!([{
        "role": "calls_vcf",
        "path": "vcf/AR-01.vcf.gz",
        "sha256": "c".repeat(64),
        "index_path": "vcf/AR-01.vcf.gz.tbi",
        "index_sha256": "d".repeat(64),
    }]);
    let (_, summary) = e.seed(&mut c, "alex", &facts, &[]).await?;
    assert_eq!(summary.reasons, BTreeMap::from([("raw_file_missing".to_string(), 1)]));
    let vcf: Option<Vec<u8>> = sqlx::query_scalar("select vcf_blob from genome_dataset where profile_id = $1")
        .bind(e.profile("alex"))
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(vcf, None);
    Ok(())
}

#[tokio::test]
async fn invalid_file_raises_with_the_reasons() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let mut facts = synthetic::alex_facts();
    facts["schema"] = json!("ownhealth.genome-facts/v2");
    let err = import_err(e.seed(&mut c, "alex", &facts, &[]).await);
    assert!(
        err.contains("unsupported genome-facts version 'ownhealth.genome-facts/v2'"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn the_same_file_on_another_profile_is_refused() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let (doc, _, _) = e.alex_genome(&mut c).await?;
    sqlx::query("update document set profile_id = $1 where id = $2")
        .bind(e.profile("sam"))
        .bind(doc)
        .execute(&mut *c)
        .await?;
    let err = import_err(import_genome_facts(&mut c, &e.blobs, doc, &[], None).await);
    assert!(err.contains("already imported for another profile"), "{err}");
    Ok(())
}

#[tokio::test]
async fn rerun_supersedes_the_previous_file() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let alex = e.profile("alex");
    let mut newer = synthetic::alex_facts();
    newer["provenance"]["generated_utc"] = json!("2026-11-01T09:00:00Z");
    let kept: Vec<Value> = newer["facts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["gene"] != "LCT")
        .cloned()
        .collect();
    newer["facts"] = json!(kept);
    let (_, summary) = e.seed(&mut c, "alex", &newer, &[]).await?;
    assert_eq!(summary.accepted as usize, kept.len());
    let states: Vec<(String, i64)> =
        sqlx::query_as("select status, count(*) from genome_dataset where profile_id = $1 group by status order by 1")
            .bind(alex)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(states, vec![("current".to_string(), 1), ("superseded".to_string(), 1)]);
    let current: Vec<(Option<String>, Option<uuid::Uuid>)> =
        sqlx::query_as("select gene, supersedes from genome_finding where profile_id = $1 and status = 'current'")
            .bind(alex)
            .fetch_all(&mut *c)
            .await?;
    assert!(!current.iter().any(|(g, _)| g.as_deref() == Some("LCT")));
    assert!(current.iter().all(|(_, sup)| sup.is_some())); // each re-run fact points at its previous version
    Ok(())
}

#[tokio::test]
async fn fingerprint_mismatch_blocks_a_second_source() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let mut other = synthetic::sam_facts();
    other["subject"]["sample_id"] = json!("AR-CHIP-2019");
    let mut tx = c.begin().await?;
    let err = import_err(e.seed(&mut tx, "alex", &other, &[]).await);
    assert!(err.contains("fingerprint mismatch"), "{err}");
    tx.rollback().await?;
    let mut same = synthetic::alex_facts();
    same["subject"]["sample_id"] = json!("AR-CHIP-2019");
    same["assay"] = json!({"type": "wgs", "build": "GRCh38"});
    e.seed(&mut c, "alex", &same, &[]).await?;
    assert_eq!(count(&mut c, "genome_dataset", e.profile("alex")).await?, 2);
    Ok(())
}

#[tokio::test]
async fn a_sample_already_on_another_profile_is_refused() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    e.alex_genome(&mut c).await?;
    let err = import_err(e.seed(&mut c, "sam", &synthetic::alex_facts(), &[]).await);
    assert!(err.contains("already imported for profile 'alex'"), "{err}");
    Ok(())
}

/// dispatch_upload -> import.genome_facts job -> findings; a bad upload becomes a job_failed item.
#[tokio::test]
async fn upload_job_end_to_end() -> R {
    let e = env().await?;
    ownhealth_genome::register();
    let raw = synthetic::alex_raw_files()?;
    let (facts, _) = synthetic::with_files(&synthetic::alex_facts(), &e.blobs, &raw)?;
    let data = serde_json::to_vec(&facts)?;
    let mut files = vec![StoredFile {
        filename: "AR-01_genome_facts.json".into(),
        sha256: e.blobs.put_bytes(&data)?,
        size: data.len() as i64,
    }];
    for (_, f) in &raw {
        for (name, bytes) in [(f.name.clone(), &f.data), (f.index_name(), &f.index)] {
            files.push(StoredFile {
                filename: name,
                sha256: e.blobs.put_bytes(bytes)?,
                size: bytes.len() as i64,
            });
        }
    }
    let mut lucia_bad = synthetic::lucia_facts();
    lucia_bad["schema"] = json!("ownhealth.genome-facts/v9");
    let bad = serde_json::to_vec(&lucia_bad)?;
    let bad_file = StoredFile {
        filename: "bad.json".into(),
        sha256: e.blobs.put_bytes(&bad)?,
        size: bad.len() as i64,
    };
    let (alex, lucia) = (e.profile("alex"), e.profile("lucia"));
    let mut tx = e.db.pool.begin().await?;
    dispatch_upload(&mut tx, &e.blobs, e.fam.household_id, alex, &files, None).await?;
    dispatch_upload(&mut tx, &e.blobs, e.fam.household_id, lucia, &[bad_file], None).await?;
    tx.commit().await?;
    let settings = e.db.settings(e.dir.path())?;
    let ctx = JobContext::new(e.db.pool.clone(), Arc::new(settings), e.blobs.clone());
    let kinds = vec!["import.genome_facts".to_string()];
    for _ in 0..2 {
        assert!(run_one(&ctx, "core", Some(&kinds)).await?.is_some());
    }
    let mut c = e.conn().await?;
    assert_eq!(
        count(&mut c, "genome_finding", alex).await? as usize,
        facts["facts"].as_array().map_or(0, Vec::len)
    );
    let vcf: bool = sqlx::query_scalar("select vcf_blob is not null from genome_dataset where profile_id = $1")
        .bind(alex)
        .fetch_one(&mut *c)
        .await?;
    assert!(vcf);
    assert_eq!(count(&mut c, "genome_dataset", lucia).await?, 0);
    assert_eq!(count(&mut c, "document", lucia).await?, 0);
    let item: Value =
        sqlx::query_scalar("select payload from review_item where kind = 'job_failed' and profile_id = $1")
            .bind(lucia)
            .fetch_one(&mut *c)
            .await?;
    assert!(
        item["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ownhealth.genome-facts/v9"),
        "{item}"
    );
    Ok(())
}

/// Someone holds only the shareable facts JSON (it names the raw files by sha256).
fn copy_of_facts_without_files(facts: &Value) -> Value {
    let mut copied = facts.clone();
    copied["provenance"]["generated_utc"] = json!("2026-11-02T09:00:00Z"); // other bytes than the original
    copied
}

/// A sha256 on its own grants nothing: raw files already in the blob store are linked only when they
/// were uploaded with this import or already belong to this profile.
#[tokio::test]
async fn a_facts_file_cannot_attach_raw_files_another_profile_uploaded() -> R {
    for other_household in [true, false] {
        let e = env().await?;
        let mut c = e.conn().await?;
        let (_, facts, _) = e.alex_genome(&mut c).await?;
        let mut copied = copy_of_facts_without_files(&facts);
        let (household, profile) = if other_household {
            let other = create_rivera_family(&mut c, &format!("Rivera {}", ids::token_hex(3))).await?;
            (other.household_id, other.profile("alex"))
        } else {
            // Sam's profile in Alex's household, under another sample id so the sample guard does not fire
            copied["subject"]["sample_id"] = json!("SR-COPY-1");
            copied["subject"].as_object_mut().map(|s| s.shift_remove("fingerprint"));
            (e.fam.household_id, e.profile("sam"))
        };
        let (_, summary) = synthetic::seed_genome(
            &mut c,
            &e.blobs,
            household,
            profile,
            &copied,
            &[],
            "genome-facts.json",
            None,
        )
        .await?;
        assert_eq!(summary.reasons.get("raw_file_missing"), Some(&2));
        let blobs_row: (Option<Vec<u8>>, Option<Vec<u8>>) = sqlx::query_as(
            "select vcf_blob, gvcf_blob from genome_dataset where profile_id = $1 and status = 'current'",
        )
        .bind(profile)
        .fetch_one(&mut *c)
        .await?;
        assert_eq!(blobs_row, (None, None));
        let q = VariantQuery {
            rsid: Some("rs4244285"),
            ..Default::default()
        };
        let answer = lookup_variant(&mut c, &e.blobs, profile, &Gate::new(true, false), &q)
            .await?
            .unwrap_or_else(|e| panic!("{e:?}"));
        let calls = answer["calls"].as_array().cloned().unwrap_or_default();
        assert!(!calls.is_empty()); // a positive control: the check below must not pass on an empty list
        assert!(calls.iter().all(|c| c["basis"] != "vcf"), "{calls:?}");
    }
    Ok(())
}

#[tokio::test]
async fn a_reimport_on_the_same_profile_keeps_its_raw_files() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let (_, facts, _) = e.alex_genome(&mut c).await?;
    let (_, summary) = e
        .seed(&mut c, "alex", &copy_of_facts_without_files(&facts), &[])
        .await?;
    assert!(!summary.reasons.contains_key("raw_file_missing"));
    let vcf: Option<Vec<u8>> =
        sqlx::query_scalar("select vcf_blob from genome_dataset where profile_id = $1 and status = 'current'")
            .bind(e.profile("alex"))
            .fetch_one(&mut *c)
            .await?;
    assert!(vcf.is_some());
    Ok(())
}

/// Lucía's facts as a chip_imputed file whose CFTR carrier site the array typed (imputed: false).
fn imputed_chip_with_typed_carrier() -> Value {
    let mut doc = synthetic::lucia_facts();
    doc["subject"]["sample_id"] = json!("LR-CHIP");
    doc["assay"] = json!({"type": "chip_imputed", "build": "GRCh38", "platform": "synthetic array"});
    let facts: Vec<Value> = doc["facts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["category"] == "pgx" || f["category"] == "carrier")
        .cloned()
        .map(|mut f| {
            let level = if f["category"] == "carrier" {
                "unconfirmed"
            } else {
                "medium"
            };
            f["confidence"]["level"] = json!(level);
            for v in f
                .get_mut("variants")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                v["genotype"]["imputed"] = json!(false);
            }
            f
        })
        .collect();
    doc["facts"] = json!(facts);
    doc
}

/// Rule 7: a site marked imputed: false must be in the chip_sites file; without that file a carrier
/// fact from chip_imputed is rejected.
#[tokio::test]
async fn chip_imputed_typed_sites_are_checked_against_the_site_list() -> R {
    for probed in [Some(true), Some(false), None] {
        let e = env().await?;
        let mut c = e.conn().await?;
        let doc = imputed_chip_with_typed_carrier();
        let mut sites: Vec<VcfRow> = Vec::new();
        for f in doc["facts"].as_array().into_iter().flatten() {
            for v in f["variants"].as_array().into_iter().flatten() {
                sites.push(VcfRow::new(
                    v["chrom"].as_str().unwrap_or_default(),
                    v["pos"].as_i64().unwrap_or_default(),
                    v["ref"].as_str().unwrap_or_default(),
                    v["alt"].as_str().unwrap_or_default(),
                    "0/0",
                ));
            }
        }
        if probed == Some(false) {
            sites.retain(|s| s.pos != 117559590);
        }
        let mut raw = vec![(
            "calls_vcf".to_string(),
            synthetic::write_vcf("c.vcf.gz", &sites, "SAMPLE")?,
        )];
        if probed.is_some() {
            raw.push(("chip_sites".into(), synthetic::write_vcf("s.vcf.gz", &sites, "SAMPLE")?));
        }
        let (facts, att) = synthetic::with_files(&doc, &e.blobs, &raw)?;
        let mut tx = c.begin().await?;
        let result = e.seed(&mut tx, "lucia", &facts, &att).await;
        if probed == Some(true) {
            let (_, summary) = result?;
            assert_eq!(summary.accepted as usize, doc["facts"].as_array().map_or(0, Vec::len));
            continue;
        }
        let err = import_err(result);
        let want = if probed == Some(false) {
            "not in the chip_sites list"
        } else {
            "needs the chip_sites list on hand"
        };
        assert!(err.contains(want), "{probed:?}: {err}");
        tx.rollback().await?;
        assert_eq!(count(&mut c, "genome_finding", e.profile("lucia")).await?, 0);
    }
    Ok(())
}
