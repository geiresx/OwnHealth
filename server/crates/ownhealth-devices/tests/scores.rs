//! Vendor scores are not measurements (migration 1220): a score-like series key never reaches
//! wearable_sample, and scores live in vendor_score, tagged computed_by 'provider'. Synthetic data for
//! Sam Rivera.

mod common;

use chrono::{TimeZone, Utc};
use common::{Env, R};
use ownhealth_devices::scores::{self, CATEGORIES, Score};
use ownhealth_devices::store::{self, Conflict, Sample};
use ownhealth_ingest_structured::wearables;
use serde_json::json;

/// Keys a connector could use for a vendor's score: each one is refused by wearable_sample.
const SCORE_LIKE: [&str; 30] = [
    "body_battery",
    "bodybattery",
    "BodyBattery",
    "body_Battery",
    "bodyBattery",
    "body-battery",
    "stresslevel",
    "stress.level",
    "STRESS",
    "readinessScore",
    "Readiness",
    "sleepScore",
    "nightly_recharge",
    "ans_charge",
    "resilience",
    "hrv_status",
    "training_status",
    "sleep_performance",
    "garmin_body_battery",
    "stress",
    "stress_level",
    "readiness",
    "readiness_score",
    "training_readiness",
    "recovery",
    "recovery_score",
    "sleep_score",
    "activity_score",
    "strain",
    "day_strain",
];

/// Measurements that look close: each one is still accepted.
const MEASUREMENTS: [&str; 4] = ["hr_recovery", "sleep_deep", "stride_length", "hrv_rmssd_night"];

fn sample(metric: &'static str, source: i16) -> Sample {
    Sample {
        metric,
        t: Utc.with_ymd_and_hms(2026, 11, 2, 7, 0, 0).single().unwrap_or_default(),
        t_end: None,
        v: 64.0,
        tz: Some(60),
        source,
    }
}

#[tokio::test]
async fn a_score_like_series_key_never_reaches_wearable_sample() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let mut c = env.db.pool.acquire().await?;
    let src = store::device_source(&mut c, sam, "garmin", Some("Forerunner"), None, None).await?;
    for key in SCORE_LIKE {
        let err = store::write(&mut c, sam, None, vec![sample(key, src)], Conflict::Keep).await;
        let msg = format!("{err:?}");
        assert!(
            err.is_err() && msg.contains("wearable_sample_not_a_vendor_score"),
            "{key} reached wearable_sample: {msg}"
        );
    }
    let n = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0, "no vendor score is stored as a sample");
    let mut keys: Vec<&'static str> = wearables::METRICS.iter().map(|m| m.0).collect();
    keys.extend(MEASUREMENTS);
    for key in &keys {
        store::write(&mut c, sam, None, vec![sample(key, src)], Conflict::Keep).await?;
    }
    let n = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(n, keys.len() as i64, "every measurement key is still stored");
    Ok(())
}

#[test]
fn is_score_key_is_the_same_rule_as_the_check() {
    for key in SCORE_LIKE {
        assert!(scores::is_score_key(key), "{key}");
    }
    for key in wearables::METRICS.iter().map(|m| m.0).chain(MEASUREMENTS) {
        assert!(!scores::is_score_key(key), "{key}");
    }
}

#[tokio::test]
async fn the_category_check_lists_exactly_the_rust_categories() -> R {
    let env = Env::new().await?;
    let def: String = sqlx::query_scalar(
        "select pg_get_constraintdef(oid) from pg_constraint where conrelid = 'vendor_score'::regclass \
         and contype = 'c' and pg_get_constraintdef(oid) like '%category%'",
    )
    .fetch_one(&env.db.pool)
    .await?;
    let mut listed: Vec<&str> = def.split('\'').skip(1).step_by(2).collect();
    listed.sort_unstable();
    let mut ours = CATEGORIES.to_vec();
    ours.sort_unstable();
    assert_eq!(listed, ours, "{def}");
    Ok(())
}

fn score(category: &str, day: u32, value: f64, qualifier: Option<&str>) -> Score {
    Score {
        provider: "oura".into(),
        category: category.into(),
        t: Utc
            .with_ymd_and_hms(2026, 11, day, 6, 30, 0)
            .single()
            .unwrap_or_default(),
        value,
        qualifier: qualifier.map(String::from),
        components: Some(json!({"hrv_balance": 78, "resting_heart_rate": 91})),
    }
}

#[tokio::test]
async fn scores_are_stored_apart_tagged_by_provider_and_a_repull_updates_in_place() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?.id;
    let mut c = env.db.pool.acquire().await?;
    let batch = vec![
        score("readiness", 1, 82.0, Some("Good")),
        score("readiness", 2, 74.0, None),
        score("sleep", 2, 88.0, Some("Optimal")),
    ];
    assert_eq!(scores::write(&mut c, sam, src, &batch).await?.written, 3);
    // the same pull again changes nothing; a new value for one morning replaces it
    assert_eq!(scores::write(&mut c, sam, src, &batch).await?.written, 0);
    let again = vec![score("readiness", 2, 76.0, Some("Fair"))];
    assert_eq!(scores::write(&mut c, sam, src, &again).await?.written, 1);
    let since = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).single().unwrap_or_default();
    let rows = scores::list(&mut c, sam, since).await?;
    let got: Vec<(&str, f64, Option<&str>, &str)> = rows
        .iter()
        .map(|r| {
            (
                r.category.as_str(),
                r.value,
                r.qualifier.as_deref(),
                r.computed_by.as_str(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("readiness", 82.0, Some("Good"), "provider"),
            ("readiness", 76.0, Some("Fair"), "provider"),
            ("sleep", 88.0, Some("Optimal"), "provider"),
        ]
    );
    assert!(rows.iter().all(|r| r.provider == "oura"));
    // nothing went into the measurements
    let n = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(n, 0);
    // an unknown category is refused
    let bad = vec![score("mood", 3, 5.0, None)];
    assert!(scores::write(&mut c, sam, src, &bad).await.is_err());
    Ok(())
}

fn garmin_morning(value: f64) -> Score {
    Score {
        provider: "garmin".into(),
        category: "body_battery".into(),
        t: Utc.with_ymd_and_hms(2026, 11, 20, 8, 0, 0).single().unwrap_or_default(),
        value,
        qualifier: None,
        components: None,
    }
}

/// Two connections that both bring Garmin's Body Battery for the same moment keep a row each, and the
/// read names the connection of every row.
#[tokio::test]
async fn two_connections_with_the_same_score_keep_a_row_each() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let gb = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let hae = env.source("health_auto_export", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    scores::write(&mut c, sam, gb.id, &[garmin_morning(62.0)]).await?;
    scores::write(&mut c, sam, hae.id, &[garmin_morning(64.0)]).await?;
    let rows: Vec<(Option<uuid::Uuid>, f64)> =
        sqlx::query_as("select source_id, value from vendor_score where profile_id = $1 order by value")
            .bind(sam)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(rows, vec![(Some(gb.id), 62.0), (Some(hae.id), 64.0)]);
    Ok(())
}

/// A vendor quirk in one score never fails the batch: a value that is not a number is dropped, an
/// empty or non-object `components` is left out, a long qualifier is cut.
#[tokio::test]
async fn one_vendor_quirk_does_not_fail_the_write() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    let mut nan = score("readiness", 1, f64::NAN, None);
    nan.t = Utc.with_ymd_and_hms(2026, 11, 1, 5, 0, 0).single().unwrap_or_default();
    let mut null_parts = score("readiness", 2, 74.0, None);
    null_parts.components = Some(serde_json::Value::Null);
    let mut list_parts = score("sleep", 2, 80.0, None);
    list_parts.components = Some(json!([1, 2]));
    let long = score("activity", 2, 90.0, Some(&"Very good ".repeat(10)));
    let batch = vec![
        nan,
        null_parts,
        list_parts,
        long,
        score("sleep", 3, 88.0, Some("Optimal")),
    ];
    let w = scores::write(&mut c, sam, src.id, &batch).await;
    assert!(w.is_ok(), "{w:?}");
    assert_eq!(
        w?,
        scores::Written { written: 4, dropped: 1 },
        "the value that is not a number is dropped and counted"
    );
    let rows: Vec<(String, f64, Option<serde_json::Value>, Option<String>)> = sqlx::query_as(
        "select category, value, components, qualifier from vendor_score where profile_id = $1 order by t, category",
    )
    .bind(sam)
    .fetch_all(&mut *c)
    .await?;
    let got: Vec<(&str, f64, bool, usize)> = rows
        .iter()
        .map(|(cat, v, comp, q)| {
            (
                cat.as_str(),
                *v,
                comp.is_some(),
                q.as_deref().map_or(0, |q| q.chars().count()),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("activity", 90.0, true, 80),
            ("readiness", 74.0, false, 0),
            ("sleep", 80.0, false, 0),
            ("sleep", 88.0, true, 7)
        ]
    );
    Ok(())
}

/// The read is one person's: Sam never sees Lucía's scores, and Lucía never sees Sam's.
#[tokio::test]
async fn the_read_returns_only_the_profiles_own_scores() -> R {
    let env = Env::new().await?;
    let (sam, lucia) = (env.fam.profile("sam"), env.fam.profile("lucia"));
    let mut c = env.db.pool.acquire().await?;
    let gb = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let gl = env.source("gadgetbridge", Some("lucia"), json!({}), None).await?;
    scores::write(&mut c, sam, gb.id, &[garmin_morning(62.0)]).await?;
    scores::write(
        &mut c,
        lucia,
        gl.id,
        &[garmin_morning(35.0), score("readiness", 2, 70.0, None)],
    )
    .await?;
    let since = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).single().unwrap_or_default();
    let values = |rows: Vec<scores::Stored>| rows.iter().map(|r| r.value).collect::<Vec<_>>();
    assert_eq!(values(scores::list(&mut c, sam, since).await?), vec![62.0]);
    assert_eq!(values(scores::list(&mut c, lucia, since).await?), vec![35.0, 70.0]);
    Ok(())
}

/// Removing a connection keeps the scores it brought; they no longer name it.
#[tokio::test]
async fn removing_the_connection_keeps_its_scores() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let src = env.source("gadgetbridge", Some("sam"), json!({}), None).await?;
    let mut c = env.db.pool.acquire().await?;
    scores::write(&mut c, sam, src.id, &[garmin_morning(62.0)]).await?;
    assert!(ownhealth_core::sources::delete_source(&mut c, src.id).await?);
    let rows: Vec<(Option<uuid::Uuid>, f64)> =
        sqlx::query_as("select source_id, value from vendor_score where profile_id = $1")
            .bind(sam)
            .fetch_all(&mut *c)
            .await?;
    assert_eq!(rows, vec![(None, 62.0)]);
    Ok(())
}

/// The CHECK on wearable_sample, as Postgres prints it.
async fn guard(env: &Env) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar(
        "select pg_get_constraintdef(oid) from pg_constraint where conrelid = 'wearable_sample'::regclass \
         and conname = 'wearable_sample_not_a_vendor_score'",
    )
    .fetch_one(&env.db.pool)
    .await?)
}

#[tokio::test]
async fn the_guard_lists_exactly_the_rust_score_words() -> R {
    let env = Env::new().await?;
    let def = guard(&env).await?;
    let group = def
        .split('\'')
        .find(|p| p.starts_with('(') && p.contains('|'))
        .unwrap_or_default();
    let mut listed: Vec<&str> = group.trim_matches(|c| c == '(' || c == ')').split('|').collect();
    listed.sort_unstable();
    let mut ours = scores::SCORE_WORDS.to_vec();
    ours.sort_unstable();
    assert_eq!(listed, ours, "{def}");
    Ok(())
}

/// The CHECK and `is_sample_key` give the same verdict on one shared list of names: every key the
/// slice knows, each score word plain, inside a key, at either end, in capitals and joined, and keys
/// that are not plain.
#[tokio::test]
async fn the_guard_and_is_sample_key_agree_on_every_name() -> R {
    let env = Env::new().await?;
    let def = guard(&env).await?;
    let expr = def.strip_prefix("CHECK ").unwrap_or(&def);
    let mut names: Vec<String> = SCORE_LIKE
        .iter()
        .chain(&MEASUREMENTS)
        .copied()
        .chain(wearables::METRICS.iter().map(|m| m.0))
        .map(String::from)
        .collect();
    for w in scores::SCORE_WORDS.iter().chain(&["recovery"]) {
        let cap = format!("{}{}", w[..1].to_uppercase(), &w[1..]);
        names.extend([
            w.to_string(),
            format!("x_{w}"),
            format!("{w}_x"),
            format!("x{w}"),
            format!("{w}2"),
            w.to_uppercase(),
            format!("day{cap}"),
            format!("day-{w}"),
        ]);
    }
    names.extend(
        [
            "",
            "Hr",
            "1hr",
            "_hr",
            "hr-rest",
            "hr.rest",
            "hr rest",
            "hr_é",
            "hr_recovery",
            "heart_rate_recovery",
        ]
        .map(String::from),
    );
    let q = format!("select {expr} from (select $1::text as metric) s");
    for name in &names {
        let db: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(q.clone()))
            .bind(name)
            .fetch_one(&env.db.pool)
            .await?;
        assert_eq!(db, scores::is_sample_key(name), "{name:?}: the CHECK says {db}");
    }
    // and the list holds both verdicts
    assert!(names.iter().any(|n| scores::is_sample_key(n)) && names.iter().any(|n| !scores::is_sample_key(n)));
    Ok(())
}
