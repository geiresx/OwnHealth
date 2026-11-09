//! Writing device samples (lane D): one path for Health Auto Export, Gadgetbridge and Nightscout into
//! `wearable_sample` (0.2.0 lane C's table), then the rollups of what changed.
//!
//! The unique key `(profile_id, metric, source_id, t)` makes every write idempotent. Two policies on a
//! sample that is already there:
//! - `Keep`: the first file to bring a sample owns it (Gadgetbridge exports, Nightscout readings);
//! - `Refresh`: a push may carry a newer value for the same bucket (Health Auto Export sums a minute or
//!   an hour that was still open at the previous push), so the value is replaced when it differs.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use ownhealth_core::Result;
use ownhealth_ingest_structured::wearables;
use sqlx::PgConnection;
use uuid::Uuid;

/// Rows per INSERT statement.
const CHUNK: usize = 5_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub metric: &'static str,
    pub t: DateTime<Utc>,
    pub t_end: Option<DateTime<Utc>>,
    pub v: f64,
    /// The original UTC offset in minutes, when the source says it.
    pub tz: Option<i16>,
    /// `wearable_source.id`.
    pub source: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    Keep,
    Refresh,
}

/// What a write did. `seen` counts distinct samples after merging repeats inside the batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Written {
    pub seen: i64,
    pub new: i64,
    pub refreshed: i64,
}

impl Written {
    /// Samples that were already stored (refreshed or not).
    pub fn duplicate(&self) -> i64 {
        self.seen - self.new
    }
}

/// The `wearable_source` of one device, linked to the connector `source` that brings it.
pub async fn device_source(
    conn: &mut PgConnection,
    profile_id: Uuid,
    provider: &str,
    device: Option<&str>,
    external_id: Option<&str>,
    source_id: Option<Uuid>,
) -> Result<i16> {
    let id = wearables::source_id(&mut *conn, profile_id, provider, device, external_id).await?;
    if let Some(s) = source_id {
        sqlx::query("update wearable_source set source_id = $2 where id = $1 and source_id is distinct from $2")
            .bind(id)
            .bind(s)
            .execute(conn)
            .await?;
    }
    Ok(id)
}

/// Insert `samples` for one profile, then rebuild the rollups of the metrics and sources that changed.
/// Repeats inside the batch (same metric, source and time) collapse to the last one.
pub async fn write(
    conn: &mut PgConnection,
    profile_id: Uuid,
    document_id: Option<Uuid>,
    samples: Vec<Sample>,
    conflict: Conflict,
) -> Result<Written> {
    let mut unique: BTreeMap<(&'static str, i16, DateTime<Utc>), Sample> = BTreeMap::new();
    for s in samples {
        if s.v.is_finite() {
            unique.insert((s.metric, s.source, s.t), s);
        }
    }
    let rows: Vec<Sample> = unique.into_values().collect();
    let mut out = Written {
        seen: rows.len() as i64,
        ..Written::default()
    };
    let on_conflict = match conflict {
        Conflict::Keep => "on conflict do nothing",
        Conflict::Refresh => {
            "on conflict (profile_id, metric, source_id, t) do update set v = excluded.v, t_end = excluded.t_end, \
             tz_offset_min = excluded.tz_offset_min where wearable_sample.v is distinct from excluded.v \
             or wearable_sample.t_end is distinct from excluded.t_end"
        }
    };
    let sql = format!(
        "insert into wearable_sample (profile_id, metric, t, t_end, v, tz_offset_min, source_id, document_id) \
         select $1, x.m, x.t, x.te, x.v, x.tz, x.s, $2 from unnest(cast($3 as text[]), cast($4 as timestamptz[]), \
         cast($5 as timestamptz[]), cast($6 as real[]), cast($7 as smallint[]), cast($8 as smallint[])) \
         as x(m, t, te, v, tz, s) {on_conflict} returning (xmax = 0) as inserted"
    );
    let mut touched_metrics: BTreeSet<String> = BTreeSet::new();
    let mut touched_sources: BTreeSet<i16> = BTreeSet::new();
    for chunk in rows.chunks(CHUNK) {
        let metrics: Vec<&str> = chunk.iter().map(|s| s.metric).collect();
        let ts: Vec<DateTime<Utc>> = chunk.iter().map(|s| s.t).collect();
        let ends: Vec<Option<DateTime<Utc>>> = chunk.iter().map(|s| s.t_end).collect();
        let vs: Vec<f32> = chunk.iter().map(|s| s.v as f32).collect();
        let tzs: Vec<Option<i16>> = chunk.iter().map(|s| s.tz).collect();
        let srcs: Vec<i16> = chunk.iter().map(|s| s.source).collect();
        let flags: Vec<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(profile_id)
            .bind(document_id)
            .bind(&metrics)
            .bind(&ts)
            .bind(&ends)
            .bind(&vs)
            .bind(&tzs)
            .bind(&srcs)
            .fetch_all(&mut *conn)
            .await?;
        if flags.is_empty() {
            continue;
        }
        out.new += flags.iter().filter(|f| **f).count() as i64;
        out.refreshed += flags.iter().filter(|f| !**f).count() as i64;
        // a chunk that wrote anything may have touched any of its metrics and sources
        touched_metrics.extend(chunk.iter().map(|s| s.metric.to_string()));
        touched_sources.extend(chunk.iter().map(|s| s.source));
    }
    if !touched_metrics.is_empty() {
        let metrics: Vec<String> = touched_metrics.into_iter().collect();
        let sources: Vec<i16> = touched_sources.into_iter().collect();
        wearables::recompute_rollups(&mut *conn, profile_id, Some(&metrics), Some(&sources)).await?;
    }
    Ok(out)
}
