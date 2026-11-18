//! Migration 1111 (consent for cross-profile hints) on a 0.3.0 database: the 0.3.0 schema with the
//! 0.3.0 demo seed, then the v0.3 migrations, then the family demo rows and a share in each direction
//! between Alex and Carmen. Every profile's state_hash stays the same at each step: consent and hints
//! live in the family document, never in the hashed state body.

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
async fn a_030_database_migrates_and_gains_shares_with_every_state_hash_kept() -> anyhow::Result<()> {
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
    assert!(applied.contains(&1111), "{applied:?}");
    // new connections after the upgrade, as the server restarts (see family_upgrade.rs)
    db.pool.close().await;
    db.pool = sqlx::PgPool::connect(&db.url).await?;
    let mut c = db.pool.acquire().await?;
    assert_eq!(
        hashes(&mut c, &profiles).await?,
        before,
        "the migration moved a state_hash"
    );

    let mut tx = db.pool.begin().await?;
    ownhealth_ops::seed_family(&mut tx, &mut ctx).await?;
    let (alex, carmen) = (profiles["alex"], profiles["carmen"]);
    for (source, target) in [(carmen, alex), (alex, carmen)] {
        sqlx::query(
            "insert into family_share (source_profile_id, target_profile_id, categories) \
             values ($1, $2, array['labs', 'cascade'])",
        )
        .bind(source)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    assert_eq!(hashes(&mut c, &profiles).await?, before, "shares moved a state_hash");
    Ok(())
}
