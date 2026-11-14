//! Supplement decisions and label photos (migration 1101) leave the state body alone: an upgrade from
//! the 0.3.0 schema keeps every state_hash, a supplement with a decision hashes the same as without one,
//! and six supplements with decisions still fit the status budget.

mod common;

use common::{EXPECTED, FIXTURES, Seeded, as_of, install_fake_genome, seeded};
use ownhealth_core::migrate::{embedded, migrate, migrate_embedded};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_core::testing::TestDb;
use ownhealth_rules::{MAX_STATE_BYTES, build_state, canonical_json, render, state_hash};
use serde_json::json;

type R = anyhow::Result<()>;

const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];
/// The last migration 0.3.0 shipped.
const V0_3_0: i32 = 1052;
/// A decision reason no output may carry.
const MARKER: &str = "Decision reason marker 7f3a";

async fn hashes(s: &Seeded) -> anyhow::Result<Vec<(String, String)>> {
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        out.push((slug.to_string(), state_hash(&body)?));
    }
    Ok(out)
}

fn expected() -> Vec<(String, String)> {
    SLUGS
        .iter()
        .map(|s| {
            (
                s.to_string(),
                EXPECTED["bodies"][s]["state_hash"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn upgrading_a_0_3_0_database_to_1101_keeps_every_state_hash() -> R {
    install_fake_genome();
    let reference = seeded().await?;
    let db = TestDb::empty().await?;
    let old: Vec<_> = embedded()?.into_iter().filter(|m| m.version <= V0_3_0).collect();
    migrate(&db.pool, &old).await?;
    let rows = std::fs::read_to_string(format!("{FIXTURES}/seed.sql"))?;
    let script = format!("begin; set local session_replication_role = replica;\n{rows}\ncommit;");
    sqlx::raw_sql(sqlx::AssertSqlSafe(script)).execute(&db.pool).await?;
    let mut s = Seeded { db };
    for slug in SLUGS {
        s.set_findings(slug, reference.findings(slug));
    }
    assert_eq!(hashes(&s).await?, expected(), "before the upgrade");
    let applied = migrate_embedded(s.pool()).await?;
    assert!(applied.contains(&1101), "{applied:?}");
    // new connections: the old ones hold plans for the old medication_statement row type
    s.db.pool.close().await;
    s.db.pool = sqlx::PgPool::connect(&s.db.url).await?;
    assert_eq!(hashes(&s).await?, expected(), "after the upgrade");
    let decided: i64 = sqlx::query_scalar(
        "select count(*) from medication_statement where decision is not null or decision_reason is not null \
         or decision_on is not null",
    )
    .fetch_one(s.pool())
    .await?;
    let labels: i64 = sqlx::query_scalar("select count(*) from medication_label")
        .fetch_one(s.pool())
        .await?;
    assert_eq!((decided, labels), (0, 0), "old rows read no decision and no label");
    Ok(())
}

#[tokio::test]
async fn a_decision_or_a_label_does_not_move_the_hash() -> R {
    let s = seeded().await?;
    assert_eq!(hashes(&s).await?, expected());
    // Same rows, same src ids; only the decision differs. The lifecycle trigger forbids this update in
    // the app (a decision is a new version there), so the test writes it as the seed does.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "begin; set local session_replication_role = replica; \
         update medication_statement set decision = 'do_not_repurchase', decision_reason = '{MARKER}', \
         decision_on = '2026-10-01'; commit;"
    )))
    .execute(s.pool())
    .await?;
    // and a label photo on every medicine
    let household: uuid::Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(s.pid("alex"))
        .fetch_one(s.pool())
        .await?;
    sqlx::query(
        "with d as (insert into document (household_id, profile_id, blob_sha256, mime, size_bytes, doc_type) \
         select $1, profile_id, sha256(id::text::bytea), 'image/png', 1, 'other' from medication_statement \
         returning id, profile_id) \
         insert into medication_label (medication_id, document_id) \
         select m.id, d.id from medication_statement m join d on d.profile_id = m.profile_id",
    )
    .bind(household)
    .execute(s.pool())
    .await?;
    let n: i64 = sqlx::query_scalar("select count(*) from medication_statement where decision is not null")
        .fetch_one(s.pool())
        .await?;
    assert!(n >= 3, "every seeded medicine now has a decision");
    assert_eq!(hashes(&s).await?, expected());
    Ok(())
}

#[tokio::test]
async fn six_supplements_with_decisions_fit_the_status_budget_and_say_nothing_of_the_decision() -> R {
    let s = seeded().await?;
    let carmen = s.pid("carmen");
    let mut tx = s.pool().begin().await?;
    for (i, name) in [
        "Vitamin D3",
        "Omega-3 fish oil",
        "Magnesium citrate",
        "Iron bisglycinate",
        "Vitamin B12",
        "Calcium carbonate",
    ]
    .iter()
    .enumerate()
    {
        let decision = ["keep", "stop", "do_not_repurchase"][i % 3];
        let rec = RecordIn::from_json(
            "medication",
            json!({"profile_id": carmen, "kind": "supplement", "name": name, "strength": "1000 mg",
                   "started": "2026-09-01", "decision": decision, "decision_on": "2026-10-01",
                   "decision_reason": format!("{MARKER}: a long reason the person wrote about this supplement")}),
        )?;
        add_record(
            &mut tx,
            &rec,
            &SrcRef::New(SrcIn::new("manual")),
            RecordOptions::new("person"),
        )
        .await?;
    }
    let body = build_state(&mut tx, carmen, as_of(), None).await?;
    assert_eq!(body["supplements"].as_array().map(Vec::len), Some(6), "{body}");
    let bytes = String::from_utf8(canonical_json(&body)?)?;
    assert!(bytes.len() <= MAX_STATE_BYTES, "{}", bytes.len());
    for lang in ["es", "en"] {
        let status = render(&body, lang, "as_reported", "pseudonymous", None)?;
        let text = serde_json::to_string(&status)?;
        for word in [MARKER, "decision", "do_not_repurchase"] {
            assert!(!text.contains(word), "{lang}: {word} in {text}");
        }
    }
    assert!(!bytes.contains(MARKER) && !bytes.contains("decision"));
    Ok(())
}
