//! Gadgetbridge database import through the real job path: an upload-shaped payload, the
//! `import.gadgetbridge` handler, samples owned by the new document. The database is built here with
//! the table and column names Gadgetbridge's greenDAO schema uses (`GBDaoGenerator.java`), in WAL mode
//! as the app writes it. Synthetic data for Sam Rivera.

mod common;

use std::path::Path;

use chrono::{TimeZone, Utc};
use common::{Env, R};
use ownhealth_core::jobs::{self, EnqueueOptions};
use ownhealth_devices::gadgetbridge;
use rusqlite::Connection;
use serde_json::{Value, json};

const T0: i64 = 1_793_000_000; // 2026-10-26, seconds

fn build(path: &Path) -> rusqlite::Result<()> {
    let db = Connection::open(path)?;
    db.execute_batch(
        "pragma journal_mode = wal;
         create table DEVICE (_id integer primary key, NAME text not null, MANUFACTURER text not null,
           IDENTIFIER text not null unique, TYPE integer not null, TYPE_NAME text not null, MODEL text, ALIAS text);
         insert into DEVICE values (1, 'Amazfit Band 7', 'Huami', 'C8:0F:10:00:00:01', 0, 'AMAZFITBAND7', null, 'Pulsera de Sam');
         insert into DEVICE values (2, 'Mi Smart Scale 2', 'Xiaomi', 'C8:0F:10:00:00:02', 0, 'MISCALE2', null, null);
         create table HUAMI_EXTENDED_ACTIVITY_SAMPLE (TIMESTAMP integer not null, DEVICE_ID integer not null,
           USER_ID integer not null, RAW_INTENSITY integer not null, STEPS integer not null, RAW_KIND integer not null,
           HEART_RATE integer not null, UNKNOWN1 integer, SLEEP integer, DEEP_SLEEP integer, REM_SLEEP integer,
           primary key (TIMESTAMP, DEVICE_ID));
         create table HUAMI_HEART_RATE_RESTING_SAMPLE (TIMESTAMP integer not null, DEVICE_ID integer not null,
           UTC_OFFSET integer not null, HEART_RATE integer not null, primary key (TIMESTAMP, DEVICE_ID));
         create table HUAMI_SPO2_SAMPLE (TIMESTAMP integer not null, DEVICE_ID integer not null,
           TYPE_NUM integer not null, SPO2 integer not null, primary key (TIMESTAMP, DEVICE_ID));
         create table HUAMI_SLEEP_RESPIRATORY_RATE_SAMPLE (TIMESTAMP integer not null, DEVICE_ID integer not null,
           UTC_OFFSET integer not null, RATE integer not null, primary key (TIMESTAMP, DEVICE_ID));
         create table MI_SCALE_WEIGHT_SAMPLE (TIMESTAMP integer not null, DEVICE_ID integer not null,
           USER_ID integer not null, WEIGHT_KG real not null, primary key (TIMESTAMP, DEVICE_ID));
         create table XIAOMI_SLEEP_STAGE_SAMPLE (TIMESTAMP integer not null, DEVICE_ID integer not null,
           STAGE integer not null, primary key (TIMESTAMP, DEVICE_ID));
         create table HUAMI_STRESS_SAMPLE (TIMESTAMP integer not null, DEVICE_ID integer not null,
           TYPE_NUM integer not null, STRESS integer not null, primary key (TIMESTAMP, DEVICE_ID));
         create view NOT_A_SAMPLE as select 1;",
    )?;
    // one day of minutes: steps on 600 of them, heart rate on all but the 255 / 0 "nothing measured"
    for m in 0..1440i64 {
        let steps = if (480..1080).contains(&m) { 10 + m % 50 } else { 0 };
        let hr = match m % 100 {
            0 => 255,
            1 => 0,
            _ => 55 + m % 60,
        };
        db.execute(
            "insert into HUAMI_EXTENDED_ACTIVITY_SAMPLE values (?1, 1, 1, 30, ?2, 1, ?3, 0, 0, 0, 0)",
            (T0 + m * 60, steps, hr),
        )?;
    }
    for d in 0..3i64 {
        let ms = (T0 + d * 86_400) * 1000;
        db.execute(
            "insert into HUAMI_HEART_RATE_RESTING_SAMPLE values (?1, 1, 60, ?2)",
            (ms, 54 + d),
        )?;
        db.execute("insert into HUAMI_SPO2_SAMPLE values (?1, 1, 0, 97)", (ms + 3_600_000,))?;
        db.execute("insert into HUAMI_SPO2_SAMPLE values (?1, 1, 0, 0)", (ms + 7_200_000,))?;
        db.execute(
            "insert into HUAMI_SLEEP_RESPIRATORY_RATE_SAMPLE values (?1, 1, 60, 14)",
            (ms + 1_800_000,),
        )?;
        db.execute(
            "insert into MI_SCALE_WEIGHT_SAMPLE values (?1, 2, 1, 71.4)",
            (ms + 25_200_000,),
        )?;
        db.execute(
            "insert into XIAOMI_SLEEP_STAGE_SAMPLE values (?1, 1, 2)",
            (ms + 600_000,),
        )?;
    }
    Ok(())
}

async fn upload(env: &Env, bytes: Vec<u8>, slug: &str) -> anyhow::Result<Option<i64>> {
    let sha = env.blobs.put_bytes(&bytes)?;
    let mut c = env.db.pool.acquire().await?;
    let payload = json!({
        "profile_id": env.fam.profile(slug).to_string(),
        "household_id": env.fam.household_id.to_string(),
        "blob_sha256": sha,
        "filename": "Gadgetbridge",
    });
    Ok(jobs::enqueue(&mut c, gadgetbridge::JOB, &payload, EnqueueOptions::default()).await?)
}

async fn job(env: &Env, id: Option<i64>) -> anyhow::Result<(String, Value, Option<String>)> {
    jobs::drain(&env.ctx, "core", 5, None).await?;
    Ok(
        sqlx::query_as("select state, coalesce(result, 'null'::jsonb), last_error from job where id = $1")
            .bind(id)
            .fetch_one(&env.db.pool)
            .await?,
    )
}

#[tokio::test]
async fn a_gadgetbridge_database_imports_its_device_tables() -> R {
    let env = Env::new().await?;
    let sam = env.fam.profile("sam");
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("Gadgetbridge");
    build(&path)?;
    // the export as the app leaves it: the main file, with the WAL checkpointed into it
    let bytes = std::fs::read(&path)?;
    assert_eq!(&bytes[..16], b"SQLite format 3\0");
    assert_eq!(
        ownhealth_core::documents::sniff("Gadgetbridge", &bytes[..64], None),
        "gadgetbridge_db"
    );

    let (state, result, err) = job(&env, upload(&env, bytes.clone(), "sam").await?).await?;
    assert_eq!(state, "done", "{err:?}");
    let by_metric: Vec<(String, i64)> =
        sqlx::query_as("select metric, count(*) from wearable_sample where profile_id = $1 group by 1 order by 1")
            .bind(sam)
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(
        by_metric,
        vec![
            ("hr".into(), 1440 - 2 * 15),
            ("hr_resting".into(), 3),
            ("resp_rate".into(), 3),
            ("spo2".into(), 3),
            ("steps".into(), 600),
            ("weight".into(), 3),
        ]
    );
    assert_eq!(result["accepted"], json!(1440 - 30 + 3 + 3 + 3 + 600 + 3));
    // seconds and milliseconds both land on the right instant
    let first_steps: chrono::DateTime<Utc> =
        sqlx::query_scalar("select min(t) from wearable_sample where profile_id = $1 and metric = 'steps'")
            .bind(sam)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(
        first_steps,
        Utc.timestamp_opt(T0 + 480 * 60, 0).single().unwrap_or_default()
    );
    let weight_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("select min(t) from wearable_sample where profile_id = $1 and metric = 'weight'")
            .bind(sam)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(
        weight_at,
        Utc.timestamp_opt(T0 + 25_200, 0).single().unwrap_or_default()
    );
    // one wearable source per device, named by its alias, keyed by its identifier
    let devices: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as("select provider, device, external_id from wearable_source where profile_id = $1 order by id")
            .bind(sam)
            .fetch_all(&env.db.pool)
            .await?;
    assert_eq!(
        devices,
        vec![
            (
                "gadgetbridge".into(),
                Some("Pulsera de Sam".into()),
                Some("C8:0F:10:00:00:01".into())
            ),
            (
                "gadgetbridge".into(),
                Some("Mi Smart Scale 2".into()),
                Some("C8:0F:10:00:00:02".into())
            ),
        ]
    );
    // the document owns the samples and says what it did not import
    let (doc, flags): (uuid::Uuid, Value) =
        sqlx::query_as("select id, flags from document where profile_id = $1 and mime = 'application/vnd.sqlite3'")
            .bind(sam)
            .fetch_one(&env.db.pool)
            .await?;
    // sleep stages are listed, not guessed; an empty table is not listed
    assert_eq!(
        flags["gadgetbridge"]["not_imported"],
        json!({"XIAOMI_SLEEP_STAGE_SAMPLE": 3})
    );
    let owned = env
        .count(
            "select count(*) from wearable_sample where profile_id = $1 and document_id is not null",
            sam,
        )
        .await?;
    let all = env
        .count("select count(*) from wearable_sample where profile_id = $1", sam)
        .await?;
    assert_eq!(owned, all);
    let days = env
        .count(
            "select count(*) from wearable_rollup where profile_id = $1 and metric = 'steps' and bucket = 'day'",
            sam,
        )
        .await?;
    assert!(days >= 1);

    // the same file again: the same document, nothing new
    let (state, again, _) = job(&env, upload(&env, bytes, "sam").await?).await?;
    assert_eq!(state, "done");
    assert_eq!(again["document_id"], json!(doc.to_string()), "{again}");
    assert_eq!(
        env.count("select count(*) from wearable_sample where profile_id = $1", sam)
            .await?,
        all
    );
    Ok(())
}

#[tokio::test]
async fn another_sqlite_file_fails_for_good_and_writes_nothing() -> R {
    let env = Env::new().await?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("notes.db");
    Connection::open(&path)?
        .execute_batch("create table notes (id integer primary key, body text); insert into notes values (1, 'x');")?;
    let (state, _, err) = job(&env, upload(&env, std::fs::read(&path)?, "sam").await?).await?;
    assert_eq!(state, "dead");
    assert!(err.unwrap_or_default().contains("not a Gadgetbridge database"));
    let mut garbage = b"SQLite format 3\0".to_vec();
    garbage.extend(std::iter::repeat_n(0u8, 4096));
    let (state, _, _) = job(&env, upload(&env, garbage, "sam").await?).await?;
    assert_eq!(state, "dead");
    assert_eq!(
        env.count(
            "select count(*) from wearable_sample where profile_id = $1",
            env.fam.profile("sam")
        )
        .await?,
        0
    );
    Ok(())
}
