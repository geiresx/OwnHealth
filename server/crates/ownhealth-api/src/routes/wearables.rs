//! Lane C: `api/wearables.py`. Daily rollups with the source-priority rule applied, one source per
//! day: `/profiles/{p}/wearables/summary` and `/profiles/{p}/wearables/{metric}`.
//!
//! Needs `read:wearables`; every read writes one audit row.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Days, NaiveDate, TimeDelta, Utc};
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, PrincipalKind};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::pyjson::{py_date, pydantic_datetime_utc};
use ownhealth_ingest_structured::py::round as py_round;
use ownhealth_ingest_structured::wearables::{self as w, Point};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::error::ApiError;
use crate::extract::Caller;

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/wearables/summary", get(summary))
        .route("/profiles/{p}/wearables/{metric}", get(metric_series))
}

pub const ORDER: [&str; 6] = ["sleep_asleep", "hrv_rmssd", "hrv_sdnn", "hr_resting", "steps", "vo2max"];
const ALL_DAYS: u64 = 365 * 30;

enum Failure {
    Api(ApiError),
    Validation(Value),
}

impl<E: Into<ApiError>> From<E> for Failure {
    fn from(e: E) -> Failure {
        Failure::Api(e.into())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Failure::Api(e) => e.into_response(),
            Failure::Validation(v) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": [v]}))).into_response(),
        }
    }
}

type Answer = Result<Json<Value>, Failure>;

fn invalid(kind: &str, key: &str, msg: &str, input: &str) -> Failure {
    invalid_ctx(kind, key, msg, input, None)
}

/// A FastAPI 422 entry; constraint errors carry pydantic's `ctx`.
fn invalid_ctx(kind: &str, key: &str, msg: &str, input: &str, ctx: Option<Value>) -> Failure {
    Failure::Validation(super::auth::error_entry(
        kind,
        vec![json!("query"), json!(key)],
        msg,
        json!(input),
        ctx,
    ))
}

fn bad_input(message: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bad_input", message)
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn literal<'a>(q: &'a [(String, String)], key: &str, allowed: &[&'a str], msg: &str) -> Result<&'a str, Failure> {
    match param(q, key) {
        None => Ok(allowed[0]),
        Some(v) if allowed.contains(&v) => Ok(v),
        Some(v) => {
            let expected = msg.trim_start_matches("Input should be ");
            Err(invalid_ctx(
                "literal_error",
                key,
                msg,
                v,
                Some(json!({"expected": expected})),
            ))
        }
    }
}

fn date_param(q: &[(String, String)], key: &str) -> Result<Option<NaiveDate>, Failure> {
    match param(q, key) {
        None => Ok(None),
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map(Some).map_err(|_| {
            invalid(
                "date_from_datetime_parsing",
                key,
                "Input should be a valid date or datetime, invalid character in year",
                s,
            )
        }),
    }
}

/// `days: int = Query(90, ge=7, le=366)`.
fn days_param(q: &[(String, String)]) -> Result<u64, Failure> {
    let Some(s) = param(q, "days") else { return Ok(90) };
    let n: i64 = s.trim().parse().map_err(|_| {
        invalid(
            "int_parsing",
            "days",
            "Input should be a valid integer, unable to parse string as an integer",
            s,
        )
    })?;
    if n < 7 {
        return Err(invalid_ctx(
            "greater_than_equal",
            "days",
            "Input should be greater than or equal to 7",
            s,
            Some(json!({"ge": 7})),
        ));
    }
    if n > 366 {
        return Err(invalid_ctx(
            "less_than_equal",
            "days",
            "Input should be less than or equal to 366",
            s,
            Some(json!({"le": 366})),
        ));
    }
    Ok(n.unsigned_abs())
}

/// `P<n>D`, `P<n>M`, `P<n>Y` (n of 1 to 4 digits) or `all`, capped at `all` (30 years).
pub fn window_days(window: &str) -> Result<u64, ApiError> {
    if window == "all" {
        return Ok(ALL_DAYS);
    }
    let bad = || bad_input("window is P<n>D, P<n>M, P<n>Y or all.");
    let win = window.strip_suffix('\n').unwrap_or(window); // Python's `$` also matches before a final newline
    let body = win.strip_prefix('P').ok_or_else(bad)?;
    let (digits, unit) = body.split_at(body.len().saturating_sub(1));
    let per = match unit {
        "D" => 1,
        "M" => 30,
        "Y" => 365,
        _ => return Err(bad()),
    };
    if !(1..=4).contains(&digits.len()) || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = digits.parse().map_err(|_| bad())?;
    Ok((n * per).min(ALL_DAYS))
}

fn via(p: &Principal) -> &'static str {
    if p.kind == PrincipalKind::Token { "api" } else { "ui" }
}

async fn audit(
    conn: &mut PgConnection,
    principal: &Principal,
    perm: &Permission,
    tool: &str,
    metrics: Vec<String>,
) -> Result<(), ApiError> {
    let mut ev = AuditEvent::new(principal.actor_kind(), via(principal), "read");
    ev.actor_id = principal.user_id;
    ev.token_id = principal.token_id;
    ev.tool = Some(tool.into());
    ev.profile_id = Some(perm.profile.id);
    ev.resource_type = Some("wearable_rollup".into());
    ev.resource_ids = Some(metrics);
    write_audit(conn, &ev).await?;
    Ok(())
}

/// `_r`: two decimals for VO2max, weight and distance, one for the rest.
fn r(v: Option<f64>, metric: &str) -> Value {
    match v {
        None => Value::Null,
        Some(v) => json!(py_round(
            v,
            if matches!(metric, "vo2max" | "weight" | "distance") {
                2
            } else {
                1
            }
        )),
    }
}

fn label(info: &w::Metric, lang: &str) -> String {
    if lang == "es" {
        info.label_es.clone()
    } else {
        info.label_en.clone()
    }
}

fn order_key(m: &str) -> (usize, String) {
    (ORDER.iter().position(|o| *o == m).unwrap_or(ORDER.len()), m.to_string())
}

fn sub_days(d: NaiveDate, n: u64) -> Option<NaiveDate> {
    let year_one = NaiveDate::from_ymd_opt(1, 1, 1).unwrap_or(NaiveDate::MIN);
    d.checked_sub_days(Days::new(n)).filter(|x| *x >= year_one)
}

/// Every metric of the profile: a daily series over `days`, the newest value and the 30-day mean.
async fn summary(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> Answer {
    let lang = literal(&q, "lang", &["es", "en"], "Input should be 'es' or 'en'")?;
    let days = days_param(&q)?;
    let as_of = date_param(&q, "as_of")?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:wearables"), None).await?;
    let end = as_of.unwrap_or_else(|| Utc::now().date_naive());
    let start = sub_days(end, days - 1).ok_or_else(|| bad_input("The window reaches before year 1."))?;
    let (since, until) = (w::day_start(start), w::day_start(end));
    let mut present = w::metrics_present(&mut tx, perm.profile.id).await?;
    present.sort_by_key(|m| order_key(m));
    let mut out: Vec<Value> = Vec::new();
    for metric in &present {
        let info = w::metric(metric);
        let pts: Vec<Point> = w::points(&mut tx, perm.profile.id, metric, since, until, "day").await?;
        // dict keyed by date: a later point of the same day replaces the earlier one in place
        let mut points: BTreeMap<NaiveDate, Point> = BTreeMap::new();
        let mut order: Vec<NaiveDate> = Vec::new();
        for pt in pts {
            let d = pt.t.date_naive();
            if points.insert(d, pt).is_none() {
                order.push(d);
            }
        }
        let Some(newest_date) = points.keys().max().copied() else {
            continue;
        };
        let mut series = Vec::with_capacity(usize::try_from(days).unwrap_or_default());
        for i in 0..days {
            let d = start.checked_add_days(Days::new(i)).unwrap_or(start);
            let pt = points.get(&d);
            series.push(json!({
                "date": py_date(&d),
                "value": pt.map_or(Value::Null, |p| r(p.value, metric)),
                "min": pt.map_or(Value::Null, |p| r(p.min, metric)),
                "max": pt.map_or(Value::Null, |p| r(p.max, metric)),
            }));
        }
        let cutoff = sub_days(end, 30);
        let last30: Vec<f64> = order
            .iter()
            .filter(|d| cutoff.is_none_or(|c| **d > c))
            .filter_map(|d| points.get(d).and_then(|p| p.value))
            .collect();
        let newest = points.get(&newest_date);
        let coverage: f64 = order.iter().filter_map(|d| points.get(d)).map(|p| p.coverage).sum();
        out.push(json!({
            "metric": metric,
            "label": label(&info, lang),
            "unit": info.unit,
            "agg": info.agg,
            "latest": r(newest.and_then(|p| p.value), metric),
            "latest_date": py_date(&newest_date),
            "avg_30d": if last30.is_empty() { Value::Null } else { r(Some(last30.iter().sum::<f64>() / last30.len() as f64), metric) },
            "series": series,
            "coverage_pct": py_round(100.0 * coverage / days as f64, 1),
            "source": newest.and_then(|p| p.device.clone()),
        }));
    }
    let shown: Vec<String> = out
        .iter()
        .filter_map(|m| m["metric"].as_str().map(String::from))
        .collect();
    audit(&mut tx, &caller.0, &perm, "wearables.summary", shown).await?;
    let devices = w::devices(&mut tx, perm.profile.id, since, until).await?;
    tx.commit().await?;
    Ok(Json(json!({"metrics": out, "devices": devices})))
}

/// One metric: every bucket with data from the best source for it (hour buckets up to 14 days).
async fn metric_series(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, metric)): Path<(String, String)>,
    Query(q): Query<Vec<(String, String)>>,
) -> Answer {
    let window = param(&q, "window").unwrap_or("P30D");
    let resolution = literal(&q, "resolution", &["day", "hour"], "Input should be 'day' or 'hour'")?;
    let as_of = date_param(&q, "as_of")?;
    let lang = literal(&q, "lang", &["es", "en"], "Input should be 'es' or 'en'")?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:wearables"), None).await?;
    let days = window_days(window)?;
    if resolution == "hour" && days > 14 {
        return Err(bad_input("Hourly series go up to P14D.").into());
    }
    let end = as_of.unwrap_or_else(|| Utc::now().date_naive());
    // as_of so early the window reaches before year 1
    let start = sub_days(end, days - 1).ok_or_else(|| bad_input("The window reaches before year 1."))?;
    let since = w::day_start(start);
    let until = w::day_start(end) + TimeDelta::days(1) - TimeDelta::seconds(1);
    let info = w::metric(&metric);
    let points = w::points(&mut tx, perm.profile.id, &metric, since, until, resolution).await?;
    audit(&mut tx, &caller.0, &perm, "wearables.series", vec![metric.clone()]).await?;
    let priority = w::priority(&mut tx, perm.profile.id, &metric).await?;
    tx.commit().await?;
    let pts: Vec<Value> = points
        .iter()
        .map(|pt| {
            json!({
                "t": pydantic_datetime_utc(&pt.t),
                "value": r(pt.value, &metric),
                "min": r(pt.min, &metric),
                "max": r(pt.max, &metric),
                "mean": r(pt.mean, &metric),
                "median": r(pt.median, &metric),
                "p10": r(pt.p10, &metric),
                "p90": r(pt.p90, &metric),
                "n": pt.n,
                "coverage": py_round(pt.coverage, 3),
                "source_id": pt.source_id,
                "device": pt.device,
            })
        })
        .collect();
    Ok(Json(json!({
        "metric": metric,
        "label": label(&info, lang),
        "unit": info.unit,
        "agg": info.agg,
        "resolution": resolution,
        "window": window,
        "priority": priority,
        "points": pts,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        assert_eq!(window_days("P7D").ok(), Some(7));
        assert_eq!(window_days("P2M").ok(), Some(60));
        assert_eq!(window_days("P9999Y").ok(), Some(ALL_DAYS)); // no longer than "all"
        assert_eq!(window_days("all").ok(), Some(ALL_DAYS));
        assert!(window_days("P10000D").is_err() && window_days("30D").is_err() && window_days("P1W").is_err());
    }
}
