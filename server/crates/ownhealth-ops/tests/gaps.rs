//! Document gaps on the demo seed (`ownhealth_rules::gaps`): Alex's record has exactly the two gaps
//! the mockup draws, a report from the visit's provider clears the visit gap (and one from another
//! provider does not), each gap is a `gap:<key>` item of the Due list that a snooze or a done hides,
//! and the state body never moves.

use std::collections::HashMap;

use chrono::NaiveDate;
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DemoContext, as_of, create_rivera_family};
use ownhealth_core::testing::TestDb;
use ownhealth_rules::{gaps, prevention};
use serde_json::Value;
use uuid::Uuid;

type R = anyhow::Result<()>;

struct Demo {
    db: TestDb,
    _dir: tempfile::TempDir,
    alex: Uuid,
    household: Uuid,
}

async fn demo() -> anyhow::Result<Demo> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    let mut tx = db.pool.begin().await?;
    let family = create_rivera_family(&mut tx, "Rivera (gaps)").await?;
    let mut ctx = DemoContext {
        settings,
        blobs,
        family,
        as_of: as_of(),
        documents: HashMap::new(),
        src_ids: HashMap::new(),
    };
    ownhealth_ops::seed_base(&mut tx, &mut ctx).await?;
    ownhealth_ops::seed_family(&mut tx, &mut ctx).await?;
    tx.commit().await?;
    Ok(Demo {
        alex: ctx.family.profile("alex"),
        household: ctx.family.household_id,
        db,
        _dir: dir,
    })
}

fn keys(doc: &Value, list: &str) -> Vec<String> {
    doc[list]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["key"].as_str().map(str::to_string))
        .collect()
}

async fn gaps_of(d: &Demo) -> anyhow::Result<Value> {
    let mut c = d.db.pool.acquire().await?;
    Ok(gaps::build_document(&mut c, d.alex, as_of()).await?)
}

/// A document from `provider` collected on `day`, as an upload of Alex's.
async fn report(d: &Demo, provider: &str, day: &str) -> anyhow::Result<Uuid> {
    let sha = format!("{:0>64}", hex_of(provider, day));
    Ok(sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, \
         provider_name, collected_at) values ($1, $2, decode($3, 'hex'), 'informe.pdf', 'application/pdf', 10, \
         'discharge', $4, ($5::date + time '10:00') at time zone 'UTC') returning id",
    )
    .bind(d.household)
    .bind(d.alex)
    .bind(sha)
    .bind(provider)
    .bind(day)
    .fetch_one(&d.db.pool)
    .await?)
}

fn hex_of(a: &str, b: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(format!("{a}{b}").as_bytes()))
}

#[tokio::test]
async fn the_demo_has_exactly_the_two_mockup_gaps() -> R {
    let d = demo().await?;
    let doc = gaps_of(&d).await?;
    assert_eq!(doc["schema"], gaps::SCHEMA);
    assert_eq!(doc["rules"], "gaps 0.1.0");
    let items = doc["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2, "{doc:#}");
    let visit = &items[0];
    assert_eq!(visit["rule"], "visit_without_report");
    assert_eq!(visit["date"], "2025-11-10");
    assert_eq!(visit["provider"], "Traumatología · Hospital Norte");
    assert_eq!(visit["letter"], "spain", "Alex lives in Spain");
    assert!(
        visit["key"]
            .as_str()
            .is_some_and(|k| k.starts_with("visit:2025-11-10:")),
        "{visit}"
    );
    assert_eq!(visit["src"].as_array().map(Vec::len), Some(1), "{visit}");
    let hole = &items[1];
    assert_eq!(hole["rule"], "records_hole");
    assert_eq!(hole["key"], "hole:2016-05-10:2021-11-09");
    assert_eq!(
        (hole["from"].as_str(), hole["to"].as_str()),
        (Some("2016-05-10"), Some("2021-11-09"))
    );
    assert_eq!(hole["src"].as_array().map(Vec::len), Some(2), "{hole}");
    for it in &items {
        assert!(
            it["cite"]
                .as_str()
                .is_some_and(|c| c.starts_with("https://eur-lex.europa.eu/")),
            "{it}"
        );
    }
    // the same record gives the same hash, and the state body knows nothing of gaps
    assert_eq!(doc["gaps_hash"], gaps_of(&d).await?["gaps_hash"]);
    assert_eq!(gaps::gaps_hash(&doc)?, doc["gaps_hash"].as_str().unwrap_or_default());
    Ok(())
}

/// The reverse path: a report from the visit's provider within 30 days clears the gap. One from another
/// provider, or from the same provider after the window, does not: without the provider match the
/// first assertion below goes red.
#[tokio::test]
async fn a_report_from_the_same_provider_clears_the_visit_gap() -> R {
    let d = demo().await?;
    let mut c = d.db.pool.acquire().await?;
    let before = ownhealth_rules::build_state(&mut c, d.alex, as_of(), None).await?;
    drop(c);
    report(&d, "Clínica Sur", "2025-11-12").await?;
    report(&d, "Hospital Norte", "2025-12-20").await?;
    let doc = gaps_of(&d).await?;
    assert_eq!(
        doc["items"][0]["rule"], "visit_without_report",
        "another provider's report, or one after the window, cleared the gap"
    );
    // case and accents folded, and the department's hospital is the same provider
    report(&d, "HOSPITAL NORTE", "2025-11-24").await?;
    let doc = gaps_of(&d).await?;
    assert_eq!(keys(&doc, "items"), ["hole:2016-05-10:2021-11-09"]);
    let mut c = d.db.pool.acquire().await?;
    let after = ownhealth_rules::build_state(&mut c, d.alex, as_of(), None).await?;
    assert_eq!(before["state_hash"], after["state_hash"]);
    Ok(())
}

/// While the 30 days after a visit run, its report may still come: the visit is not a gap yet.
#[tokio::test]
async fn a_visit_is_a_gap_only_once_its_window_has_passed() -> R {
    let d = demo().await?;
    let mut c = d.db.pool.acquire().await?;
    for (day, gap) in [("2025-11-20", false), ("2025-12-10", false), ("2025-12-11", true)] {
        let on = NaiveDate::parse_from_str(day, "%Y-%m-%d")?;
        let doc = gaps::build_document(&mut c, d.alex, on).await?;
        let visit = keys(&doc, "items").iter().any(|k| k.starts_with("visit:"));
        assert_eq!(visit, gap, "{day}");
    }
    Ok(())
}

#[test]
fn provider_names_match_folded_and_in_order() {
    assert!(gaps::same_provider("Traumatología · Hospital Norte", "hospital norte"));
    assert!(gaps::same_provider("Hospital Norte", "Traumatologia - HOSPITAL NORTE"));
    assert!(!gaps::same_provider("Hospital Norte", "Norte Hospital"));
    // other words may stand between the shorter name's words
    assert!(gaps::same_provider("Hospital Norte", "Hospital Universitario Norte"));
    assert!(gaps::same_provider(
        "Hospital Universitario del Norte",
        "hospital norte"
    ));
    assert!(!gaps::same_provider("Hospital Norte", "Hospital Sur"));
    assert!(!gaps::same_provider("Hospital Norte", "LabNorte"));
    assert!(!gaps::same_provider("Hospital Norte", " · "));
}

async fn decide(d: &Demo, key: &str, kind: &str, until: Option<NaiveDate>, document: Option<Uuid>) -> R {
    sqlx::query(
        "insert into prevention_decision (profile_id, item_key, kind, until, reason, document_id) \
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(d.alex)
    .bind(key)
    .bind(kind)
    .bind(until)
    .bind((kind == "dismiss").then_some("asked already"))
    .bind(document)
    .execute(&d.db.pool)
    .await?;
    Ok(())
}

async fn due(d: &Demo) -> anyhow::Result<Value> {
    let mut c = d.db.pool.acquire().await?;
    Ok(prevention::build_document(&mut c, d.alex, as_of()).await?)
}

/// Each gap is a Due item `gap:<key>` (kind document_request); a snooze hides it until its day, and
/// done with the document that came hides it too.
#[tokio::test]
async fn a_gap_is_a_due_item_that_a_snooze_or_done_hides() -> R {
    let d = demo().await?;
    let doc = gaps_of(&d).await?;
    let visit = format!("gap:{}", doc["items"][0]["key"].as_str().unwrap_or_default());
    let hole = "gap:hole:2016-05-10:2021-11-09";
    let due0 = due(&d).await?;
    let shown = keys(&due0, "items");
    assert!(shown.contains(&visit) && shown.contains(&hole.to_string()), "{shown:?}");
    let item = due0["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["key"] == visit.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (item["kind"].as_str(), item["area"].as_str()),
        (Some("document_request"), Some("document"))
    );
    assert!(
        item.get("provider").is_none(),
        "the prevention document holds dates and ids only: {item}"
    );
    let pack = ownhealth_rules::load_pack()?;
    let scopes = ownhealth_core::access::all_scopes();
    let es = prevention::render(&due0, &pack, "es", &scopes)?;
    let row = es["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["key"] == visit.as_str())
        .cloned()
        .unwrap_or_default();
    assert_eq!(row["title"], "Informe de la visita del 2025-11-10");

    decide(&d, hole, "snooze", as_of().succ_opt(), None).await?;
    let due1 = due(&d).await?;
    assert!(!keys(&due1, "items").contains(&hole.to_string()));
    assert!(keys(&due1, "hidden").contains(&hole.to_string()));

    let doc_id = report(&d, "Correo", "2025-11-30").await?;
    decide(&d, &visit, "done", None, Some(doc_id)).await?;
    let due2 = due(&d).await?;
    assert!(!keys(&due2, "items").contains(&visit), "{:?}", keys(&due2, "items"));
    assert!(keys(&due2, "hidden").contains(&visit));
    Ok(())
}

/// A sensitive document clears no gap: the gaps document is the same for every caller, so a visit
/// gap that vanished would tell a caller without read:sensitive that the document exists.
#[tokio::test]
async fn a_sensitive_report_from_the_provider_does_not_clear_the_visit_gap() -> R {
    let d = demo().await?;
    let doc = report(&d, "Hospital Norte", "2025-11-20").await?;
    let src: ownhealth_core::provenance::SrcIn =
        serde_json::from_value(serde_json::json!({"kind": "manual", "profile_id": d.alex, "document_id": doc}))?;
    let r = ownhealth_core::records::RecordIn::from_json(
        "condition",
        serde_json::json!({"profile_id": d.alex, "text": "Synthetic sensitive note", "onset": "2025-11-20",
                           "sensitivity": ["mental_health"]}),
    )?;
    let mut c = d.db.pool.acquire().await?;
    ownhealth_core::records::add_record(
        &mut c,
        &r,
        &ownhealth_core::provenance::SrcRef::New(src),
        ownhealth_core::records::RecordOptions::new("person"),
    )
    .await?;
    drop(c);
    let doc = gaps_of(&d).await?;
    assert_eq!(doc["items"][0]["rule"], "visit_without_report", "{doc:#}");
    Ok(())
}

/// The copy that was asked for often comes back with no date and no provider: marked done on Due with
/// that upload, the visit gap is hidden from its upload day on.
#[tokio::test]
async fn an_undated_upload_marks_a_visit_gap_done() -> R {
    let d = demo().await?;
    let doc = gaps_of(&d).await?;
    let visit = format!("gap:{}", doc["items"][0]["key"].as_str().unwrap_or_default());
    let undated: Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, created_at) \
         values ($1, $2, decode(repeat('ab', 32), 'hex'), 'copia.pdf', 'application/pdf', 10, 'other', \
         ($3::date + time '10:00') at time zone 'UTC') returning id",
    )
    .bind(d.household)
    .bind(d.alex)
    .bind(as_of() - chrono::Duration::days(3))
    .fetch_one(&d.db.pool)
    .await?;
    decide(&d, &visit, "done", None, Some(undated)).await?;
    let due1 = due(&d).await?;
    assert!(keys(&due1, "hidden").contains(&visit), "{:?}", keys(&due1, "items"));
    // only a document gap takes the upload day: a screening still needs the day the document shows
    let e = {
        let mut c = d.db.pool.acquire().await?;
        prevention::evidence(&mut c, d.alex, "document", undated).await?
    };
    let e = e.ok_or_else(|| anyhow::anyhow!("no evidence"))?;
    assert_eq!(e.day, None);
    assert_eq!(e.day_for(&visit), Some(as_of() - chrono::Duration::days(3)));
    assert_eq!(e.day_for("es.colorectal"), None);
    Ok(())
}

/// Alex's status page section for Due, checking that no document request is listed or counted.
async fn status_due(d: &Demo) -> anyhow::Result<Option<Value>> {
    let pack = ownhealth_rules::load_pack()?;
    let mut c = d.db.pool.acquire().await?;
    let body = ownhealth_rules::build_state(&mut c, d.alex, as_of(), None).await?;
    let sec = prevention::status_section(
        &mut c,
        d.alex,
        as_of(),
        &body,
        &pack,
        &ownhealth_core::access::all_scopes(),
    )
    .await?;
    let text = sec.as_ref().map(Value::to_string).unwrap_or_default();
    assert!(!text.contains("gap:") && !text.contains("document"), "{text}");
    Ok(sec)
}

/// The agent's status page has no room for document requests (with one flag, Alex's demo page is
/// already near 2 KB, and budget.rs holds it under): two open gaps add no Due section of their own.
#[tokio::test]
async fn the_status_page_spends_nothing_on_document_requests() -> R {
    let d = demo().await?;
    let mut c = d.db.pool.acquire().await?;
    let alex = ownhealth_core::access::get_profile(&mut c, d.alex)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no alex"))?;
    let body = ownhealth_rules::build_state(&mut c, d.alex, as_of(), None).await?;
    let pack = ownhealth_rules::load_pack()?;
    let doc = prevention::document(&mut c, &alex, as_of(), &body, &pack).await?;
    drop(c);
    let requests = doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["area"] == "document")
        .count();
    assert_eq!(requests, 2, "{doc}");
    assert_eq!(status_due(&d).await?, None);
    Ok(())
}
