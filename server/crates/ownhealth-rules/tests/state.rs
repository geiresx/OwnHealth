//! The deterministic state for the synthetic Rivera family on 2026-11-07 (0.1.1 tests/state/
//! test_state.py; architecture.md section 16, mockup screen 1): flags, gaps, the per-system status
//! words, the hash and the snapshot.

mod common;

use chrono::{DateTime, NaiveDate, Utc};
use common::{as_of, seeded};
use ownhealth_core::observations::{
    CommitOptions, ObservationIn, commit_observation, retract_observation, supersede_observation,
};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_rules::{MAX_STATE_BYTES, STATUS_WORDS, build_state, canonical_json, state_hash, status_word};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

type R = anyhow::Result<()>;

#[allow(clippy::too_many_arguments)]
fn obs(
    pid: Uuid,
    group: &str,
    loinc: &str,
    value: &str,
    day: &str,
    unit: &str,
    range: (&str, &str, &str),
    interp: Option<&str>,
) -> anyhow::Result<ObservationIn> {
    let (text, low, high) = range;
    Ok(ObservationIn::from_json(json!({
        "profile_id": pid, "printed_name": group, "printed_value": value, "printed_unit": unit,
        "printed_ref_range": text, "value_num": value, "ref_low": if low.is_empty() { Value::Null } else { json!(low) },
        "ref_high": if high.is_empty() { Value::Null } else { json!(high) }, "loinc": loinc, "alias_group": group,
        "unit_ucum": unit, "value_canonical": value, "unit_canonical": unit, "interpretation": interp,
        "derivation_version": "test", "effective_at": format!("{day}T08:00:00+00:00"),
    }))?)
}

fn system_src() -> SrcRef {
    SrcRef::New(SrcIn::new("manual"))
}

async fn commit(conn: &mut PgConnection, o: &ObservationIn) -> anyhow::Result<()> {
    commit_observation(conn, o, &system_src(), CommitOptions::new("person")).await?;
    Ok(())
}

async fn add_med(conn: &mut PgConnection, pid: Uuid, kind: &str, name: &str) -> anyhow::Result<()> {
    let rec = RecordIn::from_json(
        "medication",
        json!({"profile_id": pid, "kind": kind, "name": name, "started": "2026-09-01"}),
    )?;
    add_record(conn, &rec, &system_src(), RecordOptions::new("person")).await?;
    Ok(())
}

fn groups(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x["group"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn alex_matches_the_fixture_and_the_mockup() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let body = build_state(&mut tx, s.pid("alex"), as_of(), None).await?;
    assert_eq!(
        (&body["schema"], &body["profile"], &body["as_of"]),
        (&json!("ownhealth.state/v1"), &json!("alex"), &json!("2026-11-07"))
    );
    assert_eq!(
        body["systems"],
        json!({"cardiometabolic": "out", "liver": "ok", "kidney": "ok", "thyroid": "stale", "blood": "ok",
               "inflammation": "watch", "vitamins": "out", "sleep": "ok", "fitness": "ok"})
    );
    let flags: Vec<(String, String, String)> = body["flags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            let who = f
                .get("group")
                .or_else(|| f.get("gene"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            (
                f["reason"].as_str().unwrap_or_default().into(),
                f["rule"].as_str().unwrap_or_default().into(),
                who.into(),
            )
        })
        .collect();
    let want: Vec<(String, String, String)> = [
        ("LAB_HIGH", "range.lab", "lpa"),
        ("LAB_LOW", "range.lab", "vitamin_d_25oh"),
        ("GENE_DRUG", "pgx.slco1b1.statin", "SLCO1B1"),
        ("RCV_CHANGE", "rcv.crp_hs", "crp_hs"),
        ("TREND_UP", "trend.hba1c.rise", "hba1c"),
    ]
    .iter()
    .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    .collect();
    assert_eq!(flags, want);
    let (lpa, vitd, gene_drug) = (&body["flags"][0], &body["flags"][1], &body["flags"][2]);
    assert_eq!(
        (&lpa["value"], &lpa["ref"], &lpa["date"], &lpa["src"]),
        (&json!("142"), &json!("< 75"), &json!("2025-09-08"), &json!("src_a07"))
    );
    assert_eq!(
        (&vitd["value"], &vitd["date"], &vitd["src"]),
        (&json!("24"), &json!("2026-09-14"), &json!("src_a12"))
    );
    assert_eq!(
        (&gene_drug["src"], &gene_drug["drug"]),
        (&json!(["src_g31", "src_m02"]), &json!("atorvastatin"))
    );
    let tsh = body["stale"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["group"] == "tsh")
        .cloned();
    assert_eq!(
        tsh,
        Some(json!({"group": "tsh", "last": "2025-01-20", "rule": "recency.thyroid", "src": "src_a05"}))
    );
    let mut stale = groups(&body["stale"]);
    stale.sort();
    assert_eq!(stale, ["ft4", "tsh", "urine_acr"]);
    assert_eq!(
        body["never_measured"],
        json!([{"group": "hcv_ab", "rule": "infect.hcv.once_adult"}])
    );
    let meds: Vec<_> = body["medications"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| (m["name"].clone(), m["since"].clone(), m["src"].clone()))
        .collect();
    assert_eq!(
        meds,
        [
            (json!("Atorvastatin"), json!("2026-03-10"), json!("src_m02")),
            (json!("Omeprazole"), json!("2026-04-01"), json!("src_m05"))
        ]
    );
    assert_eq!(
        (&body["notes"][0]["src"], &body["notes"][0]["author_kind"]),
        (&json!("src_n41"), &json!("person"))
    );
    assert_eq!(
        body["freshness"],
        json!({"labs": "2026-09-14", "wearables": "2026-11-07"})
    );
    Ok(())
}

#[tokio::test]
async fn every_fact_in_the_body_cites_a_src() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let body = build_state(&mut tx, s.pid("alex"), as_of(), None).await?;
    for section in ["flags", "stale", "medications", "conditions", "notes"] {
        for item in body[section].as_array().into_iter().flatten() {
            let srcs: Vec<Value> = match &item["src"] {
                Value::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
            assert!(
                !srcs.is_empty() && srcs.iter().all(|s| s.as_str().is_some_and(|s| s.starts_with("src_"))),
                "{section} {item}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn body_fits_the_2kb_budget() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    for slug in ["alex", "sam", "lucia", "carmen"] {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        assert!(canonical_json(&body)?.len() <= MAX_STATE_BYTES, "{slug}");
    }
    Ok(())
}

#[tokio::test]
async fn other_profiles() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let sam = build_state(&mut tx, s.pid("sam"), as_of(), None).await?;
    let mut never = groups(&sam["never_measured"]);
    never.sort();
    assert_eq!(never, ["hcv_ab", "lpa"]);
    let lucia = build_state(&mut tx, s.pid("lucia"), as_of(), None).await?;
    assert_eq!(lucia["never_measured"], json!([])); // adult screening rules skip a child
    let carmen = build_state(&mut tx, s.pid("carmen"), as_of(), None).await?;
    assert_eq!(
        (&carmen["systems"]["kidney"], &carmen["systems"]["cardiometabolic"]),
        (&json!("out"), &json!("out"))
    );
    assert_eq!(groups(&carmen["flags"]), ["hba1c", "egfr"]);
    Ok(())
}

#[tokio::test]
async fn two_runs_same_bytes_and_hash_under_any_session_timezone() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("alex");
    let a = build_state(&mut tx, pid, as_of(), None).await?;
    sqlx::query("set local time zone 'Pacific/Kiritimati'")
        .execute(&mut *tx)
        .await?;
    let b = build_state(&mut tx, pid, as_of(), None).await?;
    sqlx::query("set local time zone 'America/Los_Angeles'")
        .execute(&mut *tx)
        .await?;
    let c = build_state(&mut tx, pid, as_of(), None).await?;
    assert_eq!(canonical_json(&a)?, canonical_json(&b)?);
    assert_eq!(canonical_json(&b)?, canonical_json(&c)?);
    assert_eq!(a["state_hash"].as_str(), Some(state_hash(&a)?.as_str()));
    Ok(())
}

#[tokio::test]
async fn snapshot_written_once_per_hash() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("alex");
    let count = "select count(*), max(created_at) from state_snapshot where profile_id = $1";
    let first = build_state(&mut tx, pid, as_of(), None).await?;
    let one: (i64, Option<DateTime<Utc>>) = sqlx::query_as(count).bind(pid).fetch_one(&mut *tx).await?;
    build_state(&mut tx, pid, as_of(), None).await?;
    let two: (i64, Option<DateTime<Utc>>) = sqlx::query_as(count).bind(pid).fetch_one(&mut *tx).await?;
    assert_eq!(one, two);
    let midnight = as_of().and_hms_opt(0, 0, 0).unwrap_or_default().and_utc();
    let (hash, body): (Vec<u8>, Value) =
        sqlx::query_as("select state_hash, body from state_snapshot where profile_id = $1 and as_of = $2")
            .bind(pid)
            .bind(midnight)
            .fetch_one(&mut *tx)
            .await?;
    let want = first["state_hash"].as_str().unwrap_or_default();
    assert_eq!(format!("sha256:{}", hex::encode(hash)), want);
    assert_eq!(body["state_hash"].as_str(), Some(want));
    Ok(())
}

#[tokio::test]
async fn new_data_changes_the_hash_and_known_at_reproduces_the_old_one() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("alex");
    let before = build_state(&mut tx, pid, as_of(), None).await?;
    // inside one transaction every insert has recorded_at = now(); the seed was committed earlier
    let cut: DateTime<Utc> = sqlx::query_scalar("select now() - interval '1 microsecond'")
        .fetch_one(&mut *tx)
        .await?;
    let tsh = obs(
        pid,
        "tsh",
        "3016-3",
        "2.0",
        "2026-11-02",
        "mUI/L",
        ("0.40 - 4.00", "0.40", "4.00"),
        None,
    )?;
    commit(&mut tx, &tsh).await?;
    let after = build_state(&mut tx, pid, as_of(), None).await?;
    assert_ne!(after["state_hash"], before["state_hash"]);
    assert!(!groups(&after["stale"]).contains(&"tsh".to_string()));
    assert_eq!(after["systems"]["thyroid"], "ok"); // TSH fresh, free T4 still old: one fresh marker is enough
    // what the store knew before the import gives the old bytes back
    let again = build_state(&mut tx, pid, as_of(), Some(cut)).await?;
    assert_eq!(again["state_hash"], before["state_hash"]);
    let now: DateTime<Utc> = sqlx::query_scalar("select clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(
        build_state(&mut tx, pid, as_of(), Some(now)).await?["state_hash"],
        after["state_hash"]
    );
    Ok(())
}

#[tokio::test]
async fn thyroid_card_turns_ok_when_any_marker_is_fresh() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("alex");
    for (group, loinc, value) in [("tsh", "3016-3", "2.0"), ("ft4", "3024-7", "1.2")] {
        commit(
            &mut tx,
            &obs(
                pid,
                group,
                loinc,
                value,
                "2026-11-02",
                "x",
                ("0.4 - 4.0", "0.4", "4.0"),
                None,
            )?,
        )
        .await?;
    }
    assert_eq!(
        build_state(&mut tx, pid, as_of(), None).await?["systems"]["thyroid"],
        "ok"
    );
    Ok(())
}

async fn db_today(conn: &mut PgConnection) -> anyhow::Result<NaiveDate> {
    Ok(sqlx::query_scalar("select (now() at time zone 'utc')::date")
        .fetch_one(conn)
        .await?)
}

#[tokio::test]
async fn a_retraction_appears_in_corrections_and_leaves_the_flags() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("alex");
    let oid: Uuid = sqlx::query_scalar("select o.id from observation o where o.src_id = 'src_a12'")
        .fetch_one(&mut *tx)
        .await?; // vitamin D 24 (L)
    retract_observation(&mut tx, oid, "entered for the wrong person").await?;
    // the retraction is stamped now(), so build the state as of the DB's day, never before it
    let day = db_today(&mut tx).await?.max(as_of());
    let body = build_state(&mut tx, pid, day, None).await?;
    assert!(!groups(&body["flags"]).contains(&"vitamin_d_25oh".to_string()));
    assert_eq!(
        (&body["corrections"][0]["src"], &body["corrections"][0]["status"]),
        (&json!("src_a12"), &json!("retracted"))
    );
    Ok(())
}

#[tokio::test]
async fn a_rederive_supersession_is_not_a_correction() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("alex");
    let (id, group, loinc, value, at): (Uuid, String, String, String, DateTime<Utc>) = sqlx::query_as(
        "select id, alias_group, loinc, printed_value, effective_at from observation where src_id = 'src_a12'",
    )
    .fetch_one(&mut *tx)
    .await?;
    let day = at.date_naive().to_string();
    let copy = obs(
        pid,
        &group,
        &loinc,
        &value,
        &day,
        "ng/mL",
        ("30 - 100", "30", "100"),
        None,
    )?;
    supersede_observation(
        &mut tx,
        id,
        &copy,
        &SrcRef::Id("src_a12".into()),
        "auto",
        "rederive norm-9.9.9",
        None,
    )
    .await?;
    let day = db_today(&mut tx).await?.max(as_of());
    let body = build_state(&mut tx, pid, day, None).await?;
    assert!(
        !body["corrections"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|c| c["src"] == "src_a12")
    );
    Ok(())
}

#[tokio::test]
async fn sensitive_rows_are_counted_not_evaluated() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("sam");
    let mut o = obs(
        pid,
        "ferritin",
        "2276-4",
        "5",
        "2026-10-01",
        "ng/mL",
        ("30 - 100", "30", "100"),
        Some("L"),
    )?;
    o.sensitivity = vec!["reproductive".into()];
    commit(&mut tx, &o).await?;
    let body = build_state(&mut tx, pid, as_of(), None).await?;
    assert_eq!(body["flags"], json!([]));
    assert_eq!(body["withheld"]["sensitive"], 1);
    Ok(())
}

#[tokio::test]
async fn never_measured_ferritin_on_an_iron_supplement() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("carmen");
    add_med(&mut tx, pid, "supplement", "Hierro (sulfato ferroso)").await?;
    let body = build_state(&mut tx, pid, as_of(), None).await?;
    let want = json!({"group": "ferritin", "rule": "iron.ferritin.on_iron"});
    assert!(body["never_measured"].as_array().is_some_and(|n| n.contains(&want)));
    assert_eq!(body["supplements"][0]["name"], "Hierro (sulfato ferroso)");
    Ok(())
}

/// `taking` matches whole words: "Environ" holds "iron" and is not an iron supplement.
#[tokio::test]
async fn an_iron_term_inside_another_word_is_not_an_iron_supplement() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pid = s.pid("carmen");
    add_med(&mut tx, pid, "supplement", "Environ skin cream").await?;
    let body = build_state(&mut tx, pid, as_of(), None).await?;
    let want = json!({"group": "ferritin", "rule": "iron.ferritin.on_iron"});
    assert!(!body["never_measured"].as_array().is_some_and(|n| n.contains(&want)));
    Ok(())
}

#[tokio::test]
async fn gene_lab_lpa_never_measured() -> R {
    let s = seeded().await?;
    let mut f = s.findings("sam");
    f.push(json!({"category": "risk_marker", "gene": "LPA", "call_state": "alt", "src": "src_g90"}));
    s.set_findings("sam", f);
    let mut tx = s.pool().begin().await?;
    let body = build_state(&mut tx, s.pid("sam"), as_of(), None).await?;
    // the screening rule claims Lp(a) first; the gene_lab rule does not add a second entry
    let lpa: Vec<&Value> = body["never_measured"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["group"] == "lpa")
        .collect();
    assert_eq!(lpa, [&json!({"group": "lpa", "rule": "lipid.lpa.once_adult"})]);
    Ok(())
}

#[tokio::test]
async fn status_words() -> R {
    let s = seeded().await?;
    let mut conn = s.pool().acquire().await?;
    let pid = s.pid("alex");
    let want = [
        ("vitamin_d_25oh", "out"),
        ("tsh", "stale"),
        ("crp_hs", "watch"),
        ("ldl_c", "ok"),
        ("hba1c", "watch"),
        ("lpa", "out"),
        ("hcv_ab", "nodata"),
    ];
    for (group, word) in want {
        let got = status_word(&mut conn, pid, group, as_of()).await?;
        assert_eq!(got, word, "{group}");
        assert!(STATUS_WORDS.contains(&got));
    }
    Ok(())
}

#[tokio::test]
async fn as_of_in_the_past_ignores_later_values() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let day = NaiveDate::from_ymd_opt(2025, 2, 1).unwrap_or_default();
    let body = build_state(&mut tx, s.pid("alex"), day, None).await?;
    let vitd = body["flags"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["group"] == "vitamin_d_25oh")
        .cloned()
        .unwrap_or_default();
    assert_eq!((&vitd["value"], &vitd["date"]), (&json!("29"), &json!("2025-01-20"))); // the 24 of 2026-09-14 is not visible yet
    assert_eq!(body["freshness"]["labs"], "2025-01-20");
    assert_eq!(body["systems"]["thyroid"], "ok");
    // the first atorvastatin course (2022-06-20 to 2025-12-01) was active then
    let srcs: Vec<&Value> = body["medications"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| &m["src"])
        .collect();
    assert_eq!(srcs, [&json!("src_m01")]);
    Ok(())
}
