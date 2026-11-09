//! Synthetic CGM data (demo seed and tests; never a real person's): a day of meals, a night dip, an
//! afternoon low twice a week, sensor noise, short dropouts and a two-hour sensor change every ten days.
//! Deterministic for a seed.

use chrono::{DateTime, Duration, Timelike, Utc};

/// xorshift64*: small, deterministic, good enough for noise.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A meal response: 0 before the meal, a rise to `peak` at +55 min, back down by about +3 h.
fn meal(minutes_since: f64, peak: f64) -> f64 {
    if minutes_since < 0.0 {
        return 0.0;
    }
    let x = minutes_since / 55.0;
    peak * x * (1.0 - x).exp()
}

/// CGM readings (time, mg/dL as whole numbers, as a sensor reports) every `interval_minutes` from
/// `start` for `days` days.
pub fn cgm_readings(start: DateTime<Utc>, days: i64, interval_minutes: i64, seed: u64) -> Vec<(DateTime<Utc>, f64)> {
    let mut rng = Rng(seed | 1);
    let step = interval_minutes.max(1);
    let per_day = 1440 / step;
    let mut out = Vec::with_capacity((days * per_day) as usize);
    let mut dropout_until: Option<DateTime<Utc>> = None;
    for day in 0..days {
        let peaks = [
            40.0 + 40.0 * rng.next(),
            55.0 + 45.0 * rng.next(),
            50.0 + 50.0 * rng.next(),
        ];
        let low_day = day % 7 == 2 || day % 7 == 5;
        for i in 0..per_day {
            let t = start
                + Duration::days(day)
                + Duration::minutes(i * step)
                + Duration::seconds((rng.next() * 20.0) as i64);
            // a sensor change: two hours without readings every ten days
            if day % 10 == 9 && (600..720).contains(&(i * step)) {
                continue;
            }
            if dropout_until.is_some_and(|u| t < u) {
                continue;
            }
            // a dropout of 10 to 20 minutes, about twice a day
            if rng.next() < 2.0 / per_day as f64 {
                dropout_until = Some(t + Duration::minutes(10 + (rng.next() * 10.0) as i64));
                continue;
            }
            let m = f64::from(t.hour() * 60 + t.minute());
            let mut g = 104.0 - 10.0 * (-((m - 180.0) / 90.0).powi(2)).exp();
            g += meal(m - 450.0, peaks[0]) + meal(m - 810.0, peaks[1]) + meal(m - 1230.0, peaks[2]);
            if low_day {
                g -= 55.0 * (-((m - 1050.0) / 30.0).powi(2)).exp();
            }
            g += (rng.next() + rng.next() + rng.next() - 1.5) * 8.0;
            out.push((t, g.round().clamp(40.0, 400.0)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::cgm;

    #[test]
    fn fourteen_days_are_a_valid_mostly_in_range_summary() -> ownhealth_core::Result<()> {
        let start = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).single().unwrap_or_default();
        let r = cgm_readings(start, 14, 5, 7);
        assert_eq!(r, cgm_readings(start, 14, 5, 7), "deterministic");
        let s = cgm::summarize(&r, start, start + Duration::days(14), 5.0)?;
        assert!(s.valid, "{s:?}");
        assert!(s.wear_fraction > 0.9 && s.wear_fraction < 1.0, "{}", s.wear_fraction);
        let tir = s.in_range_pct.unwrap_or_default();
        assert!((70.0..98.0).contains(&tir), "{s:?}");
        assert!(s.low_pct.unwrap_or_default() > 0.0, "{s:?}");
        assert!(s.high_pct.unwrap_or_default() > 0.0, "{s:?}");
        Ok(())
    }
}
