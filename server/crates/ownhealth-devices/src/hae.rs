//! Health Auto Export receiver (lane D): the iOS app's REST automation posts JSON v2
//! (`{"data": {"metrics": [{name, units, data: [{date, qty, source, ...}]}], "workouts": [...]}}`) on a
//! schedule, typically every 15 minutes with overlapping date ranges. Each push is stored as a blob
//! first (connectors.md 6.3 rule 1, `source_item` `push:<sha256>`), mapped to wearable metrics, and
//! written idempotently: the `wearable_sample` key is (profile, metric, inbox, time), and a bucket the
//! app re-sends with a larger value (the hour that was still open at the last push) is refreshed in
//! place, so overlapping pushes add 0 duplicate samples.
//!
//! Dates are `yyyy-MM-dd HH:mm:ss Z`; the offset is kept. Metrics we do not chart yet (workouts,
//! nutrition, symptoms, ...) are skipped and logged by name only. Points of a metric we do chart that
//! cannot be read (a unit we do not know, a point without a value or date) are reported as run problems,
//! so the run reads `partial` in the Runs panel.
//!
//! Each inbox writes through one `wearable_source` (external id = the inbox id), whatever the sample's
//! `source` string says. The app merges sources into `"iPhone|Watch"` for summarized buckets, and the
//! same bucket can arrive as `"iPhone"` at one push (the phone wrote the hour first) and as
//! `"iPhone|Watch"` at the next (the watch synced later); a key that held the device would store that
//! bucket twice. `source` is still parsed (`Point::device`) but does not key the sample.
//!
//! Day-granularity metrics (resting heart rate) are keyed on the local calendar date at 00:00 UTC, not
//! on the instant: the app writes a day bucket as local midnight in the phone's current time zone, so
//! the same day re-sent after the phone changed offset (travel) would otherwise be a second sample.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source};
use ownhealth_ingest_structured::apple_health::parse::parse_time;
use serde_json::Value;
use sqlx::PgConnection;

use crate::store::{self, Conflict, Sample};

/// The largest push body we accept.
pub const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;
pub const PROVIDER: &str = "health_auto_export";

/// HAE metric name -> (our metric, the field holding the value, [(unit as sent, factor)]). An empty
/// unit list takes any unit.
type Spec = (&'static str, &'static str, &'static [(&'static str, f64)]);

const SIMPLE: [(&str, Spec); 9] = [
    ("step_count", ("steps", "qty", &[])),
    ("heart_rate", ("hr", "Avg", &[])),
    ("resting_heart_rate", ("hr_resting", "qty", &[])),
    ("heart_rate_variability", ("hrv_sdnn", "qty", &[("ms", 1.0)])),
    ("vo2_max", ("vo2max", "qty", &[])),
    ("respiratory_rate", ("resp_rate", "qty", &[])),
    (
        "weight_body_mass",
        (
            "weight",
            "qty",
            &[("kg", 1.0), ("lb", 0.45359237), ("lbs", 0.45359237), ("g", 0.001)],
        ),
    ),
    (
        "active_energy",
        (
            "active_energy",
            "qty",
            &[("kcal", 1.0), ("Cal", 1.0), ("kJ", 1.0 / 4.184)],
        ),
    ),
    (
        "walking_running_distance",
        ("distance", "qty", &[("km", 1.0), ("mi", 1.609344), ("m", 0.001)]),
    ),
];

/// Metrics the app sends one bucket per local day for.
const DAY_METRICS: [&str; 1] = ["hr_resting"];

/// The key time of a day bucket: its local calendar date (`t` at offset `tz` minutes) at 00:00 UTC.
fn day_key(t: DateTime<Utc>, tz: i16) -> DateTime<Utc> {
    let local = (t + Duration::minutes(i64::from(tz))).date_naive();
    local.and_time(chrono::NaiveTime::MIN).and_utc()
}

/// One mapped point of a push.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub device: Option<String>,
    pub metric: &'static str,
    pub t: DateTime<Utc>,
    pub t_end: Option<DateTime<Utc>>,
    pub v: f64,
    pub tz: i16,
}

/// What a push held: mapped points, and what was skipped by reason (`metric:<name>`, `unit:<name>`,
/// `bad_point:<name>`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub points: Vec<Point>,
    pub skipped: BTreeMap<String, i64>,
}

/// The device named by one HAE `source` string: the Watch when any part names a watch, else the iPhone,
/// else the first part. Informational only: it does not key the sample (see the module docs).
pub fn device_of(source: Option<&str>) -> Option<String> {
    let parts: Vec<&str> = source?.split('|').map(str::trim).filter(|p| !p.is_empty()).collect();
    let has = |needle: &str| parts.iter().find(|p| p.to_lowercase().contains(needle)).copied();
    has("watch")
        .or_else(|| has("iphone"))
        .or_else(|| parts.first().copied())
        .map(String::from)
}

fn num(item: &Value, key: &str) -> Option<f64> {
    item.get(key).and_then(Value::as_f64).filter(|v| v.is_finite())
}

fn time(item: &Value, key: &str) -> Option<(DateTime<Utc>, i16)> {
    item.get(key).and_then(Value::as_str).and_then(parse_time)
}

fn bump(skipped: &mut BTreeMap<String, i64>, key: String) {
    *skipped.entry(key).or_default() += 1;
}

/// Sleep, summarized (one row per night: totals in hours with start and end) or per segment.
fn sleep(item: &Value, device: Option<String>, out: &mut Vec<Point>) -> bool {
    let mut push = |metric, (t, tz): (DateTime<Utc>, i16), end: Option<DateTime<Utc>>, hours: f64| {
        if hours > 0.0 && hours < 24.0 {
            out.push(Point {
                device: device.clone(),
                metric,
                t,
                t_end: end,
                v: hours * 60.0,
                tz,
            });
        }
    };
    if let Some(start) = time(item, "sleepStart") {
        let end = time(item, "sleepEnd").map(|e| e.0);
        let total = num(item, "totalSleep").or_else(|| num(item, "asleep")).unwrap_or(0.0);
        push("sleep_asleep", start, end, total);
        if let (Some(bed), Some(start)) = (num(item, "inBed"), time(item, "inBedStart")) {
            push("sleep_in_bed", start, time(item, "inBedEnd").map(|e| e.0), bed);
        }
        return true;
    }
    let stage = item.get("value").and_then(Value::as_str).unwrap_or_default();
    let metric = match stage {
        "In Bed" => "sleep_in_bed",
        "Asleep" | "Core" | "Deep" | "REM" | "Unspecified" => "sleep_asleep",
        "Awake" => return true,
        _ => return false,
    };
    let Some(start) = time(item, "startDate")
        .or_else(|| time(item, "start"))
        .or_else(|| time(item, "date"))
    else {
        return false;
    };
    let end = time(item, "endDate").or_else(|| time(item, "end")).map(|e| e.0);
    let hours = match end {
        Some(e) if e > start.0 => (e - start.0).num_seconds() as f64 / 3600.0,
        _ => num(item, "qty").unwrap_or(0.0),
    };
    push(metric, start, end, hours);
    true
}

/// Map one push body. Pure; the caller runs it off the async runtime.
pub fn parse(body: &[u8]) -> Result<Parsed> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|_| Error::Invalid("not Health Auto Export JSON: the body does not parse".into()))?;
    let Some(data) = v.get("data").filter(|d| d.is_object()) else {
        return Err(Error::Invalid("not Health Auto Export JSON: no \"data\" object".into()));
    };
    let mut out = Parsed::default();
    for (key, value) in data.as_object().into_iter().flatten() {
        if key != "metrics" {
            let n = value.as_array().map_or(0, Vec::len) as i64;
            if n > 0 {
                *out.skipped.entry(format!("{key}:all")).or_default() += n;
            }
        }
    }
    for m in data.get("metrics").and_then(Value::as_array).into_iter().flatten() {
        let name = m.get("name").and_then(Value::as_str).unwrap_or_default();
        let units = m.get("units").and_then(Value::as_str).unwrap_or_default();
        let items = m.get("data").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        for item in items {
            let device = device_of(item.get("source").and_then(Value::as_str));
            if name == "sleep_analysis" {
                if !sleep(item, device, &mut out.points) {
                    bump(&mut out.skipped, format!("bad_point:{name}"));
                }
                continue;
            }
            let Some((t, tz)) = time(item, "date") else {
                bump(&mut out.skipped, format!("bad_point:{name}"));
                continue;
            };
            let t_end = time(item, "end").map(|e| e.0).filter(|e| *e > t);
            let mut point = |metric: &'static str, v: f64| {
                let (t, t_end) = if DAY_METRICS.contains(&metric) {
                    (day_key(t, tz), None)
                } else {
                    (t, t_end)
                };
                out.points.push(Point {
                    device: device.clone(),
                    metric,
                    t,
                    t_end,
                    v,
                    tz,
                })
            };
            if name == "blood_pressure" {
                match (num(item, "systolic"), num(item, "diastolic")) {
                    (Some(s), Some(d)) => {
                        point("bp_systolic", s);
                        point("bp_diastolic", d);
                    }
                    _ => bump(&mut out.skipped, format!("bad_point:{name}")),
                }
                continue;
            }
            if name == "blood_oxygen_saturation" {
                match num(item, "qty") {
                    // Apple stores a fraction; the app sends percent, older versions the fraction
                    Some(q) => point("spo2", if q <= 1.0 { q * 100.0 } else { q }),
                    None => bump(&mut out.skipped, format!("bad_point:{name}")),
                }
                continue;
            }
            let Some((_, (metric, field, unit_table))) = SIMPLE.iter().find(|s| s.0 == name) else {
                bump(&mut out.skipped, format!("metric:{name}"));
                continue;
            };
            let factor = if unit_table.is_empty() {
                Some(1.0)
            } else {
                unit_table.iter().find(|u| u.0 == units).map(|u| u.1)
            };
            let Some(factor) = factor else {
                bump(&mut out.skipped, format!("unit:{name}"));
                continue;
            };
            match num(item, field).or_else(|| num(item, "qty")) {
                Some(q) => point(metric, q * factor),
                None => bump(&mut out.skipped, format!("bad_point:{name}")),
            }
        }
    }
    Ok(out)
}

/// One push from the source's device: store, map, insert. Returns what the run did (the route records
/// it with `core::sources::{begin_run, finish_run}`).
pub async fn import_push(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    source: &Source,
    body: &[u8],
) -> Result<RunOutcome> {
    if source.kind != "health_auto_export" {
        return Err(Error::Invalid("this source is not a Health Auto Export inbox".into()));
    }
    let profile_id = source
        .profile_id
        .ok_or_else(|| Error::Invalid("this Health Auto Export inbox has no profile: pick one first".into()))?;
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::Invalid("the push is larger than 50 MB".into()));
    }
    let (sha, _) = ownhealth_core::arrive::store_bytes(blobs, body.to_vec()).await?;
    ownhealth_core::dedup::record_item(&mut *conn, source.id, &format!("push:{sha}"), Some(&sha), None, None).await?;
    let raw = body.to_vec();
    let parsed = tokio::task::spawn_blocking(move || parse(&raw))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("push parse task failed: {e}")))??;
    // unknown units and unreadable points of metrics we chart lose data: report them; metrics and
    // sections we do not chart yet are expected on every push and only logged
    let mut problems = Vec::new();
    let mut not_charted = Vec::new();
    for (key, count) in &parsed.skipped {
        if key.starts_with("unit:") || key.starts_with("bad_point:") {
            problems.push(RunProblem {
                code: "points_skipped".into(),
                message: format!("{key} x{count}"),
            });
        } else {
            not_charted.push(key.as_str());
        }
    }
    if !not_charted.is_empty() {
        tracing::debug!(source = %source.id, skipped = ?not_charted, "health auto export: metrics not imported");
    }
    let wsid = store::device_source(
        &mut *conn,
        profile_id,
        PROVIDER,
        None,
        Some(&source.id.to_string()),
        Some(source.id),
    )
    .await?;
    let samples: Vec<Sample> = parsed
        .points
        .into_iter()
        .map(|p| Sample {
            metric: p.metric,
            t: p.t,
            t_end: p.t_end,
            v: p.v,
            tz: Some(p.tz),
            source: wsid,
        })
        .collect();
    let w = store::write(&mut *conn, profile_id, None, samples, Conflict::Refresh).await?;
    let n = |x: i64| i32::try_from(x).unwrap_or(i32::MAX);
    Ok(RunOutcome {
        seen: n(w.seen),
        new: n(w.new),
        duplicate: n(w.duplicate()),
        problems,
        ..RunOutcome::default()
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn device_prefers_the_watch_then_the_iphone() {
        assert_eq!(
            device_of(Some("iPhone de Alex|Apple Watch de Alex")).as_deref(),
            Some("Apple Watch de Alex")
        );
        assert_eq!(
            device_of(Some("Apple Watch de Alex")).as_deref(),
            Some("Apple Watch de Alex")
        );
        assert_eq!(device_of(Some("LabNorte Scale|iPhone")).as_deref(), Some("iPhone"));
        assert_eq!(device_of(Some("LabNorte Scale")).as_deref(), Some("LabNorte Scale"));
        assert_eq!(device_of(Some(" | ")), None);
        assert_eq!(device_of(None), None);
    }

    #[test]
    fn metrics_units_and_special_shapes() -> Result<()> {
        let body = json!({"data": {"metrics": [
            {"name": "step_count", "units": "count", "data": [{"date": "2026-11-02 10:00:00 +0100", "qty": 321, "source": "Watch"}]},
            {"name": "heart_rate", "units": "count/min", "data": [{"date": "2026-11-02 10:00:00 +0100", "Min": 60, "Avg": 71.5, "Max": 90}]},
            {"name": "weight_body_mass", "units": "lb", "data": [{"date": "2026-11-02 07:00:00 +0100", "qty": 150}]},
            {"name": "weight_body_mass", "units": "stone", "data": [{"date": "2026-11-02 07:00:00 +0100", "qty": 11}]},
            {"name": "active_energy", "units": "kJ", "data": [{"date": "2026-11-02 10:00:00 +0100", "qty": 418.4}]},
            {"name": "blood_pressure", "units": "mmHg", "data": [{"date": "2026-11-02 08:00:00 +0100", "systolic": 118, "diastolic": 76}]},
            {"name": "blood_oxygen_saturation", "units": "%", "data": [{"date": "2026-11-02 03:00:00 +0100", "qty": 0.97}, {"date": "2026-11-02 04:00:00 +0100", "qty": 96}]},
            {"name": "sleep_analysis", "units": "hr", "data": [{"date": "2026-11-02 00:00:00 +0100", "totalSleep": 7.5, "asleep": 0.5,
                "sleepStart": "2026-11-01 23:00:00 +0100", "sleepEnd": "2026-11-02 06:30:00 +0100", "inBed": 8.0,
                "inBedStart": "2026-11-01 22:45:00 +0100", "inBedEnd": "2026-11-02 06:45:00 +0100", "source": "Watch"}]},
            {"name": "sleep_analysis", "units": "hr", "data": [
                {"startDate": "2026-11-02 23:00:00 +0100", "endDate": "2026-11-02 23:30:00 +0100", "qty": 0.5, "value": "Core"},
                {"startDate": "2026-11-02 23:30:00 +0100", "endDate": "2026-11-02 23:40:00 +0100", "qty": 0.17, "value": "Awake"}]},
            {"name": "dietary_energy", "units": "kcal", "data": [{"date": "2026-11-02 08:00:00 +0100", "qty": 240, "foodType": "x"}]},
            {"name": "step_count", "units": "count", "data": [{"date": "yesterday", "qty": 1}]}
        ], "workouts": [{"name": "Run"}]}});
        let p = parse(&serde_json::to_vec(&body)?)?;
        let get = |m: &str| {
            p.points
                .iter()
                .filter(|x| x.metric == m)
                .map(|x| x.v)
                .collect::<Vec<_>>()
        };
        assert_eq!(get("steps"), vec![321.0]);
        assert_eq!(get("hr"), vec![71.5]);
        assert!((get("weight")[0] - 68.0388555).abs() < 1e-6);
        assert!((get("active_energy")[0] - 100.0).abs() < 1e-9);
        assert_eq!((get("bp_systolic"), get("bp_diastolic")), (vec![118.0], vec![76.0]));
        assert_eq!(get("spo2"), vec![97.0, 96.0]);
        assert_eq!(get("sleep_asleep"), vec![450.0, 30.0]);
        assert_eq!(get("sleep_in_bed"), vec![480.0]);
        let steps = p
            .points
            .iter()
            .find(|x| x.metric == "steps")
            .map(|x| (x.t.to_rfc3339(), x.tz, x.device.clone()));
        assert_eq!(
            steps,
            Some(("2026-11-02T09:00:00+00:00".into(), 60, Some("Watch".into())))
        );
        assert_eq!(p.skipped.get("unit:weight_body_mass"), Some(&1));
        assert_eq!(p.skipped.get("metric:dietary_energy"), Some(&1));
        assert_eq!(p.skipped.get("bad_point:step_count"), Some(&1));
        assert_eq!(p.skipped.get("workouts:all"), Some(&1));
        Ok(())
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(matches!(parse(b"not json"), Err(Error::Invalid(_))));
        assert!(matches!(parse(br#"{"metrics": []}"#), Err(Error::Invalid(_))));
        assert!(parse(br#"{"data": {}}"#).is_ok_and(|p| p.points.is_empty()));
    }
}
