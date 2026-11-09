//! Gadgetbridge SQLite export (lane D): the Android app's database file, fully offline. Job kind
//! `import.gadgetbridge`, payload `{profile_id, household_id, blob_sha256, filename, channel?,
//! source_id?}` (as the other structured importers, `core::documents::job_kind`). The file becomes a
//! document of the profile and owns the samples it brings, so retracting it removes them.
//!
//! The database is copied to a temporary directory (an export can be in WAL mode, which SQLite cannot
//! open from a read-only blob) and read with rusqlite in `spawn_blocking`, `query_only` and
//! `trusted_schema = off`, only from tables (never views or triggers). Gadgetbridge has one table per
//! device family (greenDAO, `GBDaoGenerator.java`); tables are chosen by name suffix and columns, so a
//! new device family with the usual columns needs no change here:
//!
//! | Table suffix | Timestamp | Columns -> metric |
//! |---|---|---|
//! | `_ACTIVITY_SAMPLE` (Mi Band, Amazfit/Huami, Xiaomi, Pebble, ...) | seconds | `STEPS` -> steps (per minute, > 0), `HEART_RATE` -> hr |
//! | `_HEART_RATE_SAMPLE`, `_HEART_RATE_MANUAL_SAMPLE` | ms | `HEART_RATE` -> hr |
//! | `_HEART_RATE_RESTING_SAMPLE` | ms | `HEART_RATE` -> hr_resting |
//! | `_SPO2_SAMPLE` | ms | `SPO2` -> spo2 |
//! | `_RESPIRATORY_RATE_SAMPLE` | ms | `RESPIRATORY_RATE` or `RATE` -> resp_rate |
//! | `_WEIGHT_SAMPLE` (Mi Scale, generic scales) | ms | `WEIGHT_KG` -> weight |
//!
//! Gadgetbridge writes 0 or 255 where a device measured nothing; values outside a plausible range are
//! dropped. Other `*_SAMPLE` tables with rows (sleep stages, stress, PAI, HRV, vendor scores) are not
//! imported yet and are listed in the import summary by table. Sleep needs per-device stage decoding;
//! HRV needs the method stored with it (connectors.md 4), so neither is guessed here.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::DateTime;
use ownhealth_core::access::get_profile;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_ingest_structured::fhir::mark_imported;
use ownhealth_ingest_structured::jobs::document_for;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::store::{self, Conflict, Sample};

pub const JOB: &str = "import.gadgetbridge";
pub const PROVIDER: &str = "gadgetbridge";
/// Samples one file may hold: years of minute data from several devices.
pub const MAX_SAMPLES: usize = 20_000_000;

/// (table suffix, timestamp column holds ms, [(column, metric, min, max)]).
type Rule = (&'static str, &'static [(&'static str, &'static str, f64, f64)]);

// Longest suffix first: `_HEART_RATE_RESTING_SAMPLE` before `_HEART_RATE_SAMPLE`.
const RULES: [Rule; 7] = [
    (
        "_HEART_RATE_RESTING_SAMPLE",
        &[("HEART_RATE", "hr_resting", 25.0, 200.0)],
    ),
    ("_HEART_RATE_MANUAL_SAMPLE", &[("HEART_RATE", "hr", 25.0, 250.0)]),
    (
        "_RESPIRATORY_RATE_SAMPLE",
        &[
            ("RESPIRATORY_RATE", "resp_rate", 4.0, 60.0),
            ("RATE", "resp_rate", 4.0, 60.0),
        ],
    ),
    ("_HEART_RATE_SAMPLE", &[("HEART_RATE", "hr", 25.0, 250.0)]),
    (
        "_ACTIVITY_SAMPLE",
        &[("STEPS", "steps", 1.0, 1000.0), ("HEART_RATE", "hr", 25.0, 250.0)],
    ),
    ("_WEIGHT_SAMPLE", &[("WEIGHT_KG", "weight", 2.0, 400.0)]),
    ("_SPO2_SAMPLE", &[("SPO2", "spo2", 50.0, 100.0)]),
];

/// One device row (`DEVICE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub identifier: String,
}

/// A sample before its wearable source is resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Raw {
    pub device_id: i64,
    pub metric: &'static str,
    pub t_ms: i64,
    pub v: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Read {
    pub devices: Vec<Device>,
    pub samples: Vec<Raw>,
    /// Tables read, with the samples each gave.
    pub tables: BTreeMap<String, i64>,
    /// `*_SAMPLE` tables with rows that are not imported, with their row counts.
    pub not_imported: BTreeMap<String, i64>,
}

fn not_gadgetbridge(why: &str) -> Error {
    Error::Upload(format!("not a Gadgetbridge database: {why}"))
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn columns(db: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut st = db.prepare(&format!("pragma table_info({})", quote(table)))?;
    let cols = st
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(cols.into_iter().map(|c| c.to_uppercase()).collect())
}

fn sql_err(e: rusqlite::Error) -> Error {
    not_gadgetbridge(&e.to_string())
}

/// Read a Gadgetbridge database file. Blocking: call it from `spawn_blocking`.
pub fn read_db(path: &Path) -> Result<Read> {
    let dir = tempfile::tempdir()?;
    let copy = dir.path().join("gadgetbridge.db");
    std::fs::copy(path, &copy)?;
    let db = Connection::open_with_flags(
        &copy,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(sql_err)?;
    db.execute_batch("pragma trusted_schema = off; pragma query_only = on;")
        .map_err(sql_err)?;
    let tables: Vec<String> = {
        let mut st = db
            .prepare("select name from sqlite_master where type = 'table' order by name")
            .map_err(sql_err)?;
        st.query_map([], |r| r.get(0))
            .map_err(sql_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(sql_err)?
    };
    let device_table = tables
        .iter()
        .find(|t| t.eq_ignore_ascii_case("DEVICE"))
        .ok_or_else(|| not_gadgetbridge("no DEVICE table"))?;
    let dcols = columns(&db, device_table).map_err(sql_err)?;
    if !["_ID", "NAME", "IDENTIFIER"]
        .iter()
        .all(|c| dcols.iter().any(|d| d == c))
    {
        return Err(not_gadgetbridge("the DEVICE table has no _id, name and identifier"));
    }
    let alias = if dcols.iter().any(|c| c == "ALIAS") {
        "ALIAS"
    } else {
        "NULL"
    };
    let mut out = Read::default();
    {
        let mut st = db
            .prepare(&format!(
                "select _ID, NAME, IDENTIFIER, {alias} from {}",
                quote(device_table)
            ))
            .map_err(sql_err)?;
        let rows = st
            .query_map([], |r| {
                let alias: Option<String> = r.get(3)?;
                let name: Option<String> = r.get(1)?;
                Ok(Device {
                    id: r.get(0)?,
                    name: alias
                        .filter(|a| !a.trim().is_empty())
                        .or(name)
                        .unwrap_or_else(|| "Gadgetbridge device".into()),
                    identifier: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                })
            })
            .map_err(sql_err)?;
        for d in rows {
            out.devices.push(d.map_err(sql_err)?);
        }
    }
    for table in &tables {
        let upper = table.to_uppercase();
        if !upper.ends_with("_SAMPLE") {
            continue;
        }
        let cols = columns(&db, table).map_err(sql_err)?;
        let has = |c: &str| cols.iter().any(|x| x == c);
        let rule = RULES.iter().find(|(suffix, _)| upper.ends_with(suffix));
        let picked: Vec<(&str, &'static str, f64, f64)> = rule
            .map(|(_, cs)| cs.iter().copied().filter(|(c, ..)| has(c)).collect())
            .unwrap_or_default();
        if picked.is_empty() || !has("TIMESTAMP") || !has("DEVICE_ID") {
            let n: i64 = db
                .query_row(&format!("select count(*) from {}", quote(table)), [], |r| r.get(0))
                .map_err(sql_err)?;
            if n > 0 {
                out.not_imported.insert(table.clone(), n);
            }
            continue;
        }
        // respiratory rate: one column of the two
        let mut seen_metric: Vec<&str> = Vec::new();
        let picked: Vec<_> = picked
            .into_iter()
            .filter(|(_, m, ..)| {
                let first = !seen_metric.contains(m);
                seen_metric.push(m);
                first
            })
            .collect();
        let select: Vec<String> = picked.iter().map(|(c, ..)| (*c).to_string()).collect();
        let sql = format!(
            "select TIMESTAMP, DEVICE_ID, {} from {}",
            select.join(", "),
            quote(table)
        );
        let mut st = db.prepare(&sql).map_err(sql_err)?;
        let mut rows = st.query([]).map_err(sql_err)?;
        let mut n = 0i64;
        while let Some(r) = rows.next().map_err(sql_err)? {
            let ts: i64 = r.get(0).map_err(sql_err)?;
            let device_id: i64 = r.get(1).map_err(sql_err)?;
            // greenDAO activity samples hold seconds, time samples milliseconds
            let t_ms = if ts > 100_000_000_000 {
                ts
            } else {
                ts.saturating_mul(1000)
            };
            for (i, (_, metric, lo, hi)) in picked.iter().enumerate() {
                let v: Option<f64> = r.get(i + 2).map_err(sql_err)?;
                if let Some(v) = v.filter(|v| (*lo..=*hi).contains(v)) {
                    out.samples.push(Raw {
                        device_id,
                        metric,
                        t_ms,
                        v,
                    });
                    n += 1;
                }
            }
            if out.samples.len() > MAX_SAMPLES {
                return Err(Error::Upload(format!(
                    "the database holds more than {MAX_SAMPLES} samples: export a shorter period"
                )));
            }
        }
        out.tables.insert(table.clone(), n);
    }
    Ok(out)
}

/// Import a stored Gadgetbridge database document into its profile.
pub async fn import_gadgetbridge(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    source_id: Option<Uuid>,
) -> Result<ImportSummary> {
    let (profile_id, sha, flags): (Option<Uuid>, String, Value) =
        sqlx::query_as("select profile_id, encode(blob_sha256, 'hex'), flags from document where id = $1")
            .bind(document_id)
            .fetch_one(&mut *conn)
            .await?;
    if let Some(done) = flags.get("import").filter(|d| d.is_object()) {
        // imported before (a retried job): the stored summary
        let n = |k: &str| done.get(k).and_then(Value::as_i64).unwrap_or(0);
        let mut s = ImportSummary::new(document_id.to_string());
        (s.accepted, s.queued, s.skipped) = (n("accepted"), n("queued"), n("skipped"));
        s.reasons = done
            .get("reasons")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, c)| (k.clone(), c.as_i64().unwrap_or(0))).collect())
            .unwrap_or_default();
        return Ok(s);
    }
    let profile_id = profile_id.ok_or_else(|| Error::Invalid("the document has no profile".into()))?;
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("unknown profile {profile_id}")))?;
    let path = blobs.path(&sha)?;
    let read = tokio::task::spawn_blocking(move || read_db(&path))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("gadgetbridge read task failed: {e}")))??;
    let mut ids: HashMap<i64, i16> = HashMap::new();
    for d in &read.devices {
        let ext = (!d.identifier.is_empty()).then_some(d.identifier.as_str());
        ids.insert(
            d.id,
            store::device_source(&mut *conn, profile.id, PROVIDER, Some(&d.name), ext, source_id).await?,
        );
    }
    let mut unknown_device = 0i64;
    let mut samples = Vec::with_capacity(read.samples.len());
    for s in &read.samples {
        let (Some(source), Some(t)) = (ids.get(&s.device_id), DateTime::from_timestamp_millis(s.t_ms)) else {
            unknown_device += 1;
            continue;
        };
        samples.push(Sample {
            metric: s.metric,
            t,
            t_end: None,
            v: s.v,
            tz: None,
            source: *source,
        });
    }
    let w = store::write(&mut *conn, profile.id, Some(document_id), samples, Conflict::Keep).await?;
    let mut summary = ImportSummary::new(document_id.to_string());
    summary.accepted = w.new;
    summary.skipped = w.duplicate() + unknown_device;
    if w.duplicate() > 0 {
        summary.reasons.insert("duplicate".into(), w.duplicate());
    }
    if unknown_device > 0 {
        summary.reasons.insert("unknown_device".into(), unknown_device);
    }
    let details = json!({
        "samples": w.new,
        "duplicates": w.duplicate(),
        "devices": read.devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
        "tables": read.tables,
        "not_imported": read.not_imported,
    });
    sqlx::query(
        "update document set flags = flags || jsonb_build_object('gadgetbridge', cast($1 as jsonb)) where id = $2",
    )
    .bind(&details)
    .bind(document_id)
    .execute(&mut *conn)
    .await?;
    mark_imported(conn, document_id, &summary, None).await?;
    Ok(summary)
}

async fn run(ctx: JobContext, job: Job) -> jobs::HandlerResult {
    let channel = job
        .str("channel")
        .filter(|c| !c.is_empty())
        .unwrap_or(PROVIDER)
        .to_string();
    let source_id = job.str("source_id").and_then(|s| Uuid::parse_str(s).ok());
    let mut tx = ctx.pool.begin().await?;
    let doc = document_for(&mut tx, &ctx.blobs, &job.payload, "application/vnd.sqlite3", &channel).await?;
    let summary = import_gadgetbridge(&mut tx, &ctx.blobs, doc, source_id)
        .await
        .map_err(|e| match e {
            Error::Upload(m) => JobError::permanent(m),
            Error::Invalid(m) => JobError::permanent(format!("not a readable Gadgetbridge database: {m}")),
            other => JobError::from(other),
        })?;
    tx.commit().await?;
    Ok(Some(summary.to_value()))
}

pub fn register() {
    jobs::register_handler(JOB, |ctx, job| Box::pin(run(ctx, job)));
}
