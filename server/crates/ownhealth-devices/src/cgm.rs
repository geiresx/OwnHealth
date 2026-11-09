//! CGM (lane D): readings and the HL7 CGM IG summary. Interstitial glucose readings are LOINC 99504-3;
//! the summary follows the HL7 CGM IG profiles: mean glucose (97507-8), GMI, and the times-in-ranges
//! panel (106793-3: very low < 54, low 54-69, in range 70-180, high 181-250, very high > 250 mg/dL).
//! A summary is valid only with >= 70% sensor wear over 14 days (the international time-in-range
//! consensus); otherwise it says why it is not valid instead of a number. Information, not diagnosis.
//!
//! Percentages are shares of the readings, as the consensus and Nightscout's distribution report count
//! them. Readings less than a minute apart count once (Nightscout's `uniqSgv`), so two uploaders
//! writing the same reading, or two sources of one sensor, do not count twice.

use chrono::{DateTime, Duration, Utc};
use ownhealth_core::error::{Error, Result};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

/// The `wearable_sample.metric` of a CGM reading (mg/dL).
pub const METRIC: &str = "glucose_cgm";
pub const LOINC_READING: &str = "99504-3";
pub const LOINC_MEAN_GLUCOSE: &str = "97507-8";
pub const LOINC_TIR_PANEL: &str = "106793-3";
pub const VALID_WEAR_FRACTION: f64 = 0.70;
pub const DEFAULT_DAYS: i64 = 14;
/// The reading interval when the data cannot tell (most sensors write every 5 minutes).
pub const DEFAULT_INTERVAL_MINUTES: f64 = 5.0;

/// One summary over `[start, end)`. Percentages are 0..100 of the readings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CgmSummary {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub readings: i64,
    pub wear_fraction: f64,
    pub valid: bool,
    /// Why not valid (`no_readings`, `short_window`, `insufficient_wear`), when not.
    pub invalid_reason: Option<String>,
    pub mean_mg_dl: Option<f64>,
    pub gmi_percent: Option<f64>,
    pub very_low_pct: Option<f64>,
    pub low_pct: Option<f64>,
    pub in_range_pct: Option<f64>,
    pub high_pct: Option<f64>,
    pub very_high_pct: Option<f64>,
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Sorted readings with repeats less than a minute after the last kept one dropped.
fn uniq(readings: &[(DateTime<Utc>, f64)], start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<(DateTime<Utc>, f64)> {
    let mut r: Vec<(DateTime<Utc>, f64)> = readings
        .iter()
        .copied()
        .filter(|(t, v)| *t >= start && *t < end && v.is_finite() && *v > 0.0)
        .collect();
    r.sort_by_key(|x| x.0);
    let mut out: Vec<(DateTime<Utc>, f64)> = Vec::with_capacity(r.len());
    for x in r {
        if out.last().is_none_or(|last| x.0 - last.0 >= Duration::minutes(1)) {
            out.push(x);
        }
    }
    out
}

/// GMI (%) from the mean glucose in mg/dL (Bergenstal et al., Diabetes Care 2018).
pub fn gmi(mean_mg_dl: f64) -> f64 {
    3.31 + 0.02392 * mean_mg_dl
}

/// The summary of `readings` (time, mg/dL) over `[start, end)` with the sensor's reading interval
/// (5 min for most sensors, 15 min for some Libre setups). Pure.
pub fn summarize(
    readings: &[(DateTime<Utc>, f64)],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    interval_minutes: f64,
) -> Result<CgmSummary> {
    if end <= start {
        return Err(Error::Invalid("the summary window ends before it starts".into()));
    }
    if !(interval_minutes.is_finite() && interval_minutes >= 1.0) {
        return Err(Error::Invalid(
            "the reading interval must be at least one minute".into(),
        ));
    }
    let r = uniq(readings, start, end);
    let window_min = (end - start).num_seconds() as f64 / 60.0;
    let slots = (window_min / interval_minutes).ceil().max(1.0);
    let mut covered: Vec<i64> = r
        .iter()
        .map(|(t, _)| ((*t - start).num_seconds() as f64 / 60.0 / interval_minutes).floor() as i64)
        .collect();
    covered.dedup();
    let wear = (covered.len() as f64 / slots).min(1.0);
    let mut s = CgmSummary {
        start,
        end,
        readings: r.len() as i64,
        wear_fraction: (wear * 1000.0).round() / 1000.0,
        valid: false,
        invalid_reason: None,
        mean_mg_dl: None,
        gmi_percent: None,
        very_low_pct: None,
        low_pct: None,
        in_range_pct: None,
        high_pct: None,
        very_high_pct: None,
    };
    let reason = if r.is_empty() {
        Some("no_readings")
    } else if end - start < Duration::days(DEFAULT_DAYS) {
        Some("short_window")
    } else if wear < VALID_WEAR_FRACTION {
        Some("insufficient_wear")
    } else {
        None
    };
    if let Some(reason) = reason {
        s.invalid_reason = Some(reason.into());
        return Ok(s);
    }
    let n = r.len() as f64;
    let pct = |f: &dyn Fn(f64) -> bool| Some(round1(100.0 * r.iter().filter(|x| f(x.1)).count() as f64 / n));
    let mean = r.iter().map(|x| x.1).sum::<f64>() / n;
    s.valid = true;
    s.mean_mg_dl = Some(round1(mean));
    s.gmi_percent = Some(round1(gmi(mean)));
    s.very_low_pct = pct(&|v| v < 54.0);
    s.low_pct = pct(&|v| (54.0..70.0).contains(&v));
    s.in_range_pct = pct(&|v| (70.0..=180.0).contains(&v));
    s.high_pct = pct(&|v| v > 180.0 && v <= 250.0);
    s.very_high_pct = pct(&|v| v > 250.0);
    Ok(s)
}

/// The reading intervals sensors write at, in minutes: Libre 2/3 read through xDrip or LibreLinkUp's
/// live feed (1), Dexcom, Medtronic and most others (5), LibreLinkUp's history (15).
pub const SENSOR_INTERVALS: [f64; 3] = [1.0, 5.0, 15.0];

/// The reading interval the data shows: the median gap between readings, snapped down to the nearest
/// interval a sensor writes at (`SENSOR_INTERVALS`). Wear is measured against this interval, so it must
/// not come from the gaps alone: a 5-minute sensor that loses every other reading has 10-minute gaps,
/// and an interval of 10 would read as full wear. Evenly sparse data at a real sensor interval (a
/// 5-minute sensor keeping one reading in three looks like a 15-minute one) cannot be told apart.
pub fn infer_interval(readings: &[(DateTime<Utc>, f64)]) -> f64 {
    let mut ts: Vec<DateTime<Utc>> = readings.iter().map(|r| r.0).collect();
    ts.sort();
    let mut gaps: Vec<i64> = ts
        .windows(2)
        .map(|w| (w[1] - w[0]).num_seconds())
        .filter(|g| *g >= 60)
        .collect();
    if gaps.is_empty() {
        return DEFAULT_INTERVAL_MINUTES;
    }
    gaps.sort_unstable();
    let median = (gaps[gaps.len() / 2] as f64 / 60.0).round();
    SENSOR_INTERVALS
        .iter()
        .copied()
        .rfind(|i| *i <= median)
        .unwrap_or(SENSOR_INTERVALS[0])
}

/// The summary of a profile's stored readings over the last `days` days before `end`.
pub async fn cgm_summary(
    conn: &mut PgConnection,
    profile_id: Uuid,
    end: DateTime<Utc>,
    days: i64,
) -> Result<CgmSummary> {
    if !(1..=90).contains(&days) {
        return Err(Error::Invalid("days must be between 1 and 90".into()));
    }
    let start = end - Duration::days(days);
    let rows: Vec<(DateTime<Utc>, f32)> = sqlx::query_as(
        "select t, v from wearable_sample where profile_id = $1 and metric = $2 and t >= $3 and t < $4 order by t",
    )
    .bind(profile_id)
    .bind(METRIC)
    .bind(start)
    .bind(end)
    .fetch_all(conn)
    .await?;
    let readings: Vec<(DateTime<Utc>, f64)> = rows.into_iter().map(|(t, v)| (t, f64::from(v))).collect();
    let interval = infer_interval(&uniq(&readings, start, end));
    summarize(&readings, start, end, interval)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(min: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).single().unwrap_or_default() + Duration::minutes(min)
    }

    fn series(days: i64, every: i64, f: impl Fn(i64) -> f64) -> Vec<(DateTime<Utc>, f64)> {
        (0..days * 1440 / every).map(|i| (at(i * every), f(i))).collect()
    }

    #[test]
    fn ranges_follow_the_consensus_boundaries() -> Result<()> {
        // 10 readings per value, one value per band edge
        let values = [53.0, 54.0, 69.0, 70.0, 180.0, 181.0, 250.0, 251.0];
        let r = series(14, 5, |i| values[(i as usize) % values.len()]);
        let s = summarize(&r, at(0), at(14 * 1440), 5.0)?;
        assert!(s.valid, "{s:?}");
        assert_eq!(s.very_low_pct, Some(12.5)); // 53
        assert_eq!(s.low_pct, Some(25.0)); // 54, 69
        assert_eq!(s.in_range_pct, Some(25.0)); // 70, 180
        assert_eq!(s.high_pct, Some(25.0)); // 181, 250
        assert_eq!(s.very_high_pct, Some(12.5)); // 251
        assert_eq!(s.wear_fraction, 1.0);
        Ok(())
    }

    #[test]
    fn mean_and_gmi() -> Result<()> {
        let r = series(14, 5, |_| 154.0);
        let s = summarize(&r, at(0), at(14 * 1440), 5.0)?;
        assert_eq!(s.mean_mg_dl, Some(154.0));
        // 3.31 + 0.02392 * 154 = 6.99
        assert_eq!(s.gmi_percent, Some(7.0));
        Ok(())
    }

    #[test]
    fn under_70_percent_wear_is_not_valid_and_shows_no_numbers() -> Result<()> {
        // readings on 9 of 14 days: 64% wear
        let r: Vec<_> = series(14, 5, |_| 120.0)
            .into_iter()
            .filter(|(t, _)| (*t - at(0)).num_days() < 9)
            .collect();
        let s = summarize(&r, at(0), at(14 * 1440), 5.0)?;
        assert!(!s.valid);
        assert_eq!(s.invalid_reason.as_deref(), Some("insufficient_wear"));
        assert!((s.wear_fraction - 9.0 / 14.0).abs() < 0.001, "{}", s.wear_fraction);
        assert_eq!((s.in_range_pct, s.mean_mg_dl, s.gmi_percent), (None, None, None));
        // 10 of 14 days: 71%, valid
        let r: Vec<_> = series(14, 5, |_| 120.0)
            .into_iter()
            .filter(|(t, _)| (*t - at(0)).num_days() < 10)
            .collect();
        assert!(summarize(&r, at(0), at(14 * 1440), 5.0)?.valid);
        Ok(())
    }

    #[test]
    fn a_short_window_or_no_data_is_not_valid() -> Result<()> {
        let r = series(7, 5, |_| 120.0);
        let s = summarize(&r, at(0), at(7 * 1440), 5.0)?;
        assert_eq!(s.invalid_reason.as_deref(), Some("short_window"));
        let s = summarize(&[], at(0), at(14 * 1440), 5.0)?;
        assert_eq!((s.readings, s.invalid_reason.as_deref()), (0, Some("no_readings")));
        assert!(summarize(&[], at(10), at(0), 5.0).is_err());
        assert!(summarize(&[], at(0), at(10), 0.0).is_err());
        Ok(())
    }

    #[test]
    fn repeats_within_a_minute_count_once_and_wear_counts_slots() -> Result<()> {
        let mut r = series(14, 5, |_| 100.0);
        // a second uploader writes every reading again 20 s later, all of them high
        let copies: Vec<_> = r.iter().map(|(t, _)| (*t + Duration::seconds(20), 300.0)).collect();
        r.extend(copies);
        let s = summarize(&r, at(0), at(14 * 1440), 5.0)?;
        assert_eq!(s.readings, 14 * 288);
        assert_eq!(s.in_range_pct, Some(100.0));
        assert_eq!(s.wear_fraction, 1.0);
        // a 15-minute Libre setup read as 5-minute slots is a third of the wear
        let r = series(14, 15, |_| 100.0);
        assert!((summarize(&r, at(0), at(14 * 1440), 5.0)?.wear_fraction - 1.0 / 3.0).abs() < 0.001);
        assert_eq!(summarize(&r, at(0), at(14 * 1440), 15.0)?.wear_fraction, 1.0);
        Ok(())
    }

    #[test]
    fn the_interval_is_the_median_gap_snapped_to_a_sensor_interval() {
        assert_eq!(infer_interval(&series(1, 5, |_| 1.0)), 5.0);
        assert_eq!(infer_interval(&series(1, 15, |_| 1.0)), 15.0);
        assert_eq!(infer_interval(&series(1, 1, |_| 1.0)), 1.0);
        assert_eq!(infer_interval(&[]), DEFAULT_INTERVAL_MINUTES);
        assert_eq!(infer_interval(&series(1, 10, |_| 1.0)), 5.0);
        assert_eq!(infer_interval(&series(1, 3, |_| 1.0)), 1.0);
        assert_eq!(infer_interval(&series(1, 30, |_| 1.0)), 15.0);
    }

    #[test]
    fn evenly_sparse_readings_do_not_look_like_full_wear() -> Result<()> {
        // a 5-minute sensor that lost every other reading for 14 days: half the wear, not valid
        let r: Vec<_> = series(14, 10, |_| 120.0);
        let interval = infer_interval(&r);
        assert_eq!(interval, 5.0);
        let s = summarize(&r, at(0), at(14 * 1440), interval)?;
        assert!((s.wear_fraction - 0.5).abs() < 0.001, "{s:?}");
        assert!(!s.valid);
        assert_eq!(s.invalid_reason.as_deref(), Some("insufficient_wear"));
        Ok(())
    }
}
