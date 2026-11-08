//! Demo seed: the synthetic Rivera family with enough data for every screen (`demo/`).
//!
//! `ownhealth demo-seed`, or OWNHEALTH_DEMO=1 at startup (seeds once). The family itself lives
//! here because the tests of every lane use it too. The base data (`demo/base.py`: lab reports,
//! records, wearables, the review queue) and each lane's extra data register as demo hooks with
//! `register_demo_hook`; hooks run in `order`, then name, all in one transaction, so a broken
//! hook leaves nothing behind. The base seed registers with order 0 so it runs first.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use chrono::NaiveDate;
use futures::future::BoxFuture;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::access::{ProfileIn, create_household, create_profile, create_user, set_grant};
use crate::blobs::BlobStore;
use crate::config::Settings;
use crate::error::Result;

pub const DEMO_HOUSEHOLD: &str = "Rivera (demo)";
const DEMO_LOCK: i64 = 4242;

/// The date every example in the docs uses.
pub fn as_of() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default()
}

/// | Profile | Born       | Sex    | Login | Alex's role | Sam's role |
/// |---------|------------|--------|-------|-------------|------------|
/// | alex    | 1979-05-14 | male   | Alex  | owner       | viewer     |
/// | sam     | 1981-02-02 | female | Sam   | viewer      | owner      |
/// | lucia   | 2017-06-21 | female | none  | guardian    | guardian   |
/// | carmen  | 1955-01-30 | female | none  | caregiver   | (none)     |
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Family {
    pub household_id: Uuid,
    /// alex, sam
    pub users: HashMap<String, Uuid>,
    /// alex, sam, lucia, carmen
    pub profiles: HashMap<String, Uuid>,
}

impl Family {
    pub fn user(&self, slug: &str) -> Uuid {
        self.users.get(slug).copied().unwrap_or_default()
    }

    pub fn profile(&self, slug: &str) -> Uuid {
        self.profiles.get(slug).copied().unwrap_or_default()
    }
}

fn ymd(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, d)
}

pub async fn create_rivera_family(conn: &mut PgConnection, household_name: &str) -> Result<Family> {
    let h = create_household(&mut *conn, household_name).await?;
    let mut fam = Family {
        household_id: h,
        ..Family::default()
    };
    fam.users.insert(
        "alex".into(),
        create_user(&mut *conn, h, "Alex Rivera", "es", None).await?,
    );
    fam.users.insert(
        "sam".into(),
        create_user(&mut *conn, h, "Sam Rivera", "en", None).await?,
    );
    let people = [
        (
            "alex",
            "Alex Rivera",
            "self",
            ymd(1979, 5, 14),
            "male",
            Some(fam.user("alex")),
        ),
        (
            "sam",
            "Sam Rivera",
            "partner",
            ymd(1981, 2, 2),
            "female",
            Some(fam.user("sam")),
        ),
        ("lucia", "Lucía Rivera", "child", ymd(2017, 6, 21), "female", None),
        ("carmen", "Carmen Ortega", "parent", ymd(1955, 1, 30), "female", None),
    ];
    for (slug, name, rel, born, sex, user) in people {
        let id = create_profile(
            &mut *conn,
            &ProfileIn {
                household_id: h,
                slug: slug.into(),
                display_name: name.into(),
                relationship: Some(rel.into()),
                birth_date: born,
                sex_at_birth: Some(sex.into()),
                country: Some("ES".into()),
                user_id: user,
                unit_pref: None,
            },
        )
        .await?;
        fam.profiles.insert(slug.into(), id);
    }
    let (a, s) = (fam.user("alex"), fam.user("sam"));
    let p = |k: &str| fam.profile(k);
    set_grant(&mut *conn, a, p("alex"), "owner", None, None, None).await?;
    set_grant(&mut *conn, a, p("sam"), "viewer", None, Some(s), None).await?;
    set_grant(&mut *conn, a, p("lucia"), "guardian", None, None, None).await?;
    set_grant(&mut *conn, a, p("carmen"), "caregiver", None, None, None).await?;
    set_grant(&mut *conn, s, p("sam"), "owner", None, None, None).await?;
    set_grant(&mut *conn, s, p("alex"), "viewer", None, Some(a), None).await?;
    set_grant(&mut *conn, s, p("lucia"), "guardian", None, None, None).await?;
    Ok(fam)
}

#[derive(Debug, Clone)]
pub struct DemoContext {
    pub settings: Settings,
    pub blobs: BlobStore,
    pub family: Family,
    pub as_of: NaiveDate,
    /// "alex:2026-09-14" -> lab report document id
    pub documents: HashMap<String, Uuid>,
    /// Named sources other hooks may cite ("alex_vitamin_d" -> "src_a12").
    pub src_ids: HashMap<String, String>,
}

pub type DemoHook = for<'c> fn(&'c mut PgConnection, &'c mut DemoContext) -> BoxFuture<'c, Result<()>>;

#[derive(Clone)]
struct HookEntry {
    order: i32,
    name: String,
    hook: DemoHook,
}

static HOOKS: LazyLock<RwLock<Vec<HookEntry>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a demo hook. Python ORDER values: base 0 (runs first), lanes 100..900 (default 500).
/// Registering the same name again replaces the hook.
pub fn register_demo_hook(name: &str, order: i32, hook: DemoHook) {
    if let Ok(mut hooks) = HOOKS.write() {
        hooks.retain(|h| h.name != name);
        hooks.push(HookEntry {
            order,
            name: name.into(),
            hook,
        });
        hooks.sort_by(|a, b| (a.order, &a.name).cmp(&(b.order, &b.name)));
    }
}

pub fn demo_hook_names() -> Vec<String> {
    HOOKS
        .read()
        .map(|h| h.iter().map(|e| e.name.clone()).collect())
        .unwrap_or_default()
}

pub async fn seed_into(conn: &mut PgConnection, ctx: &mut DemoContext) -> Result<()> {
    let hooks: Vec<HookEntry> = HOOKS.read().map(|h| h.clone()).unwrap_or_default();
    for h in hooks {
        tracing::info!(hook = %h.name, "demo hook");
        (h.hook)(&mut *conn, ctx).await?;
    }
    Ok(())
}

/// Seed the demo household once. Returns false when it already exists (and not `force`).
pub async fn seed_demo(pool: &PgPool, settings: &Settings, blobs: &BlobStore, force: bool) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(DEMO_LOCK)
        .execute(&mut *tx)
        .await?;
    let exists: Option<i32> = sqlx::query_scalar("select 1 from household where name = $1")
        .bind(DEMO_HOUSEHOLD)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_some() && !force {
        return Ok(false);
    }
    let family = create_rivera_family(&mut tx, DEMO_HOUSEHOLD).await?;
    let mut ctx = DemoContext {
        settings: settings.clone(),
        blobs: blobs.clone(),
        family,
        as_of: as_of(),
        documents: HashMap::new(),
        src_ids: HashMap::new(),
    };
    seed_into(&mut tx, &mut ctx).await?;
    tx.commit().await?;
    Ok(true)
}
