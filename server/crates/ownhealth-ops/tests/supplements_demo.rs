//! The supplements demo hook (migration 1101): Alex's vitamin D3 with a label photo and a decision, and
//! Carmen's iron marked do_not_repurchase. No state_hash moves, and deleting a profile with a label
//! still works.

use std::collections::{BTreeMap, HashMap};

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DemoContext, as_of, create_rivera_family};
use ownhealth_core::testing::TestDb;
use sqlx::PgConnection;
use uuid::Uuid;

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

#[test]
fn the_demo_label_is_a_png_drawn_the_same_every_time() -> anyhow::Result<()> {
    let a = ownhealth_ops::demo_label_png()?;
    assert_eq!(a, ownhealth_ops::demo_label_png()?);
    let img = image::load_from_memory_with_format(&a, image::ImageFormat::Png)?;
    assert_eq!((img.width(), img.height()), (300, 400));
    Ok(())
}

#[tokio::test]
async fn the_supplements_hook_adds_a_label_and_two_decisions_and_moves_no_state_hash() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    let mut tx = db.pool.begin().await?;
    let family = create_rivera_family(&mut tx, "Rivera (supplements)").await?;
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
    let profiles: BTreeMap<String, Uuid> = ctx.family.profiles.clone().into_iter().collect();
    let before = hashes(&mut tx, &profiles).await?;
    ownhealth_ops::seed_supplements(&mut tx, &mut ctx).await?;
    assert_eq!(
        hashes(&mut tx, &profiles).await?,
        before,
        "a demo supplement moved a state_hash"
    );

    let alex = ctx.family.profile("alex");
    // two versions of the vitamin D3, the decision on the current one, the label on both
    let versions: Vec<(String, Option<String>, Option<String>, i64)> = sqlx::query_as(
        "select m.status, m.decision, m.decision_reason, (select count(*) from medication_label l \
         where l.medication_id = m.id) from medication_statement m where m.profile_id = $1 \
         and m.kind = 'supplement' order by m.supersedes nulls first",
    )
    .bind(alex)
    .fetch_all(&mut *tx)
    .await?;
    assert_eq!(
        versions,
        [
            ("superseded".to_string(), None, None, 1),
            (
                "current".to_string(),
                Some("keep".to_string()),
                Some("Solo en invierno; la vuelvo a tomar en noviembre".to_string()),
                1
            )
        ]
    );
    let doc: (Option<String>, String, Option<String>) = sqlx::query_as(
        "select d.doc_type, d.mime, d.filename from document d join medication_label l on l.document_id = d.id \
         join medication_statement m on m.id = l.medication_id where m.profile_id = $1 and m.status = 'current'",
    )
    .bind(alex)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(
        doc,
        (
            Some("other".to_string()),
            "image/png".to_string(),
            Some("etiqueta_vitamina_d3.png".to_string())
        )
    );
    let iron: (String, Option<String>) =
        sqlx::query_as("select name, decision from medication_statement where profile_id = $1 and kind = 'supplement'")
            .bind(ctx.family.profile("carmen"))
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(
        iron,
        ("Hierro bisglicinato".to_string(), Some("do_not_repurchase".to_string()))
    );

    // deleting the demo profile that has the label removes the record, the link and the photo
    let deleted = ownhealth_auth::household::delete_profile(&mut tx, alex).await?;
    assert!(
        deleted.rows.iter().any(|(t, n)| t == "medication_statement" && *n >= 4),
        "{:?}",
        deleted.rows
    );
    let left: i64 = sqlx::query_scalar("select count(*) from medication_label")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(left, 0);
    Ok(())
}

fn zip_text(z: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, name: &str) -> anyhow::Result<String> {
    use std::io::Read;
    let mut out = String::new();
    z.by_name(name)?.read_to_string(&mut out)?;
    Ok(out)
}

#[tokio::test]
async fn the_export_keeps_the_decision_and_the_link_to_the_label_photo() -> anyhow::Result<()> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    let mut tx = db.pool.begin().await?;
    let family = create_rivera_family(&mut tx, "Rivera (supplements export)").await?;
    let mut ctx = DemoContext {
        settings,
        blobs: blobs.clone(),
        family,
        as_of: as_of(),
        documents: HashMap::new(),
        src_ids: HashMap::new(),
    };
    ownhealth_ops::seed_supplements(&mut tx, &mut ctx).await?;
    let alex = ctx.family.profile("alex");
    let mut out = std::io::Cursor::new(Vec::new());
    let manifest = ownhealth_ops::export::export_profile(&mut tx, &blobs, alex, &mut out, "zip").await?;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(out.into_inner()))?;

    // the current version says the decision, its reason and its day; the first version says none
    let statements: Vec<serde_json::Value> = zip_text(&mut z, "fhir/MedicationStatement.ndjson")?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let decisions: Vec<&serde_json::Value> = statements
        .iter()
        .flat_map(|s| s["extension"].as_array().into_iter().flatten())
        .filter(|e| e["url"] == "urn:ownhealth:decision")
        .collect();
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert_eq!(decisions.len(), 1, "{statements:?}");
    let parts = &decisions[0]["extension"];
    assert_eq!(parts[0], serde_json::json!({"url": "code", "valueCode": "keep"}));
    assert_eq!(parts[1]["url"], "reason");
    assert_eq!(parts[2], serde_json::json!({"url": "date", "valueDate": "2026-04-02"}));

    // both versions link the one label photo, which the export carries as a DocumentReference
    let doc: Uuid = sqlx::query_scalar("select id from document where profile_id = $1")
        .bind(alex)
        .fetch_one(&mut *tx)
        .await?;
    let links: Vec<serde_json::Value> = zip_text(&mut z, "ownhealth/medication_label.ndjson")?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(links.len(), 2, "{links:?}");
    assert!(links.iter().all(|l| l["document_id"] == doc.to_string()), "{links:?}");
    assert!(zip_text(&mut z, "fhir/DocumentReference.ndjson")?.contains(&doc.to_string()));
    assert_eq!(manifest["counts"]["ownhealth/medication_label"], 2);
    Ok(())
}
