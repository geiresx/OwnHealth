//! Retracting a structured document withdraws everything it brought in: observations, records (through
//! `src.document_id`), wearable samples with their rollups, and the ECG documents an export created
//! (`tests/imports/test_document_retract.py`). The cases that move a document to another profile go
//! through `assign_document` (lane B) and the retract endpoint (lane I); they are tested there.

mod common;

use std::collections::BTreeSet;
use std::io::{Cursor, Read};

use chrono::NaiveDate;
use common::{Env, JSON, R, ZIP, family, fhir_file, store_doc};
use ownhealth_core::demo::Family;
use ownhealth_core::records::{active_medications, list_records};
use ownhealth_ingest_structured::apple_health::import_apple_health;
use ownhealth_ingest_structured::apple_health::synthetic::{ExportSpec, write_export_to};
use ownhealth_ingest_structured::fhir::import_fhir;
use ownhealth_ingest_structured::retract::retract_document_facts;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

fn spec(days: i64, ecg_count: usize) -> ExportSpec {
    ExportSpec {
        days,
        end: NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default(),
        birth_date: NaiveDate::from_ymd_opt(1979, 5, 14).unwrap_or_default(),
        ecg_count,
        ..ExportSpec::default()
    }
}

fn zip_of(spec: &ExportSpec) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    write_export_to(&mut buf, spec).unwrap_or_else(|e| panic!("zip: {e}"));
    buf.into_inner()
}

async fn current_obs(conn: &mut PgConnection, p: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from observation where profile_id = $1 and status in ('current', 'provisional')",
    )
    .bind(p)
    .fetch_one(conn)
    .await?)
}

async fn wearables(conn: &mut PgConnection, p: Uuid) -> anyhow::Result<(i64, i64)> {
    let n: i64 = sqlx::query_scalar("select count(*) from wearable_sample where profile_id = $1")
        .bind(p)
        .fetch_one(&mut *conn)
        .await?;
    let r: i64 = sqlx::query_scalar("select count(*) from wearable_rollup where profile_id = $1")
        .bind(p)
        .fetch_one(&mut *conn)
        .await?;
    Ok((n, r))
}

async fn ecgs(conn: &mut PgConnection, p: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from document where profile_id = $1 and doc_type = 'ecg' and status = 'active'",
    )
    .bind(p)
    .fetch_one(conn)
    .await?)
}

async fn fhir_doc(env: &Env, conn: &mut PgConnection, fam: &Family) -> anyhow::Result<Uuid> {
    let p = fam.profile("alex");
    let doc = store_doc(
        &mut *conn,
        &env.blobs,
        fam,
        "alex",
        &fhir_file("ips-alex.json"),
        "ips.json",
        JSON,
    )
    .await?;
    import_fhir(&mut *conn, &env.blobs, doc).await?;
    assert!(
        !active_medications(&mut *conn, p, None, None, None, true)
            .await?
            .is_empty()
    );
    assert!(!list_records(&mut *conn, "condition", p, None, true).await?.is_empty());
    assert!(current_obs(&mut *conn, p).await? > 0);
    Ok(doc)
}

async fn apple_doc(env: &Env, conn: &mut PgConnection, fam: &Family, s: &ExportSpec) -> anyhow::Result<Uuid> {
    let doc = store_doc(&mut *conn, &env.blobs, fam, "alex", &zip_of(s), "e.zip", ZIP).await?;
    import_apple_health(&mut *conn, &env.blobs, doc).await?;
    let (n, r) = wearables(&mut *conn, fam.profile("alex")).await?;
    assert!(n > 0 && r > 0);
    Ok(doc)
}

#[tokio::test]
async fn retracting_a_fhir_document_withdraws_its_records() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let doc = fhir_doc(&env, &mut tx, &fam).await?;
    let out = retract_document_facts(&mut tx, doc, "document retracted: wrong patient").await?;
    assert!(active_medications(&mut tx, p, None, None, None, true).await?.is_empty());
    for kind in ["condition", "allergy", "immunization", "procedure"] {
        assert!(list_records(&mut tx, kind, p, None, true).await?.is_empty(), "{kind}");
    }
    assert_eq!(current_obs(&mut tx, p).await?, 0);
    assert_eq!(out["observations"].as_array().map(Vec::len), Some(3));
    let tables: BTreeSet<&str> = out["records"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    assert!(
        tables.is_superset(&BTreeSet::from(["condition", "medication_statement", "allergy"])),
        "{tables:?}"
    );
    // and the import can run again: its stored summary is gone
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await?;
    assert!(flags.get("import").is_none());
    let again = import_fhir(&mut tx, &env.blobs, doc).await?;
    assert_eq!(again.accepted, 8); // a real import, not the old summary
    Ok(())
}

#[tokio::test]
async fn retracting_an_apple_export_deletes_its_samples_and_rollups() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let doc = apple_doc(&env, &mut tx, &fam, &spec(3, 0)).await?;
    let out = retract_document_facts(&mut tx, doc, "document retracted: not mine").await?;
    assert!(out["wearable_samples"].as_i64().unwrap_or_default() > 0);
    assert_eq!(wearables(&mut tx, p).await?, (0, 0));
    assert!(active_medications(&mut tx, p, None, None, None, true).await?.is_empty()); // its clinical records go too
    Ok(())
}

#[tokio::test]
async fn another_exports_samples_stay() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    apple_doc(&env, &mut tx, &fam, &spec(3, 0)).await?;
    let before = wearables(&mut tx, p).await?;
    let unrelated = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        b"%PDF-1.4 other",
        "x.pdf",
        "application/pdf",
    )
    .await?;
    let out = retract_document_facts(&mut tx, unrelated, "not mine").await?;
    assert_eq!(out["wearable_samples"], 0);
    assert_eq!(wearables(&mut tx, p).await?, before);
    Ok(())
}

#[tokio::test]
async fn retracting_an_apple_export_retracts_its_ecg_documents() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let alex = fam.profile("alex");
    let doc = apple_doc(&env, &mut tx, &fam, &spec(2, 2)).await?;
    assert_eq!(ecgs(&mut tx, alex).await?, 2);
    let out = retract_document_facts(&mut tx, doc, "document retracted: not mine").await?;
    assert_eq!(ecgs(&mut tx, alex).await?, 0);
    assert_eq!(out["documents"].as_array().map(Vec::len), Some(2));
    import_apple_health(&mut tx, &env.blobs, doc).await?; // the same export again brings them back
    assert_eq!(ecgs(&mut tx, alex).await?, 2);
    Ok(())
}

/// An ECG document that existed before the export is not the export's: it stays when the export goes.
#[tokio::test]
async fn an_ecg_the_person_uploaded_on_its_own_stays() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let data = zip_of(&spec(2, 1));
    let mut z = zip::ZipArchive::new(Cursor::new(data.clone()))?;
    let name = z
        .file_names()
        .filter_map(|n| n.ok().map(|n| n.to_string()))
        .find(|n| n.contains("electrocardiograms/"))
        .ok_or_else(|| anyhow::anyhow!("no ecg member"))?;
    let mut csv = Vec::new();
    z.by_name(&name)?.read_to_end(&mut csv)?;
    let own = store_doc(&mut tx, &env.blobs, &fam, "alex", &csv, "ecg.csv", "text/csv").await?;
    let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &data, "e.zip", ZIP).await?;
    import_apple_health(&mut tx, &env.blobs, doc).await?;
    retract_document_facts(&mut tx, doc, "document retracted: not mine").await?;
    let status: String = sqlx::query_scalar("select status from document where id = $1")
        .bind(own)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(status, "active");
    Ok(())
}
