//! Apple Health export.zip import (`ingest/apple_health/__init__.py`; connectors.md 2.2 and 4). Job
//! kind `import.apple_health`.
//!
//! - `export.xml` is streamed (`parse::ExportReader`, constant memory) straight from the zip on a
//!   blocking thread; samples go to `wearable_sample` in batches through COPY, then the hour and day
//!   rollups of the touched metrics are rebuilt. Every raw sample is kept: the Watch and the iPhone
//!   both stay, and `wearables::series` picks one per day by source priority.
//! - `clinical-records/*.json` are FHIR R4 resources and go through `fhir::import_resources`.
//! - `electrocardiograms/*.csv` are stored as documents of type `ecg`.
//! - The birth date in `<Me>` must match the profile: a different one stops the import (wrong profile).
//! - Members read whole (clinical JSON, ECG CSV) are capped in size, and the zip in member count, so a
//!   small zip that inflates to gigabytes stops the job instead of the process.
//!
//! Errors: `Error::Upload` is Python's `PermanentJobError` raised by the importer itself (the job
//! fails for good with that message); `Error::Invalid` is a file that does not read (bad zip, XML or
//! JSON), which the job also fails for good, as "not a readable Apple Health export".

pub mod parse;
pub mod synthetic;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::access::get_profile;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::provenance::{SrcIn, SrcRef, ensure_src};
use ownhealth_core::{Error, Result};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::fhir::{self, Entry};
use crate::wearables;
use parse::{ExportReader, Sample};

pub const BATCH: usize = 10_000;
pub const PROVIDER: &str = "apple_health";
pub const MAX_MEMBERS: usize = 50_000;
/// One FHIR resource.
pub const MAX_CLINICAL_BYTES: u64 = 20 * 1024 * 1024;
/// One 30 s recording is a few hundred KB.
pub const MAX_ECG_BYTES: u64 = 5 * 1024 * 1024;

fn unreadable(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

struct Members {
    export: Option<String>,
    clinical: Vec<String>,
    ecg: Vec<String>,
}

fn open_zip(path: &Path) -> Result<zip::ZipArchive<std::fs::File>> {
    zip::ZipArchive::new(std::fs::File::open(path)?).map_err(unreadable)
}

/// The members, checked before anything is read or written: the member count, and the declared size
/// of every member read whole (reads are cut at that size too).
fn members(path: &Path) -> Result<Members> {
    let mut z = open_zip(path)?;
    if z.len() > MAX_MEMBERS {
        return Err(Error::Upload(format!(
            "the zip holds more than {MAX_MEMBERS} files: not an Apple Health export"
        )));
    }
    let mut m = Members {
        export: None,
        clinical: Vec::new(),
        ecg: Vec::new(),
    };
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for i in 0..z.len() {
        let f = z.by_index_raw(i).map_err(unreadable)?;
        let name = f.name().map_err(unreadable)?.to_string();
        let low = name.to_lowercase();
        let base = low.rsplit('/').next().unwrap_or_default();
        let rooted = format!("/{low}");
        if base == "export.xml" {
            m.export = Some(name.clone());
        } else if rooted.contains("/clinical-records/") && low.ends_with(".json") {
            m.clinical.push(name.clone());
        } else if rooted.contains("/electrocardiograms/") && low.ends_with(".csv") {
            m.ecg.push(name.clone());
        }
        sizes.insert(name, f.size());
    }
    m.clinical.sort();
    m.ecg.sort();
    for (names, cap) in [(&m.clinical, MAX_CLINICAL_BYTES), (&m.ecg, MAX_ECG_BYTES)] {
        for name in names {
            if sizes.get(name).copied().unwrap_or(0) > cap {
                return Err(Error::Upload(format!(
                    "{name}: over {} MB, not an Apple Health export file",
                    cap / (1024 * 1024)
                )));
            }
        }
    }
    Ok(m)
}

/// Read whole members, never more than `cap` bytes each.
fn read_members(path: &Path, names: &[String], cap: u64) -> Result<Vec<(String, Vec<u8>)>> {
    let mut z = open_zip(path)?;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let f = z.by_name(name).map_err(unreadable)?;
        let mut data = Vec::new();
        f.take(cap + 1).read_to_end(&mut data).map_err(unreadable)?;
        if data.len() as u64 > cap {
            return Err(Error::Upload(format!(
                "{name}: over {} MB, not an Apple Health export file",
                cap / (1024 * 1024)
            )));
        }
        out.push((name.clone(), data));
    }
    Ok(out)
}

/// What the reader learnt besides the samples.
struct ReadInfo {
    birth_date: Option<NaiveDate>,
    export_date: Option<String>,
    not_imported: Vec<(String, i64)>,
}

/// Parse export.xml on a blocking thread and hand the samples over in batches.
fn spawn_reader(
    path: PathBuf,
    export: String,
    tx: mpsc::Sender<Vec<Sample>>,
) -> tokio::task::JoinHandle<Result<ReadInfo>> {
    tokio::task::spawn_blocking(move || {
        let mut z = open_zip(&path)?;
        let f = z.by_name(&export).map_err(unreadable)?;
        let mut reader = ExportReader::new(BufReader::with_capacity(64 * 1024, f));
        let mut batch = Vec::with_capacity(BATCH);
        while let Some(s) = reader.next_sample().map_err(unreadable)? {
            batch.push(s);
            if batch.len() >= BATCH
                && tx
                    .blocking_send(std::mem::replace(&mut batch, Vec::with_capacity(BATCH)))
                    .is_err()
            {
                return Err(Error::Invalid("the import stopped reading".into()));
            }
        }
        if !batch.is_empty() && tx.blocking_send(batch).is_err() {
            return Err(Error::Invalid("the import stopped reading".into()));
        }
        Ok(ReadInfo {
            birth_date: reader.birth_date(),
            export_date: reader.export_date.clone(),
            not_imported: reader.most_common(30),
        })
    })
}

pub async fn import_apple_health(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
) -> Result<ImportSummary> {
    let (profile_id, household_id, sha, flags): (Option<Uuid>, Uuid, String, Value) = sqlx::query_as(
        "select profile_id, household_id, encode(blob_sha256, 'hex'), flags from document where id = $1",
    )
    .bind(document_id)
    .fetch_one(&mut *conn)
    .await?;
    if let Some(done) = flags.get("import").filter(|d| crate::py::truthy(Some(d))) {
        return Ok(fhir::summary_from(done));
    }
    let profile_id = profile_id.ok_or_else(|| Error::Invalid("the document has no profile".into()))?;
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
    let mut summary = ImportSummary::new(document_id.to_string());
    let path = blobs.path(&sha)?;
    let m = {
        let p = path.clone();
        tokio::task::spawn_blocking(move || members(&p))
            .await
            .map_err(unreadable)??
    };
    let Some(export) = m.export.clone() else {
        return Err(Error::Upload("no export.xml in the zip".into()));
    };

    let (staged, metrics, sources, info) = stage_samples(&mut *conn, profile.id, &path, export).await?;
    let dob = info.birth_date;
    if let (Some(d), Some(p)) = (dob, profile.birth_date)
        && d != p
    {
        return Err(Error::Upload(format!(
            "the export's birth date does not match profile '{}': import it into the right profile",
            profile.slug
        )));
    }
    let identity = if dob.is_some() && dob == profile.birth_date {
        "matched"
    } else {
        "missing"
    };
    let inserted = commit_samples(&mut *conn, profile.id, document_id).await?;
    if !metrics.is_empty() {
        let metrics: Vec<String> = metrics.iter().cloned().collect();
        let sources: Vec<i16> = sources.values().copied().collect::<BTreeSet<_>>().into_iter().collect();
        wearables::recompute_rollups(&mut *conn, profile.id, Some(&metrics), Some(&sources)).await?;
    }
    if inserted > 0 {
        // the citation for these samples: get_trend and the dashboard name this export
        let src = SrcIn {
            profile_id: Some(profile.id),
            document_id: Some(document_id),
            import_ref: Some(format!("export.xml#{document_id}")),
            label: Some("Apple Health export".into()),
            ..SrcIn::new("apple_health")
        }
        .validate()?;
        ensure_src(&mut *conn, &SrcRef::New(src), Some(profile.id)).await?;
    }
    summary.accepted += inserted;
    summary.skipped += staged - inserted;
    if staged - inserted > 0 {
        summary.reasons.insert("duplicate".into(), staged - inserted);
    }

    let clinical = {
        let (p, names) = (path.clone(), m.clinical.clone());
        tokio::task::spawn_blocking(move || read_members(&p, &names, MAX_CLINICAL_BYTES))
            .await
            .map_err(unreadable)??
    };
    let mut entries = Vec::with_capacity(clinical.len());
    for (name, data) in clinical {
        let resource: Value = serde_json::from_slice(&data).map_err(unreadable)?;
        entries.push(Entry {
            pointer: name,
            resource,
            full_url: None,
        });
    }
    if !entries.is_empty() {
        let key = format!("apple-{}", &sha[..12]);
        let (s, _) = fhir::import_resources(
            &mut *conn,
            &entries,
            &profile,
            document_id,
            &key,
            Some(identity),
            "Apple Health",
        )
        .await?;
        summary.accepted += s.accepted;
        summary.queued += s.queued;
        summary.skipped += s.skipped;
        for (k, v) in s.reasons {
            *summary.reasons.entry(k).or_default() += v;
        }
    }
    drop(entries);

    let ecgs = {
        let (p, names) = (path.clone(), m.ecg.clone());
        tokio::task::spawn_blocking(move || read_members(&p, &names, MAX_ECG_BYTES))
            .await
            .map_err(unreadable)??
    };
    for (name, data) in &ecgs {
        store_ecg(&mut *conn, blobs, household_id, profile.id, name, data, document_id).await?;
    }

    let not_imported: Map<String, Value> = info.not_imported.into_iter().map(|(k, v)| (k, json!(v))).collect();
    let details = json!({
        "samples": inserted,
        "duplicates": staged - inserted,
        "metrics": metrics.iter().collect::<Vec<_>>(),
        "clinical_records": m.clinical.len(),
        "ecg_documents": m.ecg.len(),
        "not_imported": not_imported,
        "export_date": info.export_date,
    });
    sqlx::query(
        "update document set flags = flags || jsonb_build_object('apple_health', cast($1 as jsonb)) where id = $2",
    )
    .bind(&details)
    .bind(document_id)
    .execute(&mut *conn)
    .await?;
    fhir::mark_imported(conn, document_id, &summary, Some(identity)).await?;
    Ok(summary)
}

pub async fn rebuild_rollups(conn: &mut PgConnection, profile_id: Uuid, metric: Option<&str>) -> Result<i64> {
    let metrics: Option<Vec<String>> = metric.map(|m| vec![m.to_string()]);
    wearables::recompute_rollups(conn, profile_id, metrics.as_deref(), None).await
}

// ------------------------------------------------------------------ samples ----

/// Stream samples into a temp table in COPY batches. Returns (rows staged, metrics, source ids by
/// device, what the reader learnt).
async fn stage_samples(
    conn: &mut PgConnection,
    profile_id: Uuid,
    path: &Path,
    export: String,
) -> Result<(i64, BTreeSet<String>, BTreeMap<String, i16>, ReadInfo)> {
    sqlx::query(
        "create temp table if not exists ah_stage (metric text, t timestamptz, t_end timestamptz, v real, \
         tz smallint, source_id smallint) on commit drop",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query("truncate ah_stage").execute(&mut *conn).await?;
    let (tx, mut rx) = mpsc::channel::<Vec<Sample>>(2);
    let reader = spawn_reader(path.to_path_buf(), export, tx);
    let mut sources: BTreeMap<String, i16> = BTreeMap::new();
    let mut metrics: BTreeSet<String> = BTreeSet::new();
    let mut total = 0i64;
    let mut failed = None;
    while let Some(batch) = rx.recv().await {
        match copy(&mut *conn, profile_id, &batch, &mut sources, &mut metrics).await {
            Ok(n) => total += n,
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    drop(rx);
    let read = reader.await.map_err(unreadable)?;
    if let Some(e) = failed {
        return Err(e);
    }
    Ok((total, metrics, sources, read?))
}

fn copy_ts(t: &DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S%.6f+00").to_string()
}

fn copy_real(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        format!("{v}")
    }
}

async fn copy(
    conn: &mut PgConnection,
    profile_id: Uuid,
    batch: &[Sample],
    sources: &mut BTreeMap<String, i16>,
    metrics: &mut BTreeSet<String>,
) -> Result<i64> {
    for s in batch {
        if !sources.contains_key(&s.device) {
            let id = wearables::source_id(&mut *conn, profile_id, PROVIDER, Some(&s.device), None).await?;
            sources.insert(s.device.clone(), id);
        }
        metrics.insert(s.metric.to_string());
    }
    let mut text = String::with_capacity(batch.len() * 96);
    for s in batch {
        let t_end = s.t_end.as_ref().map_or_else(|| "\\N".to_string(), copy_ts);
        let sid = sources.get(&s.device).copied().unwrap_or_default();
        let _ = writeln!(
            text,
            "{}\t{}\t{t_end}\t{}\t{}\t{sid}",
            s.metric,
            copy_ts(&s.t),
            copy_real(s.v),
            s.tz_offset_min
        );
    }
    let mut cp = conn
        .copy_in_raw("copy ah_stage (metric, t, t_end, v, tz, source_id) from stdin")
        .await?;
    cp.send(text.into_bytes()).await?;
    cp.finish().await?;
    Ok(batch.len() as i64)
}

/// Move staged rows into wearable_sample, owned by this export. A re-import of the same samples
/// inserts nothing.
async fn commit_samples(conn: &mut PgConnection, profile_id: Uuid, document_id: Uuid) -> Result<i64> {
    let done = sqlx::query(
        "insert into wearable_sample (profile_id, metric, t, t_end, v, tz_offset_min, source_id, document_id) \
         select distinct on (metric, source_id, t) $1, metric, t, t_end, v, tz, source_id, $2 from ah_stage \
         order by metric, source_id, t on conflict do nothing",
    )
    .bind(profile_id)
    .bind(document_id)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() as i64)
}

// ---------------------------------------------------------- clinical, ECG ----

/// One ECG CSV as its own document, owned by the export that brought it (`flags.parent_document`),
/// so retracting or moving the export takes it along. An ECG document that already existed on its
/// own stays its own.
async fn store_ecg(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    household_id: Uuid,
    profile_id: Uuid,
    name: &str,
    data: &[u8],
    parent: Uuid,
) -> Result<()> {
    let sha = blobs.put_bytes(data)?;
    let filename = name.rsplit('/').next().unwrap_or(name);
    let r = create_document(
        &mut *conn,
        &NewDocument {
            filename: Some(filename),
            channel: "apple_health",
            doc_type: Some("ecg"),
            ..NewDocument::upload(household_id, Some(profile_id), &sha, "text/csv", data.len() as i64)
        },
    )
    .await?;
    if r.created || r.reactivated {
        sqlx::query(
            "update document set flags = flags || jsonb_build_object('parent_document', cast($1 as text)) where id = $2",
        )
        .bind(parent.to_string())
        .bind(r.id)
        .execute(&mut *conn)
        .await?;
    }
    let text = String::from_utf8_lossy(data);
    let mut recorded = None;
    for line in crate::py::splitlines(text.as_bytes()).into_iter().take(20) {
        let line = String::from_utf8_lossy(line);
        let (key, value) = line.split_once(',').unwrap_or((&line, ""));
        let key = key.trim().to_lowercase();
        if key == "recorded date" || key == "fecha de registro" {
            recorded = parse::parse_time(value.trim().trim_matches('"')).map(|(t, _)| t);
        }
    }
    if let Some(c) = recorded {
        sqlx::query("update document set collected_at = $1 where id = $2")
            .bind(c)
            .bind(r.id)
            .execute(conn)
            .await?;
    }
    Ok(())
}
