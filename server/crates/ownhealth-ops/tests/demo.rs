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
    // Alex's traumatology visit with no report, typed by hand
    let visits: Vec<(String, String)> = sqlx::query_as(
        "select to_char(e.period_start at time zone 'UTC', 'YYYY-MM-DD'), s.kind from encounter e \
         join src s on s.id = e.src_id join profile p on p.id = e.profile_id where p.slug = 'alex'",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(visits, [("2025-11-10".to_string(), "manual".to_string())]);
    // and one dated report that is not a lab report, so the timeline shows a document event
    let docs: Vec<(String, String, String)> = sqlx::query_as(
        "select d.doc_type, d.filename, to_char(d.collected_at at time zone 'UTC', 'YYYY-MM-DD') from document d \
         join profile p on p.id = d.profile_id where p.slug = 'alex' and d.doc_type = 'ecg'",
    )
    .fetch_all(p)
    .await?;
    assert_eq!(
        docs,
        [(
            "ecg".to_string(),
            "ECG_Hospital_Norte_2024-02-14.pdf".to_string(),
            "2024-02-14".to_string()
        )]
    );
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
    // each household's vitamin D proposal cites its own household's source, not the first seed's src_a12
    let own: Vec<(bool, bool)> = sqlx::query_as(
        "select s.id = 'src_a12', p.household_id = r.household_id from review_item r \
         join src s on s.id = r.context_src_ids[1] join profile p on p.id = s.profile_id \
         where r.kind = 'agent_proposal' and r.payload->'payload'->>'text' like 'Ask about a winter vitamin D%' \
         and r.payload->'payload'->>'attach_to' = s.id and r.payload->'based_on'->>0 = s.id \
         order by r.created_at",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(own, vec![(true, true), (false, true)]);
    Ok(())
}
