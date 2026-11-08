//! Lane C routes (`tests/imports/test_records_api.py`, the wearables cases): the summary and one
//! metric's series, on a synthetic Apple Health export of Lucía's.

use std::collections::BTreeSet;
use std::io::Cursor;

use axum::body::Body;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::testing::TestDb;
use ownhealth_ingest_structured::apple_health::import_apple_health;
use ownhealth_ingest_structured::apple_health::synthetic::{ExportSpec, write_export_to};
use serde_json::Value;
use tower::ServiceExt;

type R = anyhow::Result<()>;

struct App {
    _db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    family: Family,
}

async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    Ok(App {
        _db: db,
        _dir: dir,
        rt,
        family,
    })
}

async fn get(a: &App, who: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    let req = Request::get(uri)
        .header(header::COOKIE, format!("{COOKIE}={secret}"))
        .body(Body::empty())?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 26).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

async fn import_lucia(a: &App) -> R {
    let spec = ExportSpec {
        days: 40,
        end: NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default(),
        birth_date: NaiveDate::from_ymd_opt(2017, 6, 21).unwrap_or_default(),
        first_name: "Lucía".into(),
        ..ExportSpec::default()
    };
    let mut buf = Cursor::new(Vec::new());
    write_export_to(&mut buf, &spec)?;
    let data = buf.into_inner();
    let sha = a.rt.blobs.put_bytes(&data)?;
    let mut tx = a.rt.pool.begin().await?;
    let doc = create_document(
        &mut tx,
        &NewDocument {
            filename: Some("export.zip"),
            ..NewDocument::upload(
                a.family.household_id,
                Some(a.family.profile("lucia")),
                &sha,
                "application/zip",
                data.len() as i64,
            )
        },
    )
    .await?;
    import_apple_health(&mut tx, &a.rt.blobs, doc.id).await?;
    tx.commit().await?;
    Ok(())
}

fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

#[tokio::test]
async fn wearables_summary_and_series() -> R {
    let a = app().await?;
    import_lucia(&a).await?;
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/summary?as_of=2026-11-07&days=30&lang=en",
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let metrics = body["metrics"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = metrics.iter().filter_map(|m| m["metric"].as_str()).collect();
    assert_eq!(names[..4], ["sleep_asleep", "hrv_sdnn", "hr_resting", "steps"]);
    let by = |k: &str| metrics.iter().find(|m| m["metric"] == k).cloned().unwrap_or_default();
    let steps = by("steps");
    assert_eq!(
        (&steps["label"], &steps["source"]),
        (&Value::from("Steps"), &Value::from("Apple Watch"))
    );
    let series = steps["series"].as_array().cloned().unwrap_or_default();
    assert_eq!(series.len(), 30);
    assert_eq!(series[29]["date"], "2026-11-07");
    assert_eq!(steps["latest"], series[29]["value"]);
    let total: f64 = series.iter().map(|p| p["value"].as_f64().unwrap_or_default()).sum();
    assert_eq!(steps["avg_30d"].as_f64(), Some(round1(total / 30.0)));
    assert_eq!(by("hr_resting")["coverage_pct"].as_f64(), Some(100.0));
    let devices: BTreeSet<&str> = body["devices"]
        .as_array()
        .map(|d| d.iter().filter_map(|x| x["device"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(devices, BTreeSet::from(["Apple Watch", "iPhone"]));
    let (_, es) = get(&a, "sam", "/profiles/lucia/wearables/summary?as_of=2026-11-07").await?;
    assert_eq!(es["metrics"][0]["label"], "Duración del sueño");
    let (_, r) = get(&a, "sam", "/profiles/lucia/wearables/steps?window=P7D&as_of=2026-11-07").await?;
    let pts = r["points"].as_array().cloned().unwrap_or_default();
    let devs: BTreeSet<&str> = pts.iter().filter_map(|p| p["device"].as_str()).collect();
    assert_eq!(pts.len(), 7);
    assert_eq!(devs, BTreeSet::from(["Apple Watch"]));
    assert_eq!(r["priority"].as_array().map(Vec::len), Some(2));
    let (_, hourly) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/steps?window=P1D&resolution=hour&as_of=2026-11-07",
    )
    .await?;
    let hourly = hourly["points"].as_array().cloned().unwrap_or_default();
    assert_eq!(hourly.len(), 16);
    let t0 = DateTime::parse_from_rfc3339(hourly[0]["t"].as_str().unwrap_or_default())?;
    assert_eq!(
        t0,
        Utc.with_ymd_and_hms(2026, 11, 7, 6, 0, 0).single().unwrap_or_default()
    );
    let (s, _) = get(&a, "sam", "/profiles/lucia/wearables/steps?window=P1Y&resolution=hour").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // a viewer without read:wearables
    let (s, _) = get(&a, "alex", "/profiles/sam/wearables/summary").await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let n: i64 =
        sqlx::query_scalar("select count(*) from audit_event where profile_id = $1 and tool like 'wearables.%'")
            .bind(a.family.profile("lucia"))
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!(n, 4); // the refused hourly request read nothing
    Ok(())
}

#[tokio::test]
async fn a_wearables_window_too_large_for_the_calendar_is_capped() -> R {
    let a = app().await?;
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/steps?window=P9999Y&as_of=2026-11-07",
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = get(
        &a,
        "sam",
        "/profiles/lucia/wearables/steps?window=P30D&as_of=0001-01-02",
    )
    .await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &Value::from("bad_input")),
        "{body}"
    );
    Ok(())
}
