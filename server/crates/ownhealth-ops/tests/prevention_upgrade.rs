//! Migration 1130 (prevention decisions and answers) on a 0.3.0 database: the 0.3.0 schema with the
//! 0.3.0 demo seed, then the v0.3 migrations, then a decision and an answer on every profile. Every
//! profile's state_hash stays the same at each step: what a person decides about a due item never
//! enters the hashed state body.

use std::collections::{BTreeMap, HashMap};

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DemoContext, as_of, create_rivera_family};
use ownhealth_core::migrate::{embedded, migrate};
use ownhealth_core::testing::TestDb;
use sqlx::PgConnection;
use uuid::Uuid;

/// The last migration 0.3.0 shipped.
const V030: i32 = 1052;

async fn hashes(
    conn: &mut PgConnection,
    profiles: &BTreeMap<String, Uuid>,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (slug, id) in profiles {
        let body = ownhealth_rules::build_state(&mut *conn, *id, as_of(), None).await?;
        out.insert(
            slug.clone(),
            body["state_hash"].as_str().unwrap_or_default().to_string(),
        );
    }
    Ok(out)
}

#[tokio::test]
async fn a_030_database_migrates_and_takes_decisions_with_every_state_hash_kept() -> anyhow::Result<()> {
    let mut db = TestDb::empty().await?;
    let all = embedded()?;
    let old: Vec<_> = all.iter().filter(|m| m.version <= V030).cloned().collect();
    migrate(&db.pool, &old).await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    let mut tx = db.pool.begin().await?;
    let family = create_rivera_family(&mut tx, "Rivera (0.3.0)").await?;
    let mut ctx = DemoContext {
        settings,
        blobs,
        family,
        as_of: as_of(),
        documents: HashMap::new(),
        src_ids: HashMap::new(),
    };
    ownhealth_ops::seed_base(&mut tx, &mut ctx).await?;
    tx.commit().await?;
    let profiles: BTreeMap<String, Uuid> = ctx.family.profiles.clone().into_iter().collect();
    let mut c = db.pool.acquire().await?;
    let before = hashes(&mut c, &profiles).await?;
    assert_eq!(before.len(), 4);
    drop(c);

    let applied = migrate(&db.pool, &all).await?;
    assert!(applied.contains(&1130), "{applied:?}");
    // new connections after the upgrade (1100 changes a row type; see family_upgrade.rs)
    db.pool.close().await;
    db.pool = sqlx::PgPool::connect(&db.url).await?;
    let mut c = db.pool.acquire().await?;
    assert_eq!(
        hashes(&mut c, &profiles).await?,
        before,
        "the migration moved a state_hash"
    );

    for id in profiles.values() {
        sqlx::query(
            "insert into prevention_decision (profile_id, item_key, kind, reason) \
             values ($1, 'infect.hcv.once_adult', 'dismiss', 'synthetic')",
        )
        .bind(id)
        .execute(&mut *c)
        .await?;
        let src: String = sqlx::query_scalar(
            "insert into src (id, profile_id, kind, actor_kind) values ('src_' || md5(random()::text), $1, 'manual', 'user') \
             returning id",
        )
        .bind(id)
        .fetch_one(&mut *c)
        .await?;
        sqlx::query(
            "insert into prevention_answer (profile_id, key, value, src_id) values ($1, 'hysterectomy', 'yes', $2)",
        )
        .bind(id)
        .bind(src)
        .execute(&mut *c)
        .await?;
    }
    assert_eq!(
        hashes(&mut c, &profiles).await?,
        before,
        "a decision or an answer moved a state_hash"
    );
    Ok(())
}
