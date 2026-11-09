//! Health Auto Export push (exit criterion "Phone push"): 7 days of 15-minute pushes with overlapping
//! windows import with 0 duplicate samples, and buckets the app re-sends with a larger value end at
//! their final value. Synthetic data for Alex Rivera.

mod common;

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, FixedOffset, TimeZone, Timelike, Utc};
use common::{Env, R};
use ownhealth_core::sources::{RunOutcome, RunProblem};
use ownhealth_devices::hae;
use serde_json::{Value, json};

fn fmt(t: DateTime<Utc>) -> String {
    let tz = FixedOffset::east_opt(3600).unwrap_or_else(|| panic!("offset"));
    t.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S %z").to_string()
}

fn start() -> DateTime<Utc> {
    // 2026-11-02 00:00 +01:00
    Utc.with_ymd_and_hms(2026, 11, 1, 23, 0, 0).single().unwrap_or_default()
}

fn hr_at(i: i64) -> f64 {
    (58 + (i * 7) % 40) as f64
}

fn steps_total(hour: i64) -> f64 {
    (100 + (hour * 37) % 900) as f64
}

const WINDOW_HOURS: i64 = 3;

/// The push the app sends at `now`: the last three hours of heart rate (every 5 min), hourly steps with
/// the open hour partial, last night's sleep (growing while asleep) and the day's resting heart rate.
fn push(now: DateTime<Utc>) -> Value {
    let from = (now - Duration::hours(WINDOW_HOURS)).max(start());
    let mut hr = Vec::new();
    let mut t = start() + Duration::minutes(((from - start()).num_minutes() + 4) / 5 * 5);
    while t < now {
        let i = (t - start()).num_minutes() / 5;
        // the app merges sources on some buckets once the iPhone's data arrives: the same bucket comes
        // with one source at its first push and with two at the next ones
        let source = if i % 13 == 0 && now - t > Duration::minutes(15) {
            "iPhone de Alex|Apple Watch de Alex"
        } else {
            "Apple Watch de Alex"
        };
        hr.push(
            json!({"date": fmt(t), "Min": hr_at(i) - 3.0, "Avg": hr_at(i), "Max": hr_at(i) + 5.0, "source": source}),
        );
        t += Duration::minutes(5);
    }
    let mut steps = Vec::new();
    let mut h = start() + Duration::hours((from - start()).num_hours());
    while h < now {
        let hour = (h - start()).num_hours();
        let done = ((now - h).num_seconds() as f64 / 3600.0).min(1.0);
        let v = (steps_total(hour) * done).floor();
        if v > 0.0 {
            // the iPhone writes the open hour first; the watch's share syncs once the hour is over, and
            // from then on the app sends the bucket with both sources
            let source = if now - h < Duration::hours(1) {
                "iPhone de Alex"
            } else {
                "iPhone de Alex|Apple Watch de Alex"
            };
            steps.push(json!({"date": fmt(h), "qty": v, "source": source}));
        }
        h += Duration::hours(1);
    }
    let local = now + Duration::hours(1);
    let day0 = now - Duration::seconds(i64::from(local.num_seconds_from_midnight()));
    let (bed, wake) = (day0 - Duration::minutes(75), day0 + Duration::minutes(390));
    let mut sleep = Vec::new();
    if now > bed && now - Duration::hours(WINDOW_HOURS) < wake && bed >= start() && wake <= start() + Duration::days(7)
    {
        let end = now.min(wake);
        sleep.push(
            json!({"date": fmt(day0), "totalSleep": (end - bed).num_minutes() as f64 / 60.0,
            "sleepStart": fmt(bed), "sleepEnd": fmt(end), "inBed": 0, "source": "Apple Watch de Alex"}),
        );
    }
    let mut resting = Vec::new();
    if local.hour() >= 8 {
        resting
            .push(json!({"date": fmt(day0), "qty": 52 + (day0 - start()).num_days(), "source": "Apple Watch de Alex"}));
    }
    json!({"data": {"metrics": [
        {"name": "heart_rate", "units": "count/min", "data": hr},
        {"name": "step_count", "units": "count", "data": steps},
        {"name": "sleep_analysis", "units": "hr", "data": sleep},
        {"name": "resting_heart_rate", "units": "count/min", "data": resting},
        {"name": "dietary_water", "units": "mL", "data": [{"date": fmt(now), "qty": 250}]}
    ], "workouts": []}})
}

#[tokio::test]
async fn seven_days_of_overlapping_15_minute_pushes_give_no_duplicates() -> R {
    let env = Env::new().await?;
    let source = env.source("health_auto_export", Some("alex"), json!({}), None).await?;
    let alex = env.fam.profile("alex");
    let pushes = 7 * 24 * 4;
    let mut new_total = 0i64;
    let mut seen_total = 0i64;
    for k in 1..=pushes {
        let now = start() + Duration::minutes(15 * k);
        let body = serde_json::to_vec(&push(now))?;
        let mut tx = env.db.pool.begin().await?;
        let o = hae::import_push(&mut tx, &env.blobs, &source, &body).await?;
        tx.commit().await?;
        new_total += i64::from(o.new);
        seen_total += i64::from(o.seen);
        assert_eq!(o.seen, o.new + o.duplicate);
    }
    let rows = env
        .count("select count(*) from wearable_sample where profile_id = $1", alex)
        .await?;
    let distinct = env
        .count(
            "select count(*) from (select distinct metric, t from wearable_sample where profile_id = $1) d",
            alex,
        )
        .await?;
    // 0 duplicate samples: one row per metric and time, whatever source string a bucket carried
    assert_eq!(rows, distinct, "duplicate samples");
    // and exactly the samples that exist: 7 days of 5-minute HR, 168 hours of steps, the 6 whole nights, 7 days
    let by_metric: Vec<(String, i64)> =
        sqlx::query_as("select metric, count(*) from wearable_sample where profile_id = $1 group by 1 order by 1")
            .bind(alex)
            .fetch_all(&env.db.pool)
            .await?;
    let by_metric: BTreeMap<String, i64> = by_metric.into_iter().collect();
    assert_eq!(
        by_metric,
        BTreeMap::from([
            ("hr".to_string(), 7 * 288),
            ("hr_resting".to_string(), 7),
            ("sleep_asleep".to_string(), 6),
            ("steps".to_string(), 7 * 24),
        ])
    );
    assert_eq!(new_total, rows, "every sample counted new exactly once");
    assert!(
        seen_total > 5 * rows,
        "the windows overlapped: {seen_total} seen for {rows} rows"
    );
    // the open hour was re-sent with more steps at each push; the stored value is the full hour
    let steps: Vec<(DateTime<Utc>, f32)> =
        sqlx::query_as("select t, v from wearable_sample where profile_id = $1 and metric = 'steps' order by t")
            .bind(alex)
            .fetch_all(&env.db.pool)
            .await?;
    for (t, v) in &steps {
        let hour = (*t - start()).num_hours();
        assert_eq!(f64::from(*v), steps_total(hour), "steps at {t}");
    }
    // the night ended at 06:30: 7 h 45 min asleep, not the partial value of the first push
    let sleep: Vec<f32> =
        sqlx::query_scalar("select v from wearable_sample where profile_id = $1 and metric = 'sleep_asleep'")
            .bind(alex)
            .fetch_all(&env.db.pool)
            .await?;
    assert!(sleep.iter().all(|v| (*v - 465.0).abs() < 0.01), "{sleep:?}");
    // one wearable source for the inbox, whatever source strings the buckets carried; the raw pushes
    // are kept as blobs
    let devices: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "select provider, device, external_id from wearable_source where profile_id = $1 and source_id = $2",
    )
    .bind(alex)
    .bind(source.id)
    .fetch_all(&env.db.pool)
    .await?;
    assert_eq!(
        devices,
        vec![("health_auto_export".into(), None, Some(source.id.to_string()))]
    );
    let kept: i64 =
        sqlx::query_scalar("select count(*) from source_item where source_id = $1 and external_id like 'push:%'")
            .bind(source.id)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(kept, pushes);
    // the daily series reads one value per day
    let days = env
        .count(
            "select count(*) from wearable_rollup where profile_id = $1 and metric = 'steps' and bucket = 'day'",
            alex,
        )
        .await?;
    assert_eq!(days, 7);
    Ok(())
}

#[tokio::test]
async fn the_same_push_twice_adds_nothing_and_refused_pushes_write_nothing() -> R {
    let env = Env::new().await?;
    let alex = env.fam.profile("alex");
    let source = env.source("health_auto_export", Some("alex"), json!({}), None).await?;
    let body = serde_json::to_vec(&push(start() + Duration::hours(10)))?;
    let mut c = env.db.pool.acquire().await?;
    let first = hae::import_push(&mut c, &env.blobs, &source, &body).await?;
    let again = hae::import_push(&mut c, &env.blobs, &source, &body).await?;
    assert!(first.new > 0);
    assert_eq!((again.new, again.duplicate, again.seen), (0, first.seen, first.seen));

    let unbound = env.source("health_auto_export", None, json!({}), None).await?;
    let e = hae::import_push(&mut c, &env.blobs, &unbound, &body).await;
    assert!(
        matches!(e, Err(ownhealth_core::Error::Invalid(ref m)) if m.contains("no profile")),
        "{e:?}"
    );
    let other = env
        .source("nightscout", Some("alex"), json!({"base_url": "http://x"}), None)
        .await?;
    assert!(hae::import_push(&mut c, &env.blobs, &other, &body).await.is_err());
    assert!(matches!(
        hae::import_push(&mut c, &env.blobs, &source, b"{\"not\": \"hae\"}").await,
        Err(ownhealth_core::Error::Invalid(_))
    ));
    let rows = env
        .count("select count(*) from wearable_sample where profile_id = $1", alex)
        .await?;
    assert_eq!(rows, i64::from(first.new));
    Ok(())
}

fn import(
    env: &Env,
    source: &ownhealth_core::sources::Source,
    body: &Value,
) -> impl Future<Output = anyhow::Result<RunOutcome>> {
    let body = serde_json::to_vec(body);
    async move {
        let mut c = env.db.pool.acquire().await?;
        Ok(hae::import_push(&mut c, &env.blobs, source, &body?).await?)
    }
}

#[tokio::test]
async fn a_day_bucket_resent_after_the_phone_changed_time_zone_stays_one_sample() -> R {
    let env = Env::new().await?;
    let alex = env.fam.profile("alex");
    let source = env.source("health_auto_export", Some("alex"), json!({}), None).await?;
    let resting = |date: &str, v: i64| {
        json!({"data": {"metrics": [{"name": "resting_heart_rate", "units": "count/min",
            "data": [{"date": date, "qty": v, "source": "Apple Watch de Alex"}]}]}})
    };
    import(&env, &source, &resting("2026-11-02 00:00:00 +0100", 54)).await?;
    // Alex flies east: the app re-sends the same day as local midnight at +02:00
    let o = import(&env, &source, &resting("2026-11-02 00:00:00 +0200", 53)).await?;
    assert_eq!(o.new, 0, "{o:?}");
    let rows: Vec<(DateTime<Utc>, f32, Option<i16>)> = sqlx::query_as(
        "select t, v, tz_offset_min from wearable_sample where profile_id = $1 and metric = 'hr_resting'",
    )
    .bind(alex)
    .fetch_all(&env.db.pool)
    .await?;
    let day = Utc.with_ymd_and_hms(2026, 11, 2, 0, 0, 0).single();
    assert_eq!(
        rows.iter().map(|r| (Some(r.0), r.1, r.2)).collect::<Vec<_>>(),
        vec![(day, 53.0, Some(120))]
    );
    Ok(())
}

#[tokio::test]
async fn points_lost_to_an_unknown_unit_make_the_run_partial() -> R {
    let env = Env::new().await?;
    let alex = env.fam.profile("alex");
    let source = env.source("health_auto_export", Some("alex"), json!({}), None).await?;
    let weight = |units: &str, qty: f64| {
        json!({"data": {"metrics": [{"name": "weight_body_mass", "units": units,
            "data": [{"date": "2026-11-02 07:00:00 +0100", "qty": qty}, {"date": "2026-11-03 07:00:00 +0100", "qty": qty}]}]}})
    };
    // stone is not a unit we convert: both points are lost, and the run says so
    let o = import(&env, &source, &weight("stone", 11.0)).await?;
    assert_eq!(
        o.problems,
        vec![RunProblem {
            code: "points_skipped".into(),
            message: "unit:weight_body_mass x2".into()
        }]
    );
    assert_eq!(o.new, 0);
    // lb converts: imported, no problem
    let o = import(&env, &source, &weight("lb", 150.0)).await?;
    assert_eq!((o.new, o.problems.len()), (2, 0), "{o:?}");
    // only metrics and sections we do not chart yet: nothing lost that we would show, the run is ok
    let o = import(
        &env,
        &source,
        &json!({"data": {"metrics": [{"name": "dietary_water", "units": "mL",
            "data": [{"date": "2026-11-02 08:00:00 +0100", "qty": 250}]}], "workouts": [{"name": "Run"}]}}),
    )
    .await?;
    assert_eq!((o.seen, o.problems.len()), (0, 0), "{o:?}");
    let rows = env
        .count("select count(*) from wearable_sample where profile_id = $1", alex)
        .await?;
    assert_eq!(rows, 2);
    Ok(())
}
