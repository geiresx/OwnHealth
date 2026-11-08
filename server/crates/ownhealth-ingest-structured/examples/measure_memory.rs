//! Time the Apple Health import on a large synthetic export (`tests/imports/measure_memory.py`).
//! Not a test; run it under `/usr/bin/time -l` for the peak RSS (macOS) or `/usr/bin/time -v` (Linux):
//!
//! ```sh
//! /usr/bin/time -l cargo run --release -p ownhealth-ingest-structured --example measure_memory -- --days 400
//! /usr/bin/time -l cargo run --release -p ownhealth-ingest-structured --example measure_memory -- --days 400 --db
//! ```
//!
//! Parse mode streams export.xml straight out of the zip; `--db` runs the whole import into a
//! throwaway database on the shared test Postgres (dropped at the end).

use std::io::BufReader;
use std::time::Instant;

use chrono::NaiveDate;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::testing::TestDb;
use ownhealth_ingest_structured::apple_health::import_apple_health;
use ownhealth_ingest_structured::apple_health::parse::ExportReader;
use ownhealth_ingest_structured::apple_health::synthetic::{ExportSpec, write_export};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let days: i64 = args
        .iter()
        .position(|a| a == "--days")
        .and_then(|i| args.get(i + 1))
        .and_then(|d| d.parse().ok())
        .unwrap_or(400);
    let db = args.iter().any(|a| a == "--db");
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("export.zip");
    let spec = ExportSpec {
        days,
        hr_every_min: 1,
        end: NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default(),
        birth_date: NaiveDate::from_ymd_opt(1979, 5, 14).unwrap_or_default(),
        ..ExportSpec::default()
    };
    write_export(&path, &spec)?;
    let mut z = zip::ZipArchive::new(std::fs::File::open(&path)?)?;
    let xml_mb = z.by_name("apple_health_export/export.xml")?.size() as f64 / 1e6;
    let t0 = Instant::now();
    let n = if db {
        let test_db = TestDb::new().await?;
        let blobs = BlobStore::new(tmp.path().join("blobs"))?;
        let (sha, size) = blobs.put_file(&path)?;
        let mut tx = test_db.pool.begin().await?;
        let fam = create_rivera_family(&mut tx, "Rivera mem").await?;
        let doc = create_document(
            &mut tx,
            &NewDocument {
                filename: Some("export.zip"),
                ..NewDocument::upload(
                    fam.household_id,
                    Some(fam.profile("alex")),
                    &sha,
                    "application/zip",
                    size as i64,
                )
            },
        )
        .await?;
        let s = import_apple_health(&mut tx, &blobs, doc.id).await?;
        tx.commit().await?;
        s.accepted
    } else {
        let member = z.by_name("apple_health_export/export.xml")?;
        let mut reader = ExportReader::new(BufReader::new(member));
        let mut n = 0i64;
        while reader.next_sample().map_err(|e| anyhow::anyhow!("{e:?}"))?.is_some() {
            n += 1;
        }
        n
    };
    println!(
        "mode={} export.xml={xml_mb:.0} MB samples={n} seconds={:.1}",
        if db { "db" } else { "parse" },
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
