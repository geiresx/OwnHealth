//! v0.2 dedup level 2 inside `import.pdf` (ingestion.md 4): every page gets a pHash next to its text
//! simhash, and a document whose pages match an earlier document of the profile is still imported,
//! then filed as one `near_duplicate` review item ("probably the same as document X").
mod support;

use ownhealth_ingest_pdf::import_pdf;
use serde_json::json;
use support::env::{env, fake_normalizer, items, uuid};
use support::generate::{self, layouts, render, scanned};
use uuid::Uuid;

type R = anyhow::Result<()>;

async fn near_of(c: &mut sqlx::PgConnection, doc: Uuid) -> anyhow::Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("select near_duplicate_of from document where id = $1")
            .bind(doc)
            .fetch_one(c)
            .await?,
    )
}

#[tokio::test]
async fn a_scan_of_a_report_we_hold_is_flagged_and_another_report_is_not() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let ls = layouts();
    let (text_pdf, _) = render(&ls[0], None);
    let (other_pdf, _) = render(&ls[2], None);

    let original = e.store_pdf(&mut c, &text_pdf, "informe.pdf").await?;
    import_pdf(&mut c, &e.blobs, original, false).await?;
    let fp: (Option<Vec<u8>>, Option<i64>) =
        sqlx::query_as("select phash, text_simhash from document_page where document_id = $1 and page_no = 1")
            .bind(original)
            .fetch_one(&mut *c)
            .await?;
    assert!(fp.0.is_some() && fp.1.is_some(), "{fp:?}");

    // the same report, printed and scanned: no text layer, so only the pHash can match
    let scan = e.store_pdf(&mut c, &scanned(&text_pdf, 100.0), "escaneo.pdf").await?;
    let s = import_pdf(&mut c, &e.blobs, scan, false).await?;
    assert_eq!(s.reasons.get("near_duplicate"), Some(&1), "{}", s.to_value());
    assert_eq!(near_of(&mut c, scan).await?, Some(original));
    let near = items(&mut c, scan, Some("near_duplicate")).await?;
    assert_eq!(near.len(), 1);
    assert_eq!(near[0]["payload"]["same_as"], json!(original.to_string()));
    assert_eq!(uuid(&near[0]["ref_id"]), original);
    // still processed: the scan waits for extraction like any other
    assert_eq!(items(&mut c, scan, Some("pending_document")).await?.len(), 1);
    // a second run does not file a second item
    import_pdf(&mut c, &e.blobs, scan, true).await?;
    assert_eq!(items(&mut c, scan, Some("near_duplicate")).await?.len(), 1);

    // another report on another template is not "the same"
    let other = e.store_pdf(&mut c, &scanned(&other_pdf, 100.0), "otro.pdf").await?;
    import_pdf(&mut c, &e.blobs, other, false).await?;
    assert_eq!(near_of(&mut c, other).await?, None);
    assert!(items(&mut c, other, Some("near_duplicate")).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn an_edition_of_an_earlier_report_is_linked_not_flagged() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let ls = layouts();
    let (pdf, _) = render(&ls[0], None);
    let reprint = render(
        &generate::Layout {
            meta: vec![("Subject", "reimpresión")],
            ..ls[0].clone()
        },
        None,
    )
    .0;
    let first = e.store_pdf(&mut c, &pdf, "a.pdf").await?;
    import_pdf(&mut c, &e.blobs, first, false).await?;
    let second = e.store_pdf(&mut c, &reprint, "b.pdf").await?;
    let s = import_pdf(&mut c, &e.blobs, second, false).await?;
    // read as an edition of the first, nothing changed: the rows are linked, no review at all
    assert_eq!((s.queued, s.skipped), (0, 12), "{}", s.to_value());
    assert!(items(&mut c, second, None).await?.is_empty());
    assert_eq!(near_of(&mut c, second).await?, None);
    Ok(())
}
