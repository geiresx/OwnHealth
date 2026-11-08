//! The upload dispatcher and the sensitive-document rule (`core/documents.py`).

mod common;

use common::*;
use ownhealth_core::Error;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::{StoredFile, dispatch_upload, document_is_sensitive};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::testing::TestDb;
use serde_json::json;

fn stored(blobs: &BlobStore, name: &str, bytes: &[u8]) -> anyhow::Result<StoredFile> {
    Ok(StoredFile {
        filename: name.into(),
        sha256: blobs.put_bytes(bytes)?,
        size: bytes.len() as i64,
    })
}

#[tokio::test]
async fn uploads_become_documents_and_jobs_once() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (hh, p) = (f.household_id, f.profile("alex"));
    let pdf = stored(&blobs, "labnorte.pdf", b"%PDF-1.4 synthetic")?;
    let out = dispatch_upload(&mut c, &blobs, hh, p, std::slice::from_ref(&pdf), None).await?;
    let item = &out["items"][0];
    let keys: Vec<&String> = item.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        ["filename", "kind", "sha256", "document_id", "duplicate", "job_id"]
    );
    assert!(item["kind"] == "pdf" && item["duplicate"] == false && item["job_id"].is_i64());
    let again = dispatch_upload(&mut c, &blobs, hh, p, &[pdf], None).await?;
    assert_eq!(again["items"][0]["duplicate"], true);
    assert_eq!(again["items"][0]["document_id"], item["document_id"]);
    assert!(again["items"][0].get("job_id").is_none());
    let sources: i64 = sqlx::query_scalar("select count(*) from document_source")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(sources, 2);

    let vcf = stored(&blobs, "x.vcf.gz", b"\x1f\x8bsynthetic")?;
    let e = dispatch_upload(&mut c, &blobs, hh, p, std::slice::from_ref(&vcf), None).await;
    assert!(
        matches!(&e, Err(Error::Upload(m)) if m.contains("genome-facts.json")),
        "{e:?}"
    );
    let facts = stored(
        &blobs,
        "genome-facts.json",
        br#"{"schema": "ownhealth.genome-facts/1"}"#,
    )?;
    let out = dispatch_upload(&mut c, &blobs, hh, p, &[facts, vcf.clone()], None).await?;
    assert_eq!(out["items"][1]["attached_to"], "genome_facts");
    let payload: serde_json::Value = sqlx::query_scalar("select payload from job where kind = 'import.genome_facts'")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(payload["files"], json!({"x.vcf.gz": vcf.sha256}));
    let txt = stored(&blobs, "notes.txt", b"hello")?;
    assert!(matches!(
        dispatch_upload(&mut c, &blobs, hh, p, &[txt], None).await,
        Err(Error::Upload(_))
    ));
    Ok(())
}

#[tokio::test]
async fn a_document_with_a_sensitive_value_is_sensitive() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (doc, _) = lab_document(&mut c, &blobs, &f, "alex").await?;
    assert!(!document_is_sensitive(&mut c, doc).await?);
    let o = ObservationIn {
        document_id: Some(doc),
        sensitivity: vec!["sexual_health".into()],
        ..obs(f.profile("alex"), "1")
    };
    commit_observation(&mut c, &o, &manual_src().into(), CommitOptions::new("person")).await?;
    assert!(document_is_sensitive(&mut c, doc).await?);
    Ok(())
}
