//! Budget (0.1.1 tests/state/test_perf.py): the status of a profile with 5 years of labs
//! (40 analytes x 60 monthly draws, 2400 values) builds and renders in under 300 ms at p95.

mod common;

use std::time::Instant;

use chrono::{NaiveDate, TimeZone, Utc};
use ownhealth_core::access::{ProfileIn, create_household, create_profile};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, create_src};
use ownhealth_core::testing::TestDb;
use ownhealth_rules::{build_state, load_pack, render};
use serde_json::json;

const RUNS: usize = 20;
const BUDGET_MS: f64 = 300.0;

#[tokio::test]
async fn status_for_five_years_of_labs_is_under_300ms_p95() -> anyhow::Result<()> {
    common::install_fake_genome();
    let db = TestDb::new().await?;
    let mut groups: Vec<(String, Option<String>)> = load_pack()?
        .analytes
        .iter()
        .map(|(g, a)| (g.clone(), a["loinc"].get(0).map(ownhealth_rules::pack::text)))
        .collect();
    let n = groups.len();
    groups.extend((0..40 - n).map(|i| (format!("extra_{i:02}"), None)));
    assert_eq!(groups.len(), 40);
    let mut tx = db.pool.begin().await?;
    let h = create_household(&mut tx, "Ortega (lane E perf)").await?;
    let pid = create_profile(
        &mut tx,
        &ProfileIn {
            household_id: h,
            slug: "perf".into(),
            display_name: "Carmen Ortega".into(),
            relationship: Some("self".into()),
            birth_date: NaiveDate::from_ymd_opt(1955, 1, 30),
            sex_at_birth: Some("female".into()),
            country: Some("ES".into()),
            user_id: None,
            unit_pref: None,
        },
    )
    .await?;
    for m in 0..60 {
        let (y, mo) = ((2021 * 12 + 10 + m) / 12, (2021 * 12 + 10 + m) % 12); // 2021-11 .. 2026-10
        let day = Utc
            .with_ymd_and_hms(y, mo as u32 + 1, 3, 8, 0, 0)
            .single()
            .unwrap_or_default();
        let src = SrcIn {
            profile_id: Some(pid),
            label: Some(format!("LabNorte · {}", day.date_naive())),
            ..SrcIn::new("manual")
        };
        let src = create_src(&mut tx, &src).await?;
        for (i, (g, loinc)) in groups.iter().enumerate() {
            let v = 50 + (m as usize * 7 + i * 13) % 40; // deterministic, crosses 80 now and then
            let obs = ObservationIn::from_json(json!({
                "profile_id": pid, "printed_name": g, "printed_value": v.to_string(), "printed_unit": "U/L",
                "printed_ref_range": "40-80", "value_num": v.to_string(), "ref_low": "40", "ref_high": "80",
                "loinc": loinc, "alias_group": g, "unit_ucum": "U/L", "value_canonical": v.to_string(),
                "unit_canonical": "U/L", "interpretation": if v > 80 { "H" } else { "N" },
                "derivation_version": "perf", "effective_at": day.to_rfc3339(),
            }))?;
            commit_observation(&mut tx, &obs, &SrcRef::Id(src.clone()), CommitOptions::new("auto")).await?;
        }
    }
    tx.commit().await?;
    let as_of = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default();
    let mut conn = db.pool.acquire().await?;
    let body = build_state(&mut conn, pid, as_of, None).await?; // warm the pack and the plan cache
    assert!(
        body["flags"].as_array().is_some_and(|f| !f.is_empty()),
        "the generated profile must exercise the rules"
    );
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t = Instant::now();
        let body = build_state(&mut conn, pid, as_of, None).await?;
        render(&body, "es", "as_reported", "pseudonymous", None)?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    let p95 = times[RUNS * 95 / 100 - 1];
    println!(
        "status p95 {p95:.1} ms, median {:.1} ms over {RUNS} runs",
        times[RUNS / 2]
    );
    assert!(p95 < BUDGET_MS, "p95 {p95:.1} ms");
    Ok(())
}
