//! A crafted .tbi must be refused, never abort the process. noodles sizes its bin map from the
//! untrusted n_bin, so an index claiming 2^31 bins asked for 38 GB and the allocation failure
//! aborted the server (no panic, so nothing could catch it). macOS overcommit hides the abort: run
//! this file on Linux with a memory limit to see it (`docker run --memory 2g`).

mod common;

use std::io::Write;

use common::{R, count, env};
use noodles::bgzf;
use ownhealth_core::Error;
use ownhealth_genome::synthetic;
use ownhealth_genome::vcf::{VcfFile, read_tabix};

fn bgzip(payload: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut w = bgzf::io::Writer::new(Vec::new());
    w.write_all(payload)?;
    w.finish()
}

/// n_ref 1, a VCF header, contig chr1, then n_bin `n_bin` and nothing after it.
fn crafted_tbi(n_bin: i32) -> std::io::Result<Vec<u8>> {
    let names = b"chr1\0";
    let mut p = b"TBI\x01".to_vec();
    for v in [1, 2, 1, 2, 0, i32::from(b'#'), 0, names.len() as i32] {
        p.extend(v.to_le_bytes());
    }
    p.extend(names);
    p.extend(n_bin.to_le_bytes());
    bgzip(&p)
}

fn invalid_data(r: std::io::Result<impl std::fmt::Debug>) -> String {
    match r {
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => e.to_string(),
        other => panic!("expected InvalidData, got {other:?}"),
    }
}

#[test]
fn a_huge_n_bin_is_refused_without_allocating() -> R {
    let dir = tempfile::tempdir()?;
    let tbi = dir.path().join("evil.tbi");
    std::fs::write(&tbi, crafted_tbi(0x7fff_ffff)?)?;
    let msg = invalid_data(read_tabix(&tbi));
    assert!(msg.contains("n_bin"), "{msg}");
    Ok(())
}

#[test]
fn counts_that_overrun_the_file_or_are_negative_are_refused() -> R {
    let dir = tempfile::tempdir()?;
    let tbi = dir.path().join("x.tbi");
    for n in [-1, 2] {
        std::fs::write(&tbi, crafted_tbi(n)?)?;
        invalid_data(read_tabix(&tbi));
    }
    let mut p = b"TBI\x01".to_vec();
    p.extend(i32::MAX.to_le_bytes()); // n_ref
    std::fs::write(&tbi, bgzip(&p)?)?;
    assert!(invalid_data(read_tabix(&tbi)).contains("n_ref"));
    std::fs::write(&tbi, bgzip(b"not an index")?)?;
    invalid_data(read_tabix(&tbi));
    Ok(())
}

#[test]
fn a_real_index_still_reads() -> R {
    let header = [
        "##fileformat=VCFv4.2".to_string(),
        "#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO".to_string(),
    ];
    let lines = ["chr1\t100\trs1\tA\tG\t.\tPASS\t.".to_string()];
    let (data, index) = ownhealth_genome::vcf::write_indexed(&header, &lines)?;
    let dir = tempfile::tempdir()?;
    let (d, i) = (dir.path().join("a.vcf.gz"), dir.path().join("a.vcf.gz.tbi"));
    std::fs::write(&d, data)?;
    std::fs::write(&i, index)?;
    let mut vf = VcfFile::open(&d, &i)?;
    vf.check_header()?;
    let recs = vf.fetch(&["chr1".to_string()], 100, 100)?;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].pos, 100);
    Ok(())
}

#[tokio::test]
async fn an_import_with_a_crafted_index_fails_as_invalid_and_writes_nothing() -> R {
    let e = env().await?;
    let mut raw = synthetic::alex_raw_files()?;
    for (role, file) in raw.iter_mut() {
        if role == "calls_vcf" {
            file.index = crafted_tbi(0x7fff_ffff)?;
        }
    }
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &e.blobs, &raw)?;
    let mut tx = e.db.pool.begin().await?;
    let err = match e.seed(&mut tx, "alex", &facts, &att).await {
        Err(Error::Invalid(m)) => m,
        other => panic!("expected an import error, got {other:?}"),
    };
    assert!(err.contains("not a bgzipped VCF with a tabix index"), "{err}");
    tx.rollback().await?;
    let mut c = e.conn().await?;
    assert_eq!(count(&mut c, "genome_finding", e.profile("alex")).await?, 0);
    Ok(())
}
