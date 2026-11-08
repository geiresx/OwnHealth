//! The base demo seed (`tests/test_demo.py`).

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DEMO_HOUSEHOLD, seed_demo};
use ownhealth_core::testing::TestDb;
use rust_decimal::Decimal;
use uuid::Uuid;

#[tokio::test]
async fn demo_seed_matches_the_alex_fixture() -> anyhow::Result<()> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    assert!(!seed_demo(&db.pool, &settings, &blobs, false).await?); // once
    let p = &db.pool;

    let (value, interp, document_id): (Decimal, String, Uuid) =
        sqlx::query_as("select value_num, interpretation, document_id from observation where src_id = 'src_a12'")
            .fetch_one(p)
            .await?;
    assert_eq!((value, interp.as_str()), (Decimal::from(24), "L"));
    let lpa: Decimal = sqlx::query_scalar("select value_num from observation where src_id = 'src_a07'")
        .fetch_one(p)
        .await?;
    assert_eq!(lpa, Decimal::from(142));
    let ldl: i64 = sqlx::query_scalar(
        "select count(*) from observation o join profile p on p.id = o.profile_id \
         where p.slug = 'alex' and alias_group = 'ldl_c'",
    )
    .fetch_one(p)
    .await?;
    assert_eq!(ldl, 9);
    let pages: i64 = sqlx::query_scalar("select count(*) from document_page where document_id = $1")
        .bind(document_id)
        .fetch_one(p)
        .await?;
    assert_eq!(pages, 2);
    let open: i64 = sqlx::query_scalar(
        "select count(*) from review_item r join household h on h.id = r.household_id \
         where h.name = $1 and r.status = 'open'",
    )
    .bind(DEMO_HOUSEHOLD)
    .fetch_one(p)
    .await?;
    assert_eq!(open, 7);
    let active: Vec<String> = sqlx::query_scalar(
        "select name from medication_statement where src_id in ('src_m02', 'src_m05') \
         and med_status = 'active' order by name",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(active, ["Atorvastatin", "Omeprazole"]);
    let metrics: Vec<String> = sqlx::query_scalar(
        "select distinct r.metric from wearable_rollup r join profile p on p.id = r.profile_id \
         where p.slug = 'alex' order by 1",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(metrics, ["hr_resting", "hrv_rmssd", "sleep_asleep", "steps", "vo2max"]);
    // every observation keeps provenance with a page and a bbox
    let missing: i64 = sqlx::query_scalar(
        "select count(*) from observation o join src s on s.id = o.src_id \
         where s.kind = 'document_region' and (s.page_no is null or s.bbox is null)",
    )
    .fetch_one(p)
    .await?;
    assert_eq!(missing, 0);

    // the wearable values Python's random.Random(20261107) gave: the first and last samples
    let first: Vec<f32> = sqlx::query_scalar(
        "select v from wearable_sample s join profile p on p.id = s.profile_id \
         where p.slug = 'alex' and metric = 'hr_resting' order by t limit 3",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(first, [58.2, 57.6, 58.8]);
    let samples: i64 = sqlx::query_scalar("select count(*) from wearable_sample")
        .fetch_one(p)
        .await?;
    assert_eq!(samples, 373);
    // the named sources other hooks cite all exist
    let named: i64 = sqlx::query_scalar(
        "select count(*) from src where id in ('src_a12', 'src_a07', 'src_a05', 'src_m02', 'src_m05', 'src_n41')",
    )
    .fetch_one(p)
    .await?;
    assert_eq!(named, 6);
    Ok(())
}

#[tokio::test]
async fn a_forced_second_seed_gets_generated_src_ids() -> anyhow::Result<()> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    assert!(seed_demo(&db.pool, &settings, &blobs, true).await?);
    let households: i64 = sqlx::query_scalar("select count(*) from household where name = $1")
        .bind(DEMO_HOUSEHOLD)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(households, 2);
    let a12: i64 = sqlx::query_scalar("select count(*) from src where id = 'src_a12'")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(a12, 1);
    Ok(())
}
