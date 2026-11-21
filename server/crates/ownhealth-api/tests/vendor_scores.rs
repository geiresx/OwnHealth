//! `GET /profiles/{p}/vendor-scores` (migration 1220): the vendor's own scores, labelled with the
//! provider and kept apart from the measurements, and deleted with the profile. Synthetic data for
//! Sam Rivera.

use axum::body::Body;
use chrono::{Duration, Utc};
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use ownhealth_devices::scores::{self, Score};
use serde_json::{Value, json};
use tower::ServiceExt;

type R = anyhow::Result<()>;

struct App {
    db: TestDb,
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
        db,
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

/// Sam's watch and ring, brought by one Gadgetbridge connection: Garmin Body Battery and Oura
/// readiness over the last days, and one Oura readiness from 60 days ago.
async fn seed(a: &App) -> anyhow::Result<uuid::Uuid> {
    let now = Utc::now();
    let s = |provider: &str, category: &str, days_ago: i64, value: f64, qualifier: Option<&str>| Score {
        provider: provider.into(),
        category: category.into(),
        t: now - Duration::days(days_ago),
        value,
        qualifier: qualifier.map(String::from),
        components: None,
    };
    let batch = vec![
        s("oura", "readiness", 60, 70.0, None),
        s("oura", "readiness", 2, 81.0, Some("Good")),
        s("oura", "readiness", 1, 84.0, Some("Optimal")),
        s("garmin", "body_battery", 1, 62.0, None),
    ];
    let mut c = a.rt.pool.acquire().await?;
    let src = ownhealth_core::sources::create_source(
        &mut c,
        &ownhealth_core::sources::NewSource {
            profile_id: Some(a.family.profile("sam")),
            ..ownhealth_core::sources::NewSource::new(a.family.household_id, "gadgetbridge", "Sam's Gadgetbridge")
        },
    )
    .await?;
    scores::write(&mut c, a.family.profile("sam"), src, &batch).await?;
    Ok(src)
}

#[tokio::test]
async fn the_vendor_scores_come_back_labelled_with_their_provider_and_apart_from_measurements() -> R {
    let a = app().await?;
    let src = seed(&a).await?;
    let (s, body) = get(&a, "sam", "/profiles/sam/vendor-scores").await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["days"], json!(30));
    let rows = body["scores"].as_array().cloned().unwrap_or_default();
    let got: Vec<(String, String, f64, Value)> = rows
        .iter()
        .map(|r| {
            (
                r["provider"].as_str().unwrap_or_default().to_string(),
                r["category"].as_str().unwrap_or_default().to_string(),
                r["value"].as_f64().unwrap_or_default(),
                r["qualifier"].clone(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("garmin".into(), "body_battery".into(), 62.0, Value::Null),
            ("oura".into(), "readiness".into(), 81.0, json!("Good")),
            ("oura".into(), "readiness".into(), 84.0, json!("Optimal")),
        ],
        "the 60-day-old score is outside the default 30 days"
    );
    assert!(rows.iter().all(|r| r["computed_by"] == json!("provider")));
    // each score names the connection that brought it
    assert!(
        rows.iter()
            .all(|r| r["source"] == json!({"id": src, "label": "Sam's Gadgetbridge"})),
        "{body}"
    );
    let (_, year) = get(&a, "sam", "/profiles/sam/vendor-scores?days=365").await?;
    assert_eq!(year["scores"].as_array().map(Vec::len), Some(4));
    // the measurements never see them
    let (_, w) = get(&a, "sam", "/profiles/sam/wearables/summary").await?;
    assert_eq!(w["metrics"], json!([]), "{w}");
    // one audit row per read
    let n: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where tool = 'wearables.vendor_scores' and resource_type = 'vendor_score' \
         and profile_id = $1",
    )
    .bind(a.family.profile("sam"))
    .fetch_one(&a.db.pool)
    .await?;
    assert_eq!(n, 2);
    Ok(())
}

#[tokio::test]
async fn a_window_outside_one_to_365_days_is_refused() -> R {
    let a = app().await?;
    for bad in ["0", "366", "x", "-3"] {
        let (s, body) = get(&a, "sam", &format!("/profiles/sam/vendor-scores?days={bad}")).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "days={bad}: {body}");
    }
    let (s, body) = get(&a, "sam", "/profiles/sam/vendor-scores?days=7").await?;
    assert_eq!((s, body["scores"].clone()), (StatusCode::OK, json!([])));
    // a viewer without read:wearables
    let (s, _) = get(&a, "alex", "/profiles/sam/vendor-scores").await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn deleting_the_profile_deletes_its_vendor_scores() -> R {
    let a = app().await?;
    seed(&a).await?;
    let sam = a.family.profile("sam");
    let mut tx = a.db.pool.begin().await?;
    let deleted = ownhealth_auth::household::delete_profile(&mut tx, sam).await?;
    tx.commit().await?;
    assert!(deleted.rows.contains(&("vendor_score".into(), 4)), "{:?}", deleted.rows);
    let n: i64 = sqlx::query_scalar("select count(*) from vendor_score where profile_id = $1")
        .bind(sam)
        .fetch_one(&a.db.pool)
        .await?;
    assert_eq!(n, 0);
    Ok(())
}
