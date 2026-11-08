//! The profile export (`tests/security/test_export_delete.py`, the parts that test `export_profile`;
//! the step-up route and its audit row are lane G's).

use std::collections::BTreeSet;
use std::io::{Cursor, Read};

use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::demo_pdf::{BBox, LabReport, LabRow, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation, retract_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::review::{self, CandidateIn};
use ownhealth_core::testing::TestDb;
use ownhealth_ops::export::export_profile;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

type R = anyhow::Result<()>;

fn obs(profile_id: Uuid, value: &str, day: &str) -> anyhow::Result<ObservationIn> {
    Ok(ObservationIn::from_json(json!({
        "profile_id": profile_id, "printed_name": "Colesterol LDL", "printed_value": value,
        "printed_unit": "mg/dL", "printed_ref_range": "< 130", "value_num": value, "ref_high": "130",
        "loinc": "13457-7", "alias_group": "ldl_c", "unit_ucum": "mg/dL", "value_canonical": value,
        "unit_canonical": "mg/dL", "derivation_version": "test", "effective_at": format!("{day}T08:00:00+00:00"),
    }))?)
}

/// A one-page LabNorte PDF; a random value makes the bytes, and so the blob, unique to the test.
async fn lab_document(
    c: &mut PgConnection,
    blobs: &BlobStore,
    f: &Family,
) -> anyhow::Result<(Uuid, String, Vec<BBox>)> {
    let (pdf, boxes) = lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id: "LN-T",
        pages: vec![vec![
            LabRow::new("Colesterol LDL", "97", "mg/dL", "< 130"),
            LabRow::new("Vitamina D", &ownhealth_core::ids::token_hex(3), "ng/mL", "30 - 100").flagged("L"),
        ]],
    });
    let sha = blobs.put_bytes(&pdf)?;
    let doc = create_document(
        c,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            ..NewDocument::upload(
                f.household_id,
                Some(f.profile("alex")),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?;
    Ok((doc.id, sha, boxes.into_iter().next().unwrap_or_default()))
}

fn read(z: &mut zip::ZipArchive<Cursor<Vec<u8>>>, name: &str) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    z.by_name(name)?.read_to_end(&mut out)?;
    Ok(out)
}

fn ndjson(bytes: &[u8]) -> anyhow::Result<Vec<Value>> {
    Ok(bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(serde_json::from_slice)
        .collect::<Result<_, _>>()?)
}

#[tokio::test]
async fn export_zip() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = create_rivera_family(&mut c, "Rivera (export)").await?;
    let pid = f.profile("alex");
    // a LabNorte report with one auto-accepted row, one row in review, a manual value later retracted
    let (doc_id, sha, boxes) = lab_document(&mut c, &blobs, &f).await?;
    let run = review::create_extraction_run(&mut c, doc_id, "rules:test", "1", b"t", None, None).await?;
    let cand = |b: BBox, conf: f64, v: &str| -> anyhow::Result<CandidateIn> {
        Ok(CandidateIn {
            page_no: 1,
            bbox: b,
            cells: json!({"value": {"text": v}}),
            row_conf: conf,
            observation: obs(pid, v, "2026-09-14")?,
            checks: vec![],
        })
    };
    let ok = review::submit_candidate(
        &mut c,
        run,
        &cand(boxes[0], 0.99, "97")?,
        "template_text",
        "matched",
        0.95,
        false,
    )
    .await?;
    let queued = review::submit_candidate(
        &mut c,
        run,
        &cand(boxes[1], 0.5, "24")?,
        "rules_text",
        "matched",
        0.95,
        false,
    )
    .await?;
    assert!(ok.state == "auto_accepted" && queued.state == "in_review");
    let manual = SrcRef::New(SrcIn::manual(Some(f.user("alex"))));
    let m = commit_observation(
        &mut c,
        &obs(pid, "101", "2026-10-01")?,
        &manual,
        CommitOptions::new("person"),
    )
    .await?;
    retract_observation(&mut c, m.id, "typed the wrong value").await?;
    write_audit(
        &mut c,
        &AuditEvent {
            actor_id: Some(f.user("sam")),
            profile_id: Some(pid),
            ..AuditEvent::new("user", "ui", "read")
        },
    )
    .await?;
    // Sam's value must not leak into Alex's export
    let sam = SrcRef::New(SrcIn::manual(Some(f.user("sam"))));
    commit_observation(
        &mut c,
        &obs(f.profile("sam"), "55", "2026-09-14")?,
        &sam,
        CommitOptions::new("person"),
    )
    .await?;

    let mut out = Cursor::new(Vec::new());
    let manifest = export_profile(&mut c, &blobs, pid, &mut out, "zip").await?;
    let mut z = zip::ZipArchive::new(Cursor::new(out.into_inner()))?;
    let m: Value = serde_json::from_slice(&read(&mut z, "export-manifest.json")?)?;
    assert_eq!(m, manifest);
    assert!(m["format"] == "ownhealth.export/v1" && m["profile"]["slug"] == "alex");
    for f in m["files"].as_array().into_iter().flatten() {
        // every entry is listed with the right hash
        let path = f["path"].as_str().unwrap_or_default();
        assert_eq!(
            hex::encode(Sha256::digest(read(&mut z, path)?)),
            f["sha256"].as_str().unwrap_or_default(),
            "{path}"
        );
    }
    let names: BTreeSet<String> = z
        .file_names()
        .map(|n| n.map(|n| n.into_owned()))
        .collect::<Result<_, _>>()?;
    for want in [
        "fhir/Patient.ndjson",
        "fhir/Observation.ndjson",
        "fhir/DocumentReference.ndjson",
        "fhir/Provenance.ndjson",
        "ownhealth/review_item.ndjson",
        "ownhealth/audit_event.ndjson",
    ] {
        assert!(names.contains(want), "{want} missing from {names:?}");
    }
    // the original upload, byte for byte
    let original = names
        .iter()
        .find(|n| n.starts_with(&format!("files/{sha}/")))
        .cloned()
        .unwrap_or_default();
    assert_eq!(hex::encode(Sha256::digest(read(&mut z, &original)?)), sha);
    let observations = ndjson(&read(&mut z, "fhir/Observation.ndjson")?)?;
    let mut statuses: Vec<String> = observations
        .iter()
        .flat_map(|o| o["meta"]["tag"].as_array().cloned().unwrap_or_default())
        .filter_map(|t| t["code"].as_str().map(str::to_string))
        .collect();
    statuses.sort();
    assert_eq!(statuses, ["current", "retracted"]); // every lifecycle state, marked; nothing of Sam's
    let alex_ids: BTreeSet<String> = sqlx::query_scalar::<_, Uuid>("select id from observation where profile_id = $1")
        .bind(pid)
        .fetch_all(&mut *c)
        .await?
        .into_iter()
        .map(|u| u.to_string())
        .collect();
    let got: BTreeSet<String> = observations
        .iter()
        .filter_map(|o| o["id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(got, alex_ids);
    assert!(observations.iter().all(|o| {
        o["subject"]["reference"]
            .as_str()
            .is_some_and(|r| r.starts_with("Patient/"))
    }));
    let prov = ndjson(&read(&mut z, "fhir/Provenance.ndjson")?)?;
    let src_id = ok.src_id.unwrap_or_default();
    let cited = prov
        .iter()
        .find(|p| p["extension"][0]["valueString"] == src_id.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        cited["entity"][0]["what"]["reference"],
        format!("DocumentReference/{doc_id}")
    );
    let loc = cited["extension"]
        .as_array()
        .and_then(|e| e.iter().find(|e| e["url"] == "urn:ownhealth:location"))
        .and_then(|e| e["valueString"].as_str())
        .unwrap_or_default();
    let loc: Value = serde_json::from_str(loc)?;
    assert!(loc["page"] == 1 && loc["bbox"].as_array().map(Vec::len) == Some(4));
    // only Alex's data, and nothing about access control or tokens
    assert!(
        names
            .iter()
            .all(|n| !n.contains("api_token") && !n.contains("profile_grant"))
    );
    let obs_text = String::from_utf8(read(&mut z, "fhir/Observation.ndjson")?)?;
    assert!(!obs_text.contains(&f.profile("sam").to_string()));
    // the manifest is Python's ensure_ascii JSON
    let raw = String::from_utf8(read(&mut z, "export-manifest.json")?)?;
    assert!(raw.is_ascii());
    Ok(())
}

#[tokio::test]
async fn a_missing_blob_is_listed_in_the_manifest() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = create_rivera_family(&mut c, "Rivera (export)").await?;
    let (_, sha, _) = lab_document(&mut c, &blobs, &f).await?;
    std::fs::remove_file(blobs.path(&sha)?)?; // the row survives, the file is gone
    let mut out = Cursor::new(Vec::new());
    let manifest = export_profile(&mut c, &blobs, f.profile("alex"), &mut out, "zip").await?;
    let mut z = zip::ZipArchive::new(Cursor::new(out.into_inner()))?;
    let names: Vec<String> = z
        .file_names()
        .map(|n| n.map(|n| n.into_owned()))
        .collect::<Result<_, _>>()?;
    assert!(names.iter().all(|n| !n.starts_with(&format!("files/{sha}/"))));
    let missing: Vec<&str> = manifest["missing_blobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["source_blob"].as_str())
        .collect();
    assert_eq!(missing, [sha.as_str()]);
    let m: Value = serde_json::from_slice(&read(&mut z, "export-manifest.json")?)?;
    assert!(
        m["missing_blobs"][0]["path"]
            .as_str()
            .is_some_and(|p| p.starts_with(&format!("files/{sha}/")))
    );
    Ok(())
}

#[tokio::test]
async fn an_unknown_format_is_refused() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = create_rivera_family(&mut c, "Rivera (export)").await?;
    let err = export_profile(&mut c, &blobs, f.profile("alex"), Cursor::new(Vec::new()), "tar").await;
    assert!(matches!(err, Err(ownhealth_core::Error::Invalid(_))));
    Ok(())
}

/// The export copies blobs and deflates on the runtime: with one worker, a big blob must not stall
/// the other tasks on it (`serve` shares the runtime with the job workers and the MCP server).
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_large_export_does_not_stall_the_runtime() -> R {
    use std::io::Read as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = create_rivera_family(&mut c, "Rivera (export)").await?;
    let size = 128u64 << 20;
    let bytes = ownhealth_core::ids::token_hex(8).into_bytes();
    let (sha, n) = blobs.put_reader(bytes.as_slice().chain(std::io::repeat(7).take(size)))?;
    create_document(
        &mut c,
        &NewDocument {
            filename: Some("apple-health-export.zip"),
            ..NewDocument::upload(
                f.household_id,
                Some(f.profile("alex")),
                &sha,
                "application/zip",
                n as i64,
            )
        },
    )
    .await?;
    let pid = f.profile("alex");
    let done = Arc::new(AtomicBool::new(false));
    let ticker = tokio::spawn({
        let done = done.clone();
        async move {
            let mut worst = Duration::ZERO;
            while !done.load(Ordering::Relaxed) {
                let t = Instant::now();
                tokio::time::sleep(Duration::from_millis(2)).await;
                worst = worst.max(t.elapsed());
            }
            worst
        }
    });
    let export = tokio::spawn(async move {
        let out = tempfile::tempfile()?;
        let started = Instant::now();
        export_profile(&mut c, &blobs, pid, out, "zip").await?;
        anyhow::Ok(started.elapsed())
    });
    let took = export.await??;
    done.store(true, Ordering::Relaxed);
    let worst = ticker.await?;
    // blocked, the sleep lasts about as long as the whole copy; the bound is relative so a busy
    // box does not make it flaky
    assert!(
        worst * 4 < took,
        "a 2 ms sleep took {worst:?} while a {took:?} export ran"
    );
    Ok(())
}
