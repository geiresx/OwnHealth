//! Wearable samples, rollups and the source-priority rule (`wearables/__init__.py`; connectors.md 4,
//! architecture.md 6.4).
//!
//! Samples keep UTC (`t`) plus the original offset (`tz_offset_min`). Rollups are stored per source:
//! hourly buckets on the UTC hour, daily buckets on the local calendar date of the sample (stored as
//! that date at 00:00 UTC). Sleep counts on the date of waking: each stage is dated by the end of its
//! night.
//!
//! Source priority applies at query time, never at ingest: every raw sample is kept, and a daily
//! series takes, for each day, the highest-priority source that has data that day. That stops an
//! iPhone and an Apple Watch both counting the same steps. A `source_priority` row overrides the
//! default order (Watch, then iPhone, then anything else).

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::Result;
use ownhealth_core::pyjson::{f32_json, py_isoformat_utc, pydantic_datetime_utc};
use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metric {
    pub key: String,
    pub unit: &'static str,
    /// `sum`: the day value is the total; `mean`: the day value is the average.
    pub agg: &'static str,
    /// One reading per day or night: coverage is 1 when present.
    pub daily: bool,
    pub label_en: String,
    pub label_es: String,
}

type Spec = (
    &'static str,
    &'static str,
    &'static str,
    bool,
    &'static str,
    &'static str,
);

pub const METRICS: [Spec; 15] = [
    ("steps", "count", "sum", false, "Steps", "Pasos"),
    ("hr", "/min", "mean", false, "Heart rate", "Frecuencia cardiaca"),
    ("hr_resting", "/min", "mean", true, "Resting heart rate", "FC en reposo"),
    ("hrv_sdnn", "ms", "mean", true, "HRV (SDNN)", "VFC (SDNN)"),
    ("hrv_rmssd", "ms", "mean", true, "HRV (RMSSD)", "VFC (RMSSD)"),
    (
        "vo2max",
        "mL/kg/min",
        "mean",
        true,
        "VO₂max estimate",
        "VO₂máx estimado",
    ),
    ("spo2", "%", "mean", false, "Blood oxygen", "Oxígeno en sangre"),
    (
        "resp_rate",
        "/min",
        "mean",
        false,
        "Respiratory rate",
        "Frecuencia respiratoria",
    ),
    ("weight", "kg", "mean", true, "Weight", "Peso"),
    (
        "bp_systolic",
        "mm[Hg]",
        "mean",
        true,
        "Blood pressure, systolic",
        "Presión arterial sistólica",
    ),
    (
        "bp_diastolic",
        "mm[Hg]",
        "mean",
        true,
        "Blood pressure, diastolic",
        "Presión arterial diastólica",
    ),
    ("active_energy", "kcal", "sum", false, "Active energy", "Energía activa"),
    (
        "distance",
        "km",
        "sum",
        false,
        "Walking + running distance",
        "Distancia caminando y corriendo",
    ),
    (
        "sleep_asleep",
        "min",
        "sum",
        true,
        "Sleep duration",
        "Duración del sueño",
    ),
    ("sleep_in_bed", "min", "sum", true, "Time in bed", "Tiempo en cama"),
];

pub const SLEEP_METRICS: [&str; 2] = ["sleep_asleep", "sleep_in_bed"];

pub fn daily_metrics() -> Vec<String> {
    METRICS.iter().filter(|m| m.3).map(|m| m.0.to_string()).collect()
}

/// Known metric, or a neutral description for one another importer added.
pub fn metric(key: &str) -> Metric {
    match METRICS.iter().find(|m| m.0 == key) {
        Some(&(key, unit, agg, daily, en, es)) => Metric {
            key: key.into(),
            unit,
            agg,
            daily,
            label_en: en.into(),
            label_es: es.into(),
        },
        None => Metric {
            key: key.into(),
            unit: "",
            agg: "mean",
            daily: false,
            label_en: key.into(),
            label_es: key.into(),
        },
    }
}

/// The metric as JSON (`key`, `unit`, `agg`, `daily`, `label_en`, `label_es`); None when unknown.
pub fn metric_info(key: &str) -> Option<Value> {
    METRICS.iter().find(|m| m.0 == key).map(|&(key, unit, agg, daily, en, es)| {
        json!({"key": key, "unit": unit, "agg": agg, "daily": daily, "label_en": en, "label_es": es})
    })
}

// ------------------------------------------------------------------- sources ----

/// Get or create the wearable_source row for one device of one profile.
pub async fn source_id(
    conn: &mut PgConnection,
    profile_id: Uuid,
    provider: &str,
    device: Option<&str>,
    external_id: Option<&str>,
) -> Result<i16> {
    loop {
        let sid: Option<i16> = sqlx::query_scalar(
            "select id from wearable_source where profile_id = $1 and provider = $2 \
             and coalesce(device, '') = coalesce(cast($3 as text), '') \
             and coalesce(external_id, '') = coalesce(cast($4 as text), '')",
        )
        .bind(profile_id)
        .bind(provider)
        .bind(device)
        .bind(external_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = sid {
            return Ok(id);
        }
        let created: Option<i16> = sqlx::query_scalar(
            "insert into wearable_source (profile_id, provider, device, external_id) values ($1, $2, $3, $4) \
             on conflict do nothing returning id",
        )
        .bind(profile_id)
        .bind(provider)
        .bind(device)
        .bind(external_id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = created {
            return Ok(id);
        }
        // created by a concurrent import: read it again
    }
}

pub fn default_rank(device: Option<&str>) -> i32 {
    let d = device.unwrap_or_default().to_lowercase();
    if d.contains("watch") {
        0
    } else if d.contains("iphone") {
        1
    } else {
        2
    }
}

/// Source ids for one metric, best first: the profile's source_priority row, then the default order.
pub async fn priority(conn: &mut PgConnection, profile_id: Uuid, metric: &str) -> Result<Vec<i16>> {
    let sources: Vec<(i16, Option<String>)> =
        sqlx::query_as("select id, device from wearable_source where profile_id = $1 order by id")
            .bind(profile_id)
            .fetch_all(&mut *conn)
            .await?;
    let explicit: Vec<i16> =
        sqlx::query_scalar("select source_ids from source_priority where profile_id = $1 and metric = $2")
            .bind(profile_id)
            .bind(metric)
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or_default();
    let mut rest: Vec<&(i16, Option<String>)> = sources.iter().filter(|s| !explicit.contains(&s.0)).collect();
    rest.sort_by_key(|s| (default_rank(s.1.as_deref()), s.0));
    Ok(explicit.iter().copied().chain(rest.into_iter().map(|s| s.0)).collect())
}

pub async fn set_priority(conn: &mut PgConnection, profile_id: Uuid, metric: &str, source_ids: &[i16]) -> Result<()> {
    sqlx::query(
        "insert into source_priority (profile_id, metric, source_ids) values ($1, $2, $3) \
         on conflict (profile_id, metric) do update set source_ids = excluded.source_ids",
    )
    .bind(profile_id)
    .bind(metric)
    .bind(source_ids)
    .execute(conn)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------- rollups ----

// The instant a sample counts at (`s.at`, with its offset `s.off`): sleep on waking, everything else
// at its start. A Watch writes a night as many short stages, so a sleep stage counts at the end of its
// session: stages less than SLEEP_GAP apart are one session (one night), dated by the local date of
// its last stage's end. Parameters: $1 profile, $2 metrics, $3 sources, $4 sleep metrics, $5 daily.
const SLEEP_GAP: &str = "2 hours";
const LOCAL_DAY: &str =
    "(date_trunc('day', (s.at at time zone 'UTC') + make_interval(mins => coalesce(s.off, 0))) at time zone 'UTC')";
const HOUR: &str = "(date_trunc('hour', s.at at time zone 'UTC') at time zone 'UTC')";

fn cols(a: &str) -> String {
    format!("{a}.profile_id, {a}.metric, {a}.t, {a}.t_end, {a}.v, {a}.tz_offset_min, {a}.source_id")
}

/// wearable_sample rows matching `sel` (on alias w), with `at` and `off` added.
fn samples(sel: &str) -> String {
    let part = |a: &str| format!("partition by {a}.profile_id, {a}.metric, {a}.source_id");
    format!(
        "(select {w}, w.t as at, w.tz_offset_min as off from wearable_sample w where {sel} \
         and not w.metric = any(cast($4 as text[])) \
         union all \
         select {z}, max(coalesce(z.t_end, z.t)) over ({pz}, z.sess), \
         first_value(z.tz_offset_min) over ({pz}, z.sess order by coalesce(z.t_end, z.t) desc) \
         from (select y.*, sum(y.brk) over ({py} order by y.t rows unbounded preceding) \
         as sess from (select {w}, case when w.t <= max(coalesce(w.t_end, w.t)) over \
         ({pw} order by w.t rows between unbounded preceding and 1 preceding) \
         + interval '{SLEEP_GAP}' then 0 else 1 end as brk from wearable_sample w where {sel} \
         and w.metric = any(cast($4 as text[]))) y) z) s",
        w = cols("w"),
        z = cols("z"),
        pz = part("z"),
        py = part("y"),
        pw = part("w"),
    )
}

const STATS: &str = "count(*), min(s.v), max(s.v), avg(s.v), sum(s.v), percentile_cont(0.1) within group (order by s.v), \
     percentile_cont(0.5) within group (order by s.v), percentile_cont(0.9) within group (order by s.v)";

/// Rebuild the hour and day rollups of one profile from wearable_sample (all metrics, or these).
/// Returns the number of rollup rows written. Whole metrics are rebuilt (per source when `source_ids`
/// is given), so a bucket is never computed from part of its samples.
pub async fn recompute_rollups(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metrics: Option<&[String]>,
    source_ids: Option<&[i16]>,
) -> Result<i64> {
    let sel = "w.profile_id = $1 and (cast($2 as text[]) is null or w.metric = any(cast($2 as text[]))) \
               and (cast($3 as smallint[]) is null or w.source_id = any(cast($3 as smallint[])))";
    let from = samples(sel);
    let sleep: Vec<String> = SLEEP_METRICS.iter().map(|s| s.to_string()).collect();
    let daily = daily_metrics();
    sqlx::query(
        "delete from wearable_rollup where profile_id = $1 \
         and (cast($2 as text[]) is null or metric = any(cast($2 as text[]))) \
         and (cast($3 as smallint[]) is null or source_id = any(cast($3 as smallint[])))",
    )
    .bind(profile_id)
    .bind(metrics)
    .bind(source_ids)
    .execute(&mut *conn)
    .await?;
    let insert = |bucket: &str, t: &str, coverage: String| {
        format!(
            "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, \
             v_p10, v_p50, v_p90, coverage) select s.profile_id, s.metric, s.source_id, '{bucket}', {t}, {STATS}, \
             case when s.metric = any(cast($5 as text[])) then 1.0 else {coverage} end \
             from {from} group by 1, 2, 3, 5"
        )
    };
    let hour_sql = insert(
        "hour",
        HOUR,
        "least(1.0, count(distinct floor(extract(minute from s.t at time zone 'UTC') / 5)) / 12.0)".into(),
    );
    let day_sql = insert("day", LOCAL_DAY, format!("least(1.0, count(distinct {HOUR}) / 24.0)"));
    let mut written = 0i64;
    for sql in [hour_sql, day_sql] {
        let done = sqlx::query(AssertSqlSafe(sql.as_str()))
            .bind(profile_id)
            .bind(metrics)
            .bind(source_ids)
            .bind(&sleep)
            .bind(&daily)
            .execute(&mut *conn)
            .await?;
        written += done.rows_affected() as i64;
    }
    Ok(written)
}

// ------------------------------------------------------------------- queries ----

/// One point of a series: the rollup of the best source for that bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub t: DateTime<Utc>,
    /// The sum or the mean, by metric.
    pub value: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub mean: Option<f64>,
    pub median: Option<f64>,
    pub p10: Option<f64>,
    pub p90: Option<f64>,
    pub n: i32,
    pub coverage: f64,
    pub source_id: i16,
    pub device: Option<String>,
}

type RollupRow = (
    DateTime<Utc>,
    i16,
    i32,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<f32>,
    f32,
    Option<String>,
);

/// A real as Python read it: the float of the text Postgres printed.
fn real(v: Option<f32>) -> Option<f64> {
    v.and_then(|x| f32_json(x).as_f64())
}

/// One point per bucket with data, taken from the best source that has that bucket. Buckets without
/// data are absent; callers fill gaps if they draw them.
pub async fn points(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metric_key: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    resolution: &str,
) -> Result<Vec<Point>> {
    let info = metric(metric_key);
    let rank: HashMap<i16, usize> = priority(&mut *conn, profile_id, metric_key)
        .await?
        .into_iter()
        .enumerate()
        .map(|(i, s)| (s, i))
        .collect();
    let rows: Vec<RollupRow> = sqlx::query_as(
        "select r.t, r.source_id, r.n, r.v_min, r.v_max, r.v_avg, r.v_sum, r.v_p10, r.v_p50, r.v_p90, r.coverage, \
         ws.device from wearable_rollup r join wearable_source ws on ws.id = r.source_id \
         where r.profile_id = $1 and r.metric = $2 and r.bucket = $3 and r.t >= $4 and r.t <= $5 order by r.t",
    )
    .bind(profile_id)
    .bind(metric_key)
    .bind(resolution)
    .bind(since)
    .bind(until)
    .fetch_all(conn)
    .await?;
    let rank_of = |s: i16| rank.get(&s).copied().unwrap_or(99);
    let mut best: BTreeMap<DateTime<Utc>, RollupRow> = BTreeMap::new();
    for r in rows {
        match best.get(&r.0) {
            Some(cur) if rank_of(r.1) >= rank_of(cur.1) => {}
            _ => {
                best.insert(r.0, r);
            }
        }
    }
    Ok(best
        .into_values()
        .map(|r| Point {
            t: r.0,
            value: if info.agg == "sum" { real(r.6) } else { real(r.5) },
            min: real(r.3),
            max: real(r.4),
            mean: real(r.5),
            median: real(r.8),
            p10: real(r.7),
            p90: real(r.9),
            n: r.2,
            coverage: real(Some(r.10)).unwrap_or(0.0),
            source_id: r.1,
            device: r.11,
        })
        .collect())
}

/// Python's `wearables.series` as JSON: `t`, `value`, `min`, `max`, `mean`, `median`, `p10`, `p90`,
/// `n`, `coverage`, `source_id`, `device` per bucket. `since` and `until` default to everything.
pub async fn series(
    conn: &mut PgConnection,
    profile_id: Uuid,
    metric_key: &str,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    resolution: &str,
) -> Result<Value> {
    let since = since.unwrap_or_else(|| day_start(NaiveDate::from_ymd_opt(1, 1, 1).unwrap_or_default()));
    let until = until.unwrap_or_else(|| day_start(NaiveDate::from_ymd_opt(9999, 12, 31).unwrap_or_default()));
    let pts = points(conn, profile_id, metric_key, since, until, resolution).await?;
    Ok(Value::Array(
        pts.into_iter()
            .map(|p| {
                json!({
                    "t": py_isoformat_utc(&p.t),
                    "value": p.value,
                    "min": p.min,
                    "max": p.max,
                    "mean": p.mean,
                    "median": p.median,
                    "p10": p.p10,
                    "p90": p.p90,
                    "n": p.n,
                    "coverage": p.coverage,
                    "source_id": p.source_id,
                    "device": p.device,
                })
            })
            .collect(),
    ))
}

pub async fn metrics_present(conn: &mut PgConnection, profile_id: Uuid) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "select distinct metric from wearable_rollup where profile_id = $1 and bucket = 'day' order by 1",
    )
    .bind(profile_id)
    .fetch_all(conn)
    .await?)
}

/// Every source of the profile with its newest sample and its mean daily coverage in the window.
pub async fn devices(
    conn: &mut PgConnection,
    profile_id: Uuid,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<Value>> {
    let days = ((until.date_naive() - since.date_naive()).num_days() + 1).max(1);
    type DeviceRow = (i16, String, Option<String>, Option<DateTime<Utc>>, f32);
    let rows: Vec<DeviceRow> = sqlx::query_as(
        "select ws.id, ws.provider, ws.device, \
         (select max(coalesce(t_end, t)) from wearable_sample where source_id = ws.id) as last_sample, \
         (select coalesce(sum(c), 0) from (select max(coverage) as c from wearable_rollup where source_id = ws.id \
           and bucket = 'day' and t >= $2 and t <= $3 group by t) d) as cov \
         from wearable_source ws where ws.profile_id = $1 order by ws.id",
    )
    .bind(profile_id)
    .bind(since)
    .bind(until)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, provider, device, last, cov)| {
            let cov = real(Some(cov)).unwrap_or(0.0);
            json!({
                "source_id": id,
                "provider": provider,
                "device": device,
                // sent by the annotated REST summary: pydantic writes UTC as `Z`
                "last_sample": last.map(|l| pydantic_datetime_utc(&l)),
                "coverage_pct": crate::py::round(100.0 * cov / days as f64, 1),
            })
        })
        .collect())
}

pub fn day_start(d: NaiveDate) -> DateTime<Utc> {
    d.and_hms_opt(0, 0, 0).map_or(DateTime::<Utc>::MIN_UTC, |n| n.and_utc())
}
