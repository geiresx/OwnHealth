//! Apple Health export.zip: streaming parse, samples, rollups, source priority, clinical records, ECG
//! (`tests/imports/test_apple_health.py`).

mod common;

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use common::{Env, R, ZIP, family, store_doc};
use ownhealth_core::Error;
use ownhealth_core::documents::{StoredFile, dispatch_upload};
use ownhealth_core::jobs::{JobContext, drain};
use ownhealth_core::records::active_medications;
use ownhealth_ingest_structured::apple_health::parse::ExportReader;
use ownhealth_ingest_structured::apple_health::synthetic::{ExportSpec, XmlStream, demo_clinical, write_export_to};
use ownhealth_ingest_structured::apple_health::{import_apple_health, rebuild_rollups};
use ownhealth_ingest_structured::wearables::{self, Point, day_start};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap_or_default()
}

fn utc(y: i32, m: u32, day: u32, h: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, day, h, min, 0).single().unwrap_or_default()
}

fn alex_dob() -> NaiveDate {
    d(1979, 5, 14)
}

fn end() -> NaiveDate {
    d(2026, 11, 7)
}

fn zip_of(spec: &ExportSpec) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    write_export_to(&mut buf, spec).unwrap_or_else(|e| panic!("zip: {e}"));
    buf.into_inner()
}

fn spec(days: i64) -> ExportSpec {
    ExportSpec {
        days,
        end: end(),
        birth_date: alex_dob(),
        ..ExportSpec::default()
    }
}

fn handmade_xml(records: &str, dob: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<HealthData locale=\"es_ES\">\n \
         <ExportDate value=\"2026-11-07 08:00:00 +0100\"/>\n <Me HKCharacteristicTypeIdentifierDateOfBirth=\"{dob}\"/>\n\
         {records}</HealthData>\n"
    )
}

fn handmade(records: &str) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file("apple_health_export/export.xml", SimpleFileOptions::default())
            .unwrap_or_else(|e| panic!("{e}"));
        z.write_all(handmade_xml(records, "1979-05-14").as_bytes())
            .unwrap_or_else(|e| panic!("{e}"));
        z.finish().unwrap_or_else(|e| panic!("{e}"));
    }
    buf.into_inner()
}

fn rec(typ: &str, start: &str, end: &str, value: &str, unit: &str, device: &str) -> String {
    let dev = format!("&lt;&lt;HKDevice: 0x1&gt;, name:{device}, model:{device}&gt;");
    format!(
        " <Record type=\"{typ}\" sourceName=\"x\" unit=\"{unit}\" device=\"{dev}\" startDate=\"{start}\" \
         endDate=\"{end}\" value=\"{value}\"/>\n"
    )
}

fn sleep(start: &str, end: &str, stage: &str) -> String {
    rec(
        "HKCategoryTypeIdentifierSleepAnalysis",
        start,
        end,
        &format!("HKCategoryValueSleepAnalysis{stage}"),
        "",
        "Apple Watch",
    )
    .replace(" unit=\"\"", "")
}

/// Per device and local date: the step totals straight from the generated XML.
fn expected_steps(spec: &ExportSpec) -> HashMap<String, HashMap<NaiveDate, f64>> {
    let mut out: HashMap<String, HashMap<NaiveDate, f64>> = HashMap::new();
    let mut reader = ExportReader::new(std::io::BufReader::new(XmlStream::new(spec)));
    while let Some(s) = reader.next_sample().unwrap_or_else(|e| panic!("{e}")) {
        if s.metric == "steps" {
            let day = (s.t + Duration::minutes(i64::from(s.tz_offset_min))).date_naive();
            *out.entry(s.device.clone()).or_default().entry(day).or_default() += s.v;
        }
    }
    out
}

async fn count(conn: &mut PgConnection, sql: &str, p: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(p)
        .fetch_one(conn)
        .await?)
}

async fn series(
    conn: &mut PgConnection,
    p: Uuid,
    metric: &str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    resolution: &str,
) -> anyhow::Result<Vec<Point>> {
    Ok(wearables::points(conn, p, metric, since, until, resolution).await?)
}

fn by_day(pts: &[Point]) -> HashMap<NaiveDate, f64> {
    pts.iter()
        .map(|p| (p.t.date_naive(), p.value.unwrap_or(f64::NAN)))
        .collect()
}

#[tokio::test]
async fn export_imports_samples_clinical_records_and_ecg() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let s = ExportSpec {
        days: 14,
        clinical: demo_clinical("Alex"),
        ecg_count: 2,
        ..spec(14)
    };
    let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &zip_of(&s), "export.zip", ZIP).await?;
    let summary = import_apple_health(&mut tx, &env.blobs, doc).await?;
    let n = count(&mut tx, "select count(*) from wearable_sample where profile_id = $1", p).await?;
    // per day: 16 Watch + 14 iPhone steps, 4 sleep stages (one Awake, skipped), in bed, resting HR, HRV; VO2max weekly
    assert_eq!(n, 14 * (16 + 14 + 3 + 1 + 1 + 1) + 2);
    assert_eq!((summary.accepted, summary.queued), (n + 2, 0)); // + the clinical lab result and prescription
    let devices: Vec<Option<String>> =
        sqlx::query_scalar("select device from wearable_source where profile_id = $1 order by device")
            .bind(p)
            .fetch_all(&mut *tx)
            .await?;
    assert_eq!(
        devices,
        vec![Some("Apple Watch".to_string()), Some("iPhone".to_string())]
    );
    // the samples are citable: one apple_health src names this export
    let cites: Vec<Option<Uuid>> =
        sqlx::query_scalar("select document_id from src where profile_id = $1 and kind = 'apple_health'")
            .bind(p)
            .fetch_all(&mut *tx)
            .await?;
    assert_eq!(cites, vec![Some(doc)]);
    let (t, tz): (DateTime<Utc>, Option<i16>) = sqlx::query_as(
        "select t, tz_offset_min from wearable_sample where profile_id = $1 and metric = 'hr_resting' order by t limit 1",
    )
    .bind(p)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!((t, tz), (utc(2026, 10, 25, 6, 5), Some(60))); // 07:05 +0100
    // clinical records follow the FHIR path, with Apple as the label
    let meds = active_medications(&mut tx, p, None, None, None, true).await?;
    assert_eq!(meds.len(), 1);
    assert_eq!(meds[0]["name"], "Cetirizine 10 mg tablet");
    assert_eq!(meds[0]["accepted_by"], "import");
    let (kind, label): (String, Option<String>) = sqlx::query_as(
        "select s.kind, s.label from observation o join src s on s.id = o.src_id \
         where o.profile_id = $1 and o.loinc = '2276-4'",
    )
    .bind(p)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(kind, "fhir_import");
    assert!(label.unwrap_or_default().starts_with("Apple Health"));
    let ecg: Vec<(String, Option<DateTime<Utc>>)> =
        sqlx::query_as("select mime, collected_at from document where profile_id = $1 and doc_type = 'ecg'")
            .bind(p)
            .fetch_all(&mut *tx)
            .await?;
    assert_eq!(ecg.len(), 2);
    assert!(ecg.iter().all(|(mime, at)| mime == "text/csv" && at.is_some()));
    let flags: Value = sqlx::query_scalar("select flags from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(flags["apple_health"]["samples"], json!(n));
    assert_eq!(
        flags["apple_health"]["not_imported"],
        json!({"skipped:sleep_awake": 14})
    );
    Ok(())
}

#[tokio::test]
async fn priority_takes_the_watch_and_never_sums_watch_and_iphone() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let s = spec(10);
    let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &zip_of(&s), "e.zip", ZIP).await?;
    import_apple_health(&mut tx, &env.blobs, doc).await?;
    let want = expected_steps(&s);
    let (since, until) = (day_start(end() - Duration::days(9)), day_start(end()));
    let pts = series(&mut tx, p, "steps", since, until, "day").await?;
    assert_eq!(pts.len(), 10);
    assert_eq!(by_day(&pts), want["Apple Watch"]);
    assert!(pts.iter().all(|pt| pt.device.as_deref() == Some("Apple Watch")));
    // the person prefers the phone: same days, the phone's totals
    let ids: HashMap<String, i16> =
        sqlx::query_as::<_, (String, i16)>("select device, id from wearable_source where profile_id = $1")
            .bind(p)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
    wearables::set_priority(&mut tx, p, "steps", &[ids["iPhone"], ids["Apple Watch"]]).await?;
    let pts = series(&mut tx, p, "steps", since, until, "day").await?;
    assert_eq!(by_day(&pts), want["iPhone"]);
    // a day the Watch was not worn falls back to the next source instead of going empty
    wearables::set_priority(&mut tx, p, "steps", &[ids["Apple Watch"], ids["iPhone"]]).await?;
    let gap = end() - Duration::days(3);
    sqlx::query(
        "delete from wearable_sample where profile_id = $1 and source_id = $2 and metric = 'steps' and t >= $3 and t < $4",
    )
    .bind(p)
    .bind(ids["Apple Watch"])
    .bind(day_start(gap))
    .bind(day_start(gap + Duration::days(1)))
    .execute(&mut *tx)
    .await?;
    rebuild_rollups(&mut tx, p, Some("steps")).await?;
    let pts = series(&mut tx, p, "steps", since, until, "day").await?;
    let day: HashMap<NaiveDate, &Point> = pts.iter().map(|pt| (pt.t.date_naive(), pt)).collect();
    assert_eq!(day[&gap].device.as_deref(), Some("iPhone"));
    assert_eq!(day[&gap].value, Some(want["iPhone"][&gap]));
    assert_eq!(day[&end()].device.as_deref(), Some("Apple Watch"));
    Ok(())
}

#[tokio::test]
async fn rollup_statistics_local_day_and_sleep_on_waking() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let hr: String = (0..10)
        .map(|i| {
            let t = format!("2026-10-02 10:{:02}:00 +0200", i * 5);
            rec(
                "HKQuantityTypeIdentifierHeartRate",
                &t,
                &t,
                &(60 + i).to_string(),
                "count/min",
                "Apple Watch",
            )
        })
        .collect();
    let late = rec(
        "HKQuantityTypeIdentifierStepCount",
        "2026-10-02 00:30:00 +0200",
        "2026-10-02 00:40:00 +0200",
        "120",
        "count",
        "iPhone",
    );
    let night = [
        ("2026-10-01 23:00:00 +0200", "2026-10-02 03:00:00 +0200", "AsleepCore"),
        ("2026-10-02 03:00:00 +0200", "2026-10-02 03:20:00 +0200", "Awake"),
        ("2026-10-02 03:20:00 +0200", "2026-10-02 06:50:00 +0200", "AsleepREM"),
    ]
    .iter()
    .map(|(a, b, v)| sleep(a, b, v))
    .collect::<String>();
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        &handmade(&(hr + &late + &night)),
        "e.zip",
        ZIP,
    )
    .await?;
    import_apple_health(&mut tx, &env.blobs, doc).await?;
    let day = utc(2026, 10, 2, 0, 0);
    let hours = series(&mut tx, p, "hr", day, day + Duration::days(1), "hour").await?;
    assert_eq!(hours.len(), 1);
    let h = &hours[0];
    assert_eq!(h.t, utc(2026, 10, 2, 8, 0)); // 10:00 +0200
    assert_eq!(
        (h.n, h.min, h.max, h.mean, h.median),
        (10, Some(60.0), Some(69.0), Some(64.5), Some(64.5))
    );
    let r2 = |v: Option<f64>| (v.unwrap_or_default() * 100.0).round() / 100.0;
    assert_eq!((r2(h.p10), r2(h.p90)), (60.9, 68.1));
    assert!((h.coverage - 10.0 / 12.0).abs() < 1e-6); // 10 of 12 five-minute slots
    let days = series(&mut tx, p, "hr", day, day, "day").await?;
    assert_eq!(days.len(), 1);
    assert!((days[0].coverage - 1.0 / 24.0).abs() < 1e-6);
    // 00:30 local on 2 Oct is 22:30 UTC on 1 Oct: it counts on the local date
    let steps = series(&mut tx, p, "steps", day - Duration::days(1), day, "day").await?;
    assert_eq!(steps.len(), 1);
    assert_eq!((steps[0].t, steps[0].value), (day, Some(120.0)));
    // the night that ends on 2 Oct: 240 + 210 minutes asleep, the Awake stage left out, on the waking date
    let nights = series(&mut tx, p, "sleep_asleep", day - Duration::days(1), day, "day").await?;
    assert_eq!(nights.len(), 1);
    assert_eq!(
        (nights[0].t, nights[0].value, nights[0].coverage),
        (day, Some(450.0), 1.0)
    );
    Ok(())
}

/// A Watch writes many short stages; those that end before 00:00 still belong to the night that ends
/// the next morning. Two nights more than 2 h apart stay two nights.
#[tokio::test]
async fn a_night_with_stages_before_midnight_counts_whole_on_the_waking_date() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let nights = [
        sleep("2026-11-05 22:45:00 +0100", "2026-11-05 23:40:00 +0100", "AsleepCore"),
        sleep("2026-11-05 23:40:00 +0100", "2026-11-06 00:30:00 +0100", "AsleepDeep"),
        sleep("2026-11-06 00:30:00 +0100", "2026-11-06 06:45:00 +0100", "AsleepREM"),
        sleep("2026-11-06 22:30:00 +0100", "2026-11-06 23:50:00 +0100", "AsleepCore"),
        sleep("2026-11-06 23:50:00 +0100", "2026-11-07 06:30:00 +0100", "AsleepREM"),
    ]
    .concat();
    let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &handmade(&nights), "e.zip", ZIP).await?;
    import_apple_health(&mut tx, &env.blobs, doc).await?;
    let pts = series(
        &mut tx,
        p,
        "sleep_asleep",
        utc(2026, 11, 1, 0, 0),
        utc(2026, 11, 8, 0, 0),
        "day",
    )
    .await?;
    let got: HashMap<NaiveDate, f64> = by_day(&pts);
    assert_eq!(got, HashMap::from([(d(2026, 11, 6), 480.0), (d(2026, 11, 7), 480.0)]));
    Ok(())
}

#[tokio::test]
async fn reimport_is_idempotent_and_wrong_profile_is_refused() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let data = zip_of(&spec(5));
    let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &data, "e.zip", ZIP).await?;
    let first = import_apple_health(&mut tx, &env.blobs, doc).await?;
    assert_eq!(import_apple_health(&mut tx, &env.blobs, doc).await?, first);
    // a later export repeats the old samples: none counted twice
    let spec2 = ExportSpec {
        end: end() + Duration::days(1),
        ..spec(6)
    };
    let doc2 = store_doc(&mut tx, &env.blobs, &fam, "alex", &zip_of(&spec2), "b.zip", ZIP).await?;
    let later = import_apple_health(&mut tx, &env.blobs, doc2).await?;
    assert!(later.reasons.get("duplicate").copied().unwrap_or(0) > 0);
    let total = count(&mut tx, "select count(*) from wearable_sample where profile_id = $1", p).await?;
    assert_eq!(total, first.accepted + later.accepted);
    // Alex's export uploaded to Sam's profile: the birth dates differ, nothing is written
    let sam = fam.profile("sam");
    let doc_sam = store_doc(&mut tx, &env.blobs, &fam, "sam", &data, "e.zip", ZIP).await?;
    let mut sp = sqlx::Connection::begin(&mut *tx).await?;
    match import_apple_health(&mut sp, &env.blobs, doc_sam).await {
        Err(Error::Upload(m)) => assert!(m.contains("birth date"), "{m}"),
        other => panic!("expected the birth date refusal, got {other:?}"),
    }
    sp.rollback().await?; // the job's transaction rolls back the rest
    let n = count(
        &mut tx,
        "select count(*) from wearable_sample where profile_id = $1",
        sam,
    )
    .await?;
    assert_eq!(n, 0);
    Ok(())
}

/// The streaming parser keeps one event in a reused buffer: ten times more records do not grow it.
/// (Python measured this with tracemalloc; the process-level measurement is `examples/measure_memory.rs`.)
#[test]
fn parser_memory_stays_flat_as_the_file_grows() {
    let peak = |days: i64| {
        let s = ExportSpec {
            hr_every_min: 10,
            ..spec(days)
        };
        let mut reader = ExportReader::new(std::io::BufReader::new(XmlStream::new(&s)));
        let mut n = 0usize;
        while reader.next_sample().unwrap_or_else(|e| panic!("{e}")).is_some() {
            n += 1;
        }
        (n, reader.buffer_capacity())
    };
    let (small_n, small) = peak(20);
    let (big_n, big) = peak(200);
    assert!(big_n > 9 * small_n);
    assert!(big <= small.max(4096) * 2, "{small} {big}");
}

#[tokio::test]
async fn upload_dispatch_runs_the_job() -> R {
    let env = Env::new().await?;
    ownhealth_ingest_structured::register();
    let mut c = env.db.pool.acquire().await?;
    let fam = family(&mut c).await?;
    drop(c);
    let data = zip_of(&ExportSpec {
        first_name: "Sam".into(),
        birth_date: d(1981, 2, 2),
        ..spec(3)
    });
    let sha = env.blobs.put_bytes(&data)?;
    let mut tx = env.tx().await?;
    let out = dispatch_upload(
        &mut tx,
        &env.blobs,
        fam.household_id,
        fam.profile("sam"),
        &[StoredFile {
            filename: "export.zip".into(),
            sha256: sha,
            size: data.len() as i64,
        }],
        None,
    )
    .await?;
    tx.commit().await?;
    let items = out["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "apple_health_zip");
    let settings = env.db.settings(env.dir.path())?;
    let ctx = JobContext::new(env.db.pool.clone(), Arc::new(settings), env.blobs.clone());
    drain(&ctx, "core", 10, Some(&["import.apple_health".to_string()])).await?;
    let (state, result): (String, Option<Value>) = sqlx::query_as("select state, result from job where id = $1")
        .bind(items[0]["job_id"].as_i64())
        .fetch_one(&env.db.pool)
        .await?;
    let result = result.unwrap_or_default();
    assert_eq!(state, "done");
    assert!(result["accepted"].as_i64().unwrap_or(0) > 0);
    let n: i64 = sqlx::query_scalar("select count(*) from wearable_sample where profile_id = $1")
        .bind(fam.profile("sam"))
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(json!(n), result["accepted"]);
    Ok(())
}

/// A small zip holding one huge, highly compressible member (and a valid export.xml).
fn bomb(member: &str, size: usize) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("apple_health_export/export.xml", opts)
            .unwrap_or_else(|e| panic!("{e}"));
        z.write_all(handmade_xml("", "1979-05-14").as_bytes())
            .unwrap_or_else(|e| panic!("{e}"));
        z.start_file(member, opts.large_file(true))
            .unwrap_or_else(|e| panic!("{e}"));
        let chunk = vec![b'0'; 1024 * 1024];
        for _ in 0..size / chunk.len() {
            z.write_all(&chunk).unwrap_or_else(|e| panic!("{e}"));
        }
        z.finish().unwrap_or_else(|e| panic!("{e}"));
    }
    buf.into_inner()
}

#[tokio::test]
async fn an_inflating_member_stops_the_import_without_being_read() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    for member in [
        "apple_health_export/electrocardiograms/ecg_2026-11-01.csv",
        "apple_health_export/clinical-records/Big.json",
    ] {
        let data = bomb(member, 50 * 1024 * 1024);
        assert!(data.len() < 1024 * 1024, "the bomb is small: {}", data.len());
        let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &data, "bomb.zip", ZIP).await?;
        match import_apple_health(&mut tx, &env.blobs, doc).await {
            Err(Error::Upload(m)) => assert!(m.contains("over 20 MB") || m.contains("over 5 MB"), "{m}"),
            other => panic!("expected the size refusal, got {other:?}"),
        }
    }
    Ok(())
}
