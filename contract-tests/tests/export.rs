//! The full export (api/export.py, export/__init__.py): a zip of FHIR R4 NDJSON, the original files,
//! the genome files and the raw tables, with a manifest that hashes every entry; step-up and
//! management required. Python: tests/security/test_export_delete.py.

use std::collections::BTreeSet;
use std::io::Read;

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{Client, uniq};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn unzip(bytes: &[u8]) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec()))?;
    let mut out = Vec::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        out.push((f.name()?.to_string(), buf));
    }
    Ok(out)
}

/// Entry names with the sha256 and uuid path parts replaced, so two databases compare.
fn layout(entries: &[(String, Vec<u8>)]) -> anyhow::Result<BTreeSet<String>> {
    let sha = regex::Regex::new(r"^files/[0-9a-f]{64}/")?;
    let uuid = regex::Regex::new(r"^genome/[0-9a-f-]{36}/")?;
    Ok(entries
        .iter()
        .map(|(n, _)| {
            uuid.replace(&sha.replace(n, "files/<sha256>/"), "genome/<dataset>/")
                .to_string()
        })
        .collect())
}

fn ndjson(entries: &[(String, Vec<u8>)], name: &str) -> anyhow::Result<Vec<Value>> {
    let (_, b) = entries
        .iter()
        .find(|(n, _)| n == name)
        .with_context(|| format!("no {name}"))?;
    String::from_utf8_lossy(b)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| Ok(serde_json::from_str(l)?))
        .collect()
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn the_demo_export_of_alex() -> anyhow::Result<()> {
    let c = Client::new()?;
    let r = c
        .post("/api/profiles/alex/export", &json!({}))
        .await?
        .expect(200)?;
    assert_eq!(r.header("content-type").as_deref(), Some("application/zip"));
    assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));
    let cd = r
        .header("content-disposition")
        .context("content-disposition")?;
    let re =
        regex::Regex::new(r#"^attachment; filename="ownhealth-alex-\d{4}-\d{2}-\d{2}\.zip"$"#)?;
    assert!(re.is_match(&cd), "{cd}");
    let entries = unzip(&r.body)?;
    let names = layout(&entries)?;
    for want in [
        "export-manifest.json",
        "fhir/Patient.ndjson",
        "fhir/Observation.ndjson",
        "fhir/Condition.ndjson",
        "fhir/MedicationStatement.ndjson",
        "fhir/Immunization.ndjson",
        "fhir/DocumentReference.ndjson",
        "fhir/Provenance.ndjson",
        "genome/<dataset>/genome-facts.json",
        "genome/<dataset>/genome.vcf.gz",
        "genome/<dataset>/genome.vcf.gz.tbi",
        "ownhealth/audit_event.ndjson",
        "ownhealth/document.ndjson",
        "ownhealth/genome_finding.ndjson",
        "ownhealth/note.ndjson",
        "ownhealth/review_item.ndjson",
        "ownhealth/src.ndjson",
        "ownhealth/wearable_sample.ndjson",
        "ownhealth/wearable_rollup.ndjson",
    ] {
        assert!(names.contains(want), "{want} missing from {names:?}");
    }
    assert!(
        names.contains("files/<sha256>/LabNorte_2026-09-14.pdf"),
        "{names:?}"
    );
    // the manifest hashes every other entry
    let (_, mb) = entries
        .iter()
        .find(|(n, _)| n == "export-manifest.json")
        .context("manifest")?;
    let manifest: Value = serde_json::from_slice(mb)?;
    assert_shape("export_manifest_alex", &manifest)?;
    assert_eq!(manifest["format"], "ownhealth.export/v1");
    assert_eq!(manifest["fhir_version"], "4.0.1");
    assert_eq!(manifest["profile"]["slug"], "alex");
    assert_eq!(manifest["missing_blobs"], json!([]));
    let files = manifest["files"].as_array().context("files")?;
    assert_eq!(files.len(), entries.len() - 1);
    for f in files {
        let path = f["path"].as_str().context("path")?;
        let (_, b) = entries
            .iter()
            .find(|(n, _)| n == path)
            .with_context(|| format!("{path} listed but absent"))?;
        assert_eq!(
            f["sha256"],
            json!(
                Sha256::digest(b)
                    .iter()
                    .map(|x| format!("{x:02x}"))
                    .collect::<String>()
            ),
            "{path}"
        );
        assert_eq!(f["bytes"], json!(b.len()), "{path}");
    }
    let patient = ndjson(&entries, "fhir/Patient.ndjson")?;
    assert_eq!(patient.len(), 1);
    assert_eq!(patient[0]["resourceType"], "Patient");
    let counts = &manifest["counts"];
    assert_eq!(
        counts["Observation"],
        json!(ndjson(&entries, "fhir/Observation.ndjson")?.len())
    );
    // audited, with the counts and the size
    let audit = c.audit(500).await?;
    let e = audit
        .iter()
        .find(|e| e["action"] == "export" && e["profile"] == "alex")
        .context("export audit row")?;
    assert_eq!(
        (e["via"].clone(), e["tool"].clone(), e["profile"].clone()),
        (
            json!("export"),
            json!("POST /api/profiles/export"),
            json!("alex")
        )
    );
    assert_eq!(e["bytes_out"], json!(r.body.len()));
    assert_eq!(
        e["args_summary"],
        json!(format!("fmt=zip;files={}", files.len()))
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn a_small_export_holds_exactly_what_was_written() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    c.post(
        &format!("/api/profiles/{p}/records/condition"),
        &json!({"text": "Asthma", "onset": "2001-05-01"}),
    )
    .await?
    .expect(201)?;
    c.post(
        &format!("/api/profiles/{p}/manual-observations"),
        &json!({"name": "Peso", "value": "70.5", "unit": "kg", "effective_at": "2026-10-01T08:00:00Z", "category": "vital-signs"}),
    )
    .await?
    .expect(201)?;
    let r = c
        .post(&format!("/api/profiles/{p}/export"), &json!({"fmt": "zip"}))
        .await?
        .expect(200)?;
    let entries = unzip(&r.body)?;
    let cond = ndjson(&entries, "fhir/Condition.ndjson")?;
    assert_eq!(cond.len(), 1);
    assert_eq!(cond[0]["code"]["text"], "Asthma", "{}", cond[0]);
    let obs = ndjson(&entries, "fhir/Observation.ndjson")?;
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0]["resourceType"], "Observation");
    let prov = ndjson(&entries, "fhir/Provenance.ndjson")?;
    assert_eq!(prov.len(), 2, "one Provenance per src id");
    let patient = ndjson(&entries, "fhir/Patient.ndjson")?;
    assert_eq!(patient[0]["birthDate"], "1980-04-02");
    assert_shape("export_small_patient", &patient[0])?;
    assert_shape("export_small_condition", &cond[0])?;
    assert_shape("export_small_observation", &obs[0])?;
    let r = c
        .post(&format!("/api/profiles/{p}/export"), &json!({"fmt": "tar"}))
        .await?;
    assert_eq!(r.status, 422);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn export_needs_management_and_a_fresh_login() -> anyhow::Result<()> {
    let c = Client::new()?;
    let r = c.post("/api/profiles/sam/export", &json!({})).await?;
    assert_eq!(
        (r.status, r.code()),
        (403, Some("forbidden")),
        "a viewer cannot export"
    );
    let slug = c.new_relative().await?;
    let m = c.enrolled_member(&format!("Exporter {}", uniq())).await?;
    let code = m.recovery_codes[0].as_str().context("code")?;
    let r = c
        .post("/api/auth/recovery", &json!({"code": code}))
        .await?
        .expect(200)?;
    let stale = c.with_session(&r.cookie_value("oh_session").context("cookie")?);
    let r = stale
        .post(&format!("/api/profiles/{slug}/export"), &json!({}))
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("step_up_required")));
    // the export is a REST action only: no MCP tool exports
    let (_, plain) = c
        .token(json!({"scopes": ownhealth_contract_tests::ALL_SCOPES}))
        .await?;
    let names = ownhealth_contract_tests::Mcp::new(&plain)?
        .tool_names()
        .await?;
    assert!(!names.iter().any(|n| n.contains("export")));
    Ok(())
}
