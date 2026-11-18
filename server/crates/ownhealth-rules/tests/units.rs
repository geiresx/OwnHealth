//! Units and wearable values (0.1.1 tests/state/test_mixed_units.py, test_wearable_markers.py,
//! test_dashboard_band.py): never compare across units, sleep is the nightly total, and the
//! dashboard band is drawn in the spark's unit.

mod common;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate};
use ownhealth_core::access::all_scopes;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::testing::TestDb;
use ownhealth_normalize::{RowIn, normalize_row};
use ownhealth_rules::engine::{Point, rcv, trend, wearable_marker};
use ownhealth_rules::{dashboard, load_pack};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

type R = anyhow::Result<()>;

fn dec(s: &str) -> Decimal {
    s.parse().unwrap_or_default()
}

fn pt(day: &str, value: &str, canonical: Option<&str>) -> Point {
    Point {
        id: Uuid::new_v4(),
        date: NaiveDate::parse_from_str(day, "%Y-%m-%d").unwrap_or_default(),
        value: Some(dec(value)),
        canonical: canonical.map(dec),
        canonical_unit: Some("mmol/L".into()),
        printed_value: value.into(),
        unit: Some("mg/dL".into()),
        unit_ucum: None,
        ref_text: None,
        ref_low: None,
        ref_high: None,
        flag: None,
        loinc: None,
        method: None,
        lab: None,
        src: "src_x".into(),
        src_label: None,
        src_kind: "document_region".into(),
        derivation: "test".into(),
    }
}

fn rule(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

#[test]
fn a_trend_never_mixes_canonical_and_printed_units() {
    let t = rule(json!({"id": "t", "points": 3, "direction": "up", "min_slope_per_year": "0.1"}));
    // 2.0 -> 2.1 mmol/L, then 90 mg/dL the normalizer could not convert: no 40-fold "rise"
    let pts = [
        pt("2024-01-01", "77", Some("2.0")),
        pt("2025-01-01", "81", Some("2.1")),
        pt("2026-01-01", "90", None),
    ];
    assert!(trend(&t, &pts).is_none());
    // same unit throughout still trends
    let pts = [
        pt("2024-01-01", "77", Some("2.0")),
        pt("2025-01-01", "81", Some("2.1")),
        pt("2026-01-01", "90", Some("2.33")),
    ];
    assert_eq!(trend(&t, &pts).map(|f| f["reason"].clone()), Some(json!("TREND_UP")));
}

#[test]
fn rcv_compares_the_unit_comparable_is_in() {
    let r = rule(json!({"id": "r", "cv_i": "6", "cv_a": "2"}));
    assert!(rcv(&r, &[pt("2025-01-01", "77", Some("2.0")), pt("2026-01-01", "90", None)]).is_none());
    // and a real change in one unit is flagged
    assert!(
        rcv(
            &r,
            &[pt("2025-01-01", "77", Some("2.0")), pt("2026-01-01", "90", Some("2.6"))]
        )
        .is_some()
    );
}

/// An RCV sentence names one unit, so its two numbers are in that unit: a creatinine printed in mg/dL
/// and then in umol/L reads in the unit the change was judged in, never "from 0.91 to 110 umol/L".
#[test]
fn an_rcv_flag_shows_both_values_in_the_unit_it_names() {
    let r = rule(json!({"id": "r", "cv_i": "4.5", "cv_a": "3"}));
    let unit = |mut p: Point, printed: &str, canonical_unit: &str| {
        p.unit = Some(printed.into());
        p.canonical_unit = Some(canonical_unit.into());
        p
    };
    let before = unit(pt("2025-01-01", "0.91", Some("80.444")), "mg/dL", "umol/L");
    let after = unit(pt("2026-01-01", "110", Some("110")), "umol/L", "umol/L");
    let f = rcv(&r, &[before, after]).unwrap_or_default();
    assert_eq!(
        (f["prev"].clone(), f["value"].clone(), f["unit"].clone()),
        (json!("80.44"), json!("110"), json!("umol/L")),
        "{f:?}"
    );
    // the same printed unit on both keeps the numbers as the lab printed them
    let before = unit(pt("2025-01-01", "0.91", Some("80.444")), "mg/dL", "umol/L");
    let after = unit(pt("2026-01-01", "1.24", Some("109.62")), "mg/dL", "umol/L");
    let f = rcv(&r, &[before, after]).unwrap_or_default();
    assert_eq!(
        (f["prev"].clone(), f["value"].clone(), f["unit"].clone()),
        (json!("0.91"), json!("1.24"), json!("mg/dL")),
        "{f:?}"
    );
}

/// A night split into sleep stages is the day's total, never the mean of its stages.
#[test]
fn sleep_is_the_nightly_total_of_its_stages() -> R {
    let pack = load_pack()?;
    let spec = pack
        .systems
        .iter()
        .flat_map(|s| s.markers.iter())
        .find(|m| m["metric"] == "sleep_asleep")
        .cloned()
        .unwrap_or_default();
    let as_of = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default();
    // three stages a night: 120 + 60 + 240 minutes asleep, mean 140
    let days: Vec<_> = (0..30)
        .rev()
        .map(|i| (as_of - Duration::days(i), Some(140.0), Some(420.0)))
        .collect();
    let w = wearable_marker(&spec, 1, &days, as_of);
    assert_eq!(w.and_then(|w| w.value), Some(Decimal::from(420)));
    Ok(())
}

fn at(day: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(&format!("{day}T00:00:00+00:00")).unwrap_or_default()
}

async fn normalized(
    conn: &mut PgConnection,
    pid: Uuid,
    v: &str,
    u: &str,
    rr: &str,
    day: &str,
) -> anyhow::Result<ObservationIn> {
    normalized_as(conn, pid, "Glucosa", v, u, rr, day).await
}

async fn normalized_as(
    conn: &mut PgConnection,
    pid: Uuid,
    name: &str,
    v: &str,
    u: &str,
    rr: &str,
    day: &str,
) -> anyhow::Result<ObservationIn> {
    let row = RowIn {
        profile_id: pid,
        printed_name: name,
        printed_value: v,
        printed_unit: Some(u),
        printed_range: Some(rr),
        printed_flag: None,
        effective_at: at(day),
        lang: "es",
        specimen: None,
        provider: None,
        loinc: None,
        method: None,
        document_id: None,
    };
    Ok(normalize_row(conn, &row).await?.observation)
}

async fn add(conn: &mut PgConnection, obs: &ObservationIn, pid: Uuid) -> R {
    let src = SrcIn {
        profile_id: Some(pid),
        label: Some("LabNorte".into()),
        ..SrcIn::new("manual")
    };
    commit_observation(conn, obs, &SrcRef::New(src), CommitOptions::new("import")).await?;
    Ok(())
}

fn rows_of<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            if m.get("alias_group").and_then(Value::as_str) == Some(key) && m.contains_key("band") {
                out.push(v);
            }
            m.values().for_each(|x| rows_of(x, key, out));
        }
        Value::Array(a) => a.iter().for_each(|x| rows_of(x, key, out)),
        _ => {}
    }
}

#[tokio::test]
async fn si_range_is_converted_to_the_spark_unit() -> R {
    common::install_fake_genome();
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let pid = create_rivera_family(&mut tx, "Rivera").await?.profile("alex");
    let a = normalized(&mut tx, pid, "92", "mg/dL", "70-110", "2026-03-01").await?;
    add(&mut tx, &a, pid).await?;
    let b = normalized(&mut tx, pid, "5,3", "mmol/L", "3,9-6,1", "2026-09-01").await?;
    add(&mut tx, &b, pid).await?;
    let d = dashboard(
        &mut tx,
        pid,
        NaiveDate::from_ymd_opt(2026, 10, 1).unwrap_or_default(),
        "en",
        &all_scopes(),
    )
    .await?;
    let mut rows = Vec::new();
    rows_of(&d, "glucose", &mut rows);
    assert!(!rows.is_empty());
    for r in rows {
        assert_eq!(r["spark"][0], json!(92.0));
        let (low, high) = (
            r["band"][0].as_f64().unwrap_or_default(),
            r["band"][1].as_f64().unwrap_or_default(),
        );
        assert!(
            69.0 < low && low < 71.0 && 109.0 < high && high < 111.0,
            "{}",
            r["band"]
        );
    }
    Ok(())
}

/// 10^3/µL and 10^9/L are the same count: a new report in the other notation keeps the history.
#[tokio::test]
async fn the_spark_keeps_history_across_equivalent_units() -> R {
    common::install_fake_genome();
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let pid = create_rivera_family(&mut tx, "Rivera").await?.profile("alex");
    // each value kept in its own canonical notation, as the demo seed and older imports store it
    for (v, u, ucum, day) in [
        ("244", "x10^9/L", "10*9/L", "2026-03-01"),
        ("276", "x10^3/µL", "10*3/uL", "2026-09-01"),
    ] {
        let mut o = normalized_as(&mut tx, pid, "Plaquetas", v, u, "150-400", day).await?;
        o.value_canonical = Some(dec(v));
        o.unit_canonical = Some(ucum.into());
        add(&mut tx, &o, pid).await?;
    }
    let d = dashboard(
        &mut tx,
        pid,
        NaiveDate::from_ymd_opt(2026, 10, 1).unwrap_or_default(),
        "en",
        &all_scopes(),
    )
    .await?;
    let mut rows = Vec::new();
    rows_of(&d, "platelets", &mut rows);
    assert!(!rows.is_empty());
    for r in rows {
        assert_eq!(r["spark"], json!([244.0, 276.0]));
    }
    Ok(())
}

/// 92 mg/dL, then 5,3 mmol/L left unconverted: not a 94% drop.
#[tokio::test]
async fn changes_never_compare_a_point_the_normalizer_could_not_convert() -> R {
    common::install_fake_genome();
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let pid = create_rivera_family(&mut tx, "Rivera").await?.profile("alex");
    let a = normalized(&mut tx, pid, "92", "mg/dL", "70-110", "2026-03-01").await?;
    add(&mut tx, &a, pid).await?;
    let mut b = normalized(&mut tx, pid, "5,3", "mmol/L", "3,9-6,1", "2026-09-01").await?;
    b.value_canonical = None;
    b.unit_canonical = None;
    add(&mut tx, &b, pid).await?;
    let d = dashboard(
        &mut tx,
        pid,
        NaiveDate::from_ymd_opt(2026, 10, 1).unwrap_or_default(),
        "en",
        &all_scopes(),
    )
    .await?;
    let down: Vec<&Value> = d["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["direction"] == "down")
        .collect();
    assert!(down.is_empty(), "{down:?}");
    Ok(())
}

/// Two lab reports, then one value typed by hand: the changes still compare the two reports.
#[tokio::test]
async fn one_value_entered_by_hand_keeps_the_changes_since_the_last_report() -> R {
    common::install_fake_genome();
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let pid = create_rivera_family(&mut tx, "Rivera").await?.profile("alex");
    for (v, day) in [("92", "2026-03-01"), ("130", "2026-09-01")] {
        let o = normalized(&mut tx, pid, v, "mg/dL", "70-110", day).await?;
        let src = SrcIn {
            profile_id: Some(pid),
            label: Some("LabNorte".into()),
            import_ref: Some(format!("labnorte-{day}")),
            ..SrcIn::new("fhir_import")
        };
        commit_observation(&mut tx, &o, &SrcRef::New(src), CommitOptions::new("import")).await?;
    }
    let by_hand = normalized_as(&mut tx, pid, "Plaquetas", "250", "x10^3/µL", "150-400", "2026-09-20").await?;
    add(&mut tx, &by_hand, pid).await?;
    let d = dashboard(
        &mut tx,
        pid,
        NaiveDate::from_ymd_opt(2026, 10, 1).unwrap_or_default(),
        "en",
        &all_scopes(),
    )
    .await?;
    assert_eq!(d["changes_vs"], json!("2026-03-01"), "{}", d["changes"]);
    assert!(
        d["changes"].as_array().is_some_and(|c| !c.is_empty()),
        "{}",
        d["changes"]
    );
    Ok(())
}
