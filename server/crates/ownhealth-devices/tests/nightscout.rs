//! Nightscout pull against a local fake Nightscout (never the internet), and the CGM exit criterion:
//! 14 days of Nightscout data give a time in range within 1 percentage point of Nightscout's own
//! "Distribution" report, computed here as Nightscout computes it (`nightscout_in_range_pct`).
//! Synthetic CGM for Lucía Rivera.

mod common;

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Duration, DurationRound, Utc};
use common::{Env, R};
use ownhealth_core::jobs;
use ownhealth_core::secrets::{SecretBox, SecretString, store_secret};
use ownhealth_core::sources::{self, Source};
use ownhealth_devices::{cgm, nightscout, synthetic};
use serde_json::{Value, json};
use uuid::Uuid;

const TOKEN: &str = "lucia-reader-6b2f0c1d9e8a7f3b";
const JWT: &str = "jwt.fake.nightscout";

#[derive(Default)]
struct Fake {
    /// (date ms, sgv, device)
    entries: Vec<(i64, f64, &'static str)>,
    requests: Vec<String>,
}

type Shared = Arc<Mutex<Fake>>;

fn param(q: &[(String, String)], k: &str) -> Option<i64> {
    q.iter().find(|(key, _)| key == k).and_then(|(_, v)| v.parse().ok())
}

fn entry(e: &(i64, f64, &str)) -> Value {
    json!({"type": "sgv", "sgv": e.1, "date": e.0, "device": e.2, "direction": "Flat", "utcOffset": 60})
}

async fn v1(State(s): State<Shared>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let mut f = s.lock().unwrap_or_else(|e| e.into_inner());
    f.requests.push(format!("v1 {q:?}"));
    if !q.iter().any(|(k, v)| k == "token" && v == TOKEN) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"status": 401}))).into_response();
    }
    let gt = param(&q, "find[date][$gt]").unwrap_or(i64::MIN);
    let lt = param(&q, "find[date][$lt]").unwrap_or(i64::MAX);
    let count = param(&q, "count").unwrap_or(10) as usize;
    let mut out: Vec<_> = f.entries.iter().filter(|e| e.0 > gt && e.0 < lt).cloned().collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.0));
    out.truncate(count);
    // a calibration and a status code among the readings, as real sites have
    let mut body: Vec<Value> = out.iter().map(entry).collect();
    if let Some(first) = out.first() {
        body.push(json!({"type": "cal", "date": first.0 - 1, "slope": 1000}));
    }
    Json(Value::Array(body)).into_response()
}

async fn v2_auth(State(s): State<Shared>, Path(token): Path<String>) -> Response {
    s.lock()
        .unwrap_or_else(|e| e.into_inner())
        .requests
        .push("v2 auth".into());
    if token == TOKEN {
        Json(json!({"token": JWT, "sub": "lucia-reader"})).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn v3(State(s): State<Shared>, headers: HeaderMap, Query(q): Query<Vec<(String, String)>>) -> Response {
    let mut f = s.lock().unwrap_or_else(|e| e.into_inner());
    f.requests.push(format!("v3 {q:?}"));
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {JWT}")) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let gt = param(&q, "date$gt").unwrap_or(i64::MIN);
    let limit = param(&q, "limit").unwrap_or(10) as usize;
    let mut out: Vec<_> = f.entries.iter().filter(|e| e.0 > gt).cloned().collect();
    out.sort_by_key(|e| e.0);
    out.truncate(limit);
    Json(json!({"status": 200, "result": out.iter().map(entry).collect::<Vec<_>>()})).into_response()
}

async fn serve(fake: Shared) -> anyhow::Result<String> {
    let app = axum::Router::new()
        .route("/api/v1/entries/sgv.json", get(v1))
        .route("/api/v2/authorization/request/{token}", get(v2_auth))
        .route("/api/v3/entries", get(v3))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{addr}"))
}

fn fake_with(readings: &[(DateTime<Utc>, f64)], device: &'static str) -> Shared {
    Arc::new(Mutex::new(Fake {
        entries: readings
            .iter()
            .map(|(t, v)| (t.timestamp_millis(), *v, device))
            .collect(),
        requests: Vec::new(),
    }))
}

async fn ns_source(
    env: &Env,
    base: &str,
    api: &str,
    token: Option<&str>,
    allow_private: bool,
) -> anyhow::Result<Source> {
    let secret = match token {
        Some(t) => {
            let sb = SecretBox::from_settings(&env.ctx.settings)?;
            let mut c = env.db.pool.acquire().await?;
            Some(
                store_secret(
                    &mut c,
                    &sb,
                    env.fam.household_id,
                    "nightscout_token",
                    Some("lector"),
                    &SecretString::new(t),
                )
                .await?,
            )
        }
        None => None,
    };
    env.source(
        "nightscout",
        Some("lucia"),
        json!({"base_url": base, "allow_private": allow_private, "api": api}),
        secret,
    )
    .await
}

/// One `source.poll` job through the real job path (begin_run, poller, finish_run with the cursor).
async fn poll(env: &Env, source: Uuid) -> anyhow::Result<Value> {
    let mut c = env.db.pool.acquire().await?;
    sources::enqueue_poll(&mut c, source).await?;
    jobs::drain(&env.ctx, "core", 10, None).await?;
    let runs = sources::list_runs(&mut c, source, 1).await?;
    Ok(serde_json::to_value(&runs[0])?)
}

/// Nightscout's "In Range" percentage from its Distribution report, ported line by line from
/// nightscout/cgm-remote-monitor at commit c6a0d8ed65637bc90d3090e6e7a0f752041c57fd:
///
/// - `lib/report/reportclient.js` `loadCGMData` and the lines after it: each report day reads
///   `/api/v1/entries.json?find[date][$gte]=<day start>&find[date][$lt]=<day end>`, keeps `sgv`
///   entries, sorts them, drops entries less than a minute after the last kept one (`uniqSgv`,
///   `lib/report/uniqsgv.js`), drops values below 39, and maps them to `{sgv, bgValue, displayTime}`
///   (`statsrecords`); `allstatsrecords` is every day's records together.
/// - `lib/report_plugins/glucosedistribution.js` `report`: pass 0 drops records without sgv or below
///   39 and sorts (its duplicate check never matches: `seen.includes` on an array filled by key);
///   pass 1 fills gaps of 9 to 25 minutes with interpolated whole-number readings (`Math.floor`),
///   starting from `[data[0]]` and pushing `data[0]` again inside the loop, as the code does; pass 2
///   replaces single jumpy readings (both deltas not <= 8 and of opposite sign, around neighbours at
///   most 5 min 10 s away) with the midpoint; then Low is `sgv > 0 && sgv < targetLow`, High is
///   `sgv >= targetHigh`, each `toFixed(1)` of the share of records, and In Range is
///   `(100 - Low - High).toFixed(1)`.
///
/// Days are UTC days (a site whose profile time zone is UTC). Every hour of the day is enabled.
fn nightscout_in_range_pct(entries: &[(i64, f64)], days: &[i64], target_low: f64, target_high: f64) -> f64 {
    #[derive(Clone, Copy)]
    struct Rec {
        sgv: f64,
        bg: f64,
        t: i64,
    }
    const DAY: i64 = 86_400_000;
    let mut data: Vec<Rec> = Vec::new();
    for &from in days {
        let mut day: Vec<(i64, f64)> = entries
            .iter()
            .copied()
            .filter(|e| e.0 >= from && e.0 < from + DAY)
            .collect();
        day.sort_by_key(|e| e.0);
        let mut last = 0i64;
        for (t, sgv) in day {
            if last + 60_000 <= t {
                last = t;
                if sgv >= 39.0 {
                    data.push(Rec {
                        sgv,
                        bg: sgv.trunc(),
                        t,
                    });
                }
            }
        }
    }
    data.retain(|r| r.sgv != 0.0 && r.bg != 0.0 && r.bg >= 39.0);
    data.sort_by_key(|r| r.t);
    if data.is_empty() {
        return 0.0;
    }
    // pass 1
    let mut g = vec![data[0]];
    for i in 0..data.len() - 1 {
        let (e, n) = (data[i], data[i + 1]);
        let dt = n.t - e.t;
        // `timeDelta < 9 min || timeDelta > 25 min`
        if !(9 * 60 * 1000..=25 * 60 * 1000).contains(&dt) {
            g.push(e);
            continue;
        }
        let missing = (dt as f64 / (5.0 * 60.0 * 990.0)).floor() as i64 - 1;
        let patch = (dt as f64 / (missing + 1) as f64).floor() as i64;
        let bg_delta = (n.bg - e.bg) / (missing + 1) as f64;
        g.push(e);
        for j in 1..=missing {
            let bg = (e.bg + bg_delta * j as f64).floor();
            g.push(Rec {
                sgv: bg,
                bg,
                t: e.t + j * patch,
            });
        }
    }
    g.push(data[data.len() - 1]);
    // pass 2
    let max_gap = 5 * 60 * 1000 + 10_000;
    let mut g2 = vec![g[0]];
    let mut prev = g[0];
    for i in 1..g.len().saturating_sub(1) {
        let (e, n) = (g[i], g[i + 1]);
        if n.t - e.t > max_gap || e.t - prev.t > max_gap {
            g2.push(e);
            prev = e;
            continue;
        }
        let (d1, d2) = (e.bg - prev.bg, n.bg - e.bg);
        if d1 <= 8.0 && d2 <= 8.0 {
            g2.push(e);
            prev = e;
            continue;
        }
        if (d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0) {
            let v = prev.bg + (n.bg - prev.bg) / 2.0;
            let r = Rec { sgv: v, bg: v, t: e.t };
            g2.push(r);
            prev = r;
            continue;
        }
        g2.push(e);
        prev = e;
    }
    g2.push(g[g.len() - 1]);
    let n = g2.len() as f64;
    let fixed1 = |x: f64| (x * 10.0).round() / 10.0;
    let low = fixed1(100.0 * g2.iter().filter(|r| r.sgv > 0.0 && r.sgv < target_low).count() as f64 / n);
    let high = fixed1(100.0 * g2.iter().filter(|r| r.sgv >= target_high).count() as f64 / n);
    fixed1(100.0 - low - high)
}

#[tokio::test]
async fn fourteen_days_give_nightscouts_time_in_range_within_one_point() -> R {
    let env = Env::new().await?;
    let lucia = env.fam.profile("lucia");
    let end = Utc::now().duration_trunc(Duration::days(1))?;
    let start = end - Duration::days(14);
    for seed in [3u64, 11, 29] {
        let readings = synthetic::cgm_readings(start, 14, 5, seed);
        let fake = fake_with(&readings, "xDrip-DexcomG6");
        let base = serve(fake.clone()).await?;
        // one profile per seed: wipe the previous run's readings
        sqlx::query("delete from wearable_rollup where profile_id = $1")
            .bind(lucia)
            .execute(&env.db.pool)
            .await?;
        sqlx::query("delete from wearable_sample where profile_id = $1")
            .bind(lucia)
            .execute(&env.db.pool)
            .await?;
        let source = ns_source(&env, &base, "v1", Some(TOKEN), true).await?;
        let run = poll(&env, source.id).await?;
        assert_eq!(run["status"], "ok", "{run}");
        assert_eq!(run["items_new"], json!(readings.len()), "{run}");

        let mut c = env.db.pool.acquire().await?;
        let ours = cgm::cgm_summary(&mut c, lucia, end, 14).await?;
        assert!(ours.valid, "{ours:?}");
        let entries: Vec<(i64, f64)> = readings.iter().map(|(t, v)| (t.timestamp_millis(), *v)).collect();
        let days: Vec<i64> = (0..14)
            .map(|d| (start + Duration::days(d)).timestamp_millis())
            .collect();
        let theirs = nightscout_in_range_pct(&entries, &days, 70.0, 180.0);
        let tir = ours.in_range_pct.unwrap_or_default();
        println!(
            "seed {seed}: ours {tir} %, Nightscout's report {theirs} %, wear {}",
            ours.wear_fraction
        );
        assert!(
            (tir - theirs).abs() <= 1.0,
            "seed {seed}: ours {tir}, Nightscout {theirs}"
        );
        // the v1 API pages backwards: 14 days of 5-minute readings take four pages
        let pages = fake.lock().unwrap_or_else(|e| e.into_inner()).requests.len();
        assert!(pages >= 4, "{pages} requests");
    }
    Ok(())
}

#[test]
fn the_reference_port_does_what_nightscout_does() {
    let (m, day) = (60_000, 1_760_054_400_000); // a UTC midnight
    let e = [
        (day, 100.0),
        (day + 15 * m, 130.0),
        (day + 45 * m, 200.0),
        (day + 50 * m, 200.0),
    ];
    // a 15-minute gap gets two interpolated readings, a 30-minute gap none; data[0] is pushed twice:
    // 100, 100, 110, 120, 130, 200, 200 -> High 2/7 = 28.6, In Range 71.4
    assert_eq!(nightscout_in_range_pct(&e, &[day], 70.0, 180.0), 71.4);
    // 180 counts as high in Nightscout (`< targetHigh`), in range in the consensus
    assert_eq!(
        nightscout_in_range_pct(&[(day, 180.0), (day + 5 * m, 180.0)], &[day], 70.0, 180.0),
        0.0
    );
    // a single jumpy reading becomes the midpoint of its neighbours: 120, 120, 60 -> 120, 120, 120
    let jumpy = [(day, 120.0), (day + 5 * m, 60.0), (day + 10 * m, 120.0)];
    assert_eq!(nightscout_in_range_pct(&jumpy, &[day], 70.0, 180.0), 100.0);
}

#[tokio::test]
async fn polls_are_incremental_and_add_no_duplicates() -> R {
    let env = Env::new().await?;
    let lucia = env.fam.profile("lucia");
    let now = Utc::now().duration_trunc(Duration::minutes(5))?;
    let readings = synthetic::cgm_readings(now - Duration::days(2), 2, 5, 5);
    let (old, new): (Vec<_>, Vec<_>) = readings.iter().partition(|r| r.0 < now - Duration::hours(6));
    let fake = fake_with(&old, "xDrip-DexcomG6");
    let base = serve(fake.clone()).await?;
    let source = ns_source(&env, &base, "v1", Some(TOKEN), true).await?;
    let first = poll(&env, source.id).await?;
    assert_eq!(first["items_new"], json!(old.len()));
    let newest_old = old.iter().map(|r| r.0.timestamp_millis()).max().unwrap_or_default();
    {
        let mut f = fake.lock().unwrap_or_else(|e| e.into_inner());
        f.entries
            .extend(new.iter().map(|(t, v)| (t.timestamp_millis(), *v, "xDrip-DexcomG6")));
        f.requests.clear();
    }
    let second = poll(&env, source.id).await?;
    // the run reads the day before the cursor again (backfilled readings): those are already stored
    let overlap_from = newest_old - Duration::hours(nightscout::DEFAULT_OVERLAP_HOURS).num_milliseconds();
    let reread = old.iter().filter(|r| r.0.timestamp_millis() > overlap_from).count();
    assert_eq!(
        (second["items_new"].clone(), second["items_duplicate"].clone()),
        (json!(new.len()), json!(reread))
    );
    let asked = fake.lock().unwrap_or_else(|e| e.into_inner()).requests.clone();
    assert!(
        asked[0].contains(&format!("\"{overlap_from}\"")),
        "the cursor was not used: {asked:?}"
    );
    // nothing new: a run that adds nothing
    let third = poll(&env, source.id).await?;
    assert_eq!(
        (third["status"].clone(), third["items_new"].clone()),
        (json!("ok"), json!(0))
    );
    let rows = env
        .count("select count(*) from wearable_sample where profile_id = $1", lucia)
        .await?;
    assert_eq!(rows, readings.len() as i64);
    let linked: i64 =
        sqlx::query_scalar("select count(*) from wearable_source where source_id = $1 and provider = 'nightscout'")
            .bind(source.id)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(linked, 1);
    Ok(())
}

#[tokio::test]
async fn freestyle_libre_through_nightscout_connect_on_the_v3_api() -> R {
    let env = Env::new().await?;
    let lucia = env.fam.profile("lucia");
    let end = Utc::now().duration_trunc(Duration::minutes(15))?;
    // LibreLinkUp history: one reading every 15 minutes, written by nightscout-connect
    let readings = synthetic::cgm_readings(end - Duration::days(14), 14, 15, 17);
    let fake = fake_with(&readings, "nightscout-connect-librelinkup");
    let base = serve(fake.clone()).await?;
    let source = ns_source(&env, &base, "v3", Some(TOKEN), true).await?;
    let run = poll(&env, source.id).await?;
    assert_eq!(
        (run["status"].clone(), run["items_new"].clone()),
        (json!("ok"), json!(readings.len())),
        "{run}"
    );
    let asked = fake.lock().unwrap_or_else(|e| e.into_inner()).requests.clone();
    assert_eq!(asked[0], "v2 auth");
    assert!(asked.iter().filter(|r| r.starts_with("v3")).count() >= 2, "{asked:?}");
    let mut c = env.db.pool.acquire().await?;
    let s = cgm::cgm_summary(&mut c, lucia, end + Duration::minutes(1), 14).await?;
    assert!(s.valid, "a 15-minute sensor read with its own interval: {s:?}");
    assert!(s.wear_fraction > 0.9, "{s:?}");
    Ok(())
}

#[tokio::test]
async fn a_wrong_token_or_a_private_address_fails_the_run_without_leaking_the_token() -> R {
    let env = Env::new().await?;
    let readings = synthetic::cgm_readings(Utc::now() - Duration::days(1), 1, 5, 1);
    let base = serve(fake_with(&readings, "xDrip")).await?;
    let wrong = ns_source(&env, &base, "v1", Some("lucia-reader-wrong-0000"), true).await?;
    let run = poll(&env, wrong.id).await?;
    assert_eq!(run["status"], "error");
    let error = run["error"].as_str().unwrap_or_default();
    assert!(error.contains("refused the token"), "{error}");
    assert!(!error.contains("lucia-reader"), "the token leaked: {error}");
    let mut c = env.db.pool.acquire().await?;
    let health = sources::source_health(&mut c, env.fam.household_id).await?;
    let h = health
        .iter()
        .find(|h| h.id == wrong.id)
        .ok_or_else(|| anyhow::anyhow!("no health row"))?;
    assert_eq!((h.consecutive_failures, h.last_success_at), (1, None));

    // the same site, not ticked "on my network": refused before any request
    let lan = ns_source(&env, &base, "v1", Some(TOKEN), false).await?;
    let run = poll(&env, lan.id).await?;
    assert_eq!(run["status"], "error");
    assert!(run["error"].as_str().unwrap_or_default().contains("private"), "{run}");
    // v3 without a token cannot ask for a JWT
    let bare = ns_source(&env, &base, "v3", None, true).await?;
    let run = poll(&env, bare.id).await?;
    assert!(
        run["error"].as_str().unwrap_or_default().contains("needs a read token"),
        "{run}"
    );
    let rows = env
        .count(
            "select count(*) from wearable_sample where profile_id = $1",
            env.fam.profile("lucia"),
        )
        .await?;
    assert_eq!(rows, 0);
    let _ = nightscout::DEFAULT_POLL_SECONDS;
    Ok(())
}

#[tokio::test]
async fn a_backfilled_reading_older_than_the_cursor_is_still_pulled() -> R {
    let env = Env::new().await?;
    let lucia = env.fam.profile("lucia");
    let now = Utc::now().duration_trunc(Duration::minutes(5))?;
    let readings = synthetic::cgm_readings(now - Duration::hours(12), 1, 5, 7);
    let readings: Vec<_> = readings.into_iter().filter(|r| r.0 < now).collect();
    // a Bluetooth gap: three readings two hours ago reach the site only after the first poll
    let gap = now - Duration::hours(2);
    let (late, on_time): (Vec<_>, Vec<_>) = readings
        .iter()
        .partition(|r| r.0 >= gap && r.0 < gap + Duration::minutes(15));
    assert_eq!(late.len(), 3);
    let fake = fake_with(&on_time, "xDrip-DexcomG6");
    let base = serve(fake.clone()).await?;
    let source = ns_source(&env, &base, "v1", Some(TOKEN), true).await?;
    let first = poll(&env, source.id).await?;
    assert_eq!(first["items_new"], json!(on_time.len()), "{first}");
    fake.lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .extend(late.iter().map(|(t, v)| (t.timestamp_millis(), *v, "xDrip-DexcomG6")));
    let second = poll(&env, source.id).await?;
    assert_eq!(second["items_new"], json!(3), "{second}");
    let rows = env
        .count("select count(*) from wearable_sample where profile_id = $1", lucia)
        .await?;
    assert_eq!(rows, readings.len() as i64);
    Ok(())
}

#[tokio::test]
async fn a_future_dated_entry_does_not_stop_later_imports() -> R {
    let env = Env::new().await?;
    let lucia = env.fam.profile("lucia");
    let now = Utc::now().duration_trunc(Duration::minutes(5))?;
    let readings = synthetic::cgm_readings(now - Duration::hours(6), 1, 5, 9);
    let (old, new): (Vec<_>, Vec<_>) = readings
        .iter()
        .filter(|r| r.0 < now + Duration::hours(1))
        .partition(|r| r.0 < now - Duration::hours(1));
    // an uploader with its clock a day ahead writes one reading dated tomorrow
    let mut first_batch = old.clone();
    first_batch.push((now + Duration::days(1), 140.0));
    let fake = fake_with(&first_batch, "xDrip-DexcomG6");
    let base = serve(fake.clone()).await?;
    let source = ns_source(&env, &base, "v1", Some(TOKEN), true).await?;
    let first = poll(&env, source.id).await?;
    assert_eq!(
        first["items_new"],
        json!(old.len()),
        "the future reading waits: {first}"
    );
    {
        let mut f = fake.lock().unwrap_or_else(|e| e.into_inner());
        f.entries.extend(
            new.iter()
                .filter(|r| r.0 <= Utc::now())
                .map(|(t, v)| (t.timestamp_millis(), *v, "xDrip-DexcomG6")),
        );
    }
    let arrived = new.iter().filter(|r| r.0 <= Utc::now()).count();
    assert!(arrived > 0);
    let second = poll(&env, source.id).await?;
    assert_eq!(second["items_new"], json!(arrived), "{second}");
    let future = env
        .count(
            "select count(*) from wearable_sample where profile_id = $1 and t > now() + interval '1 hour'",
            lucia,
        )
        .await?;
    assert_eq!(future, 0);
    Ok(())
}
