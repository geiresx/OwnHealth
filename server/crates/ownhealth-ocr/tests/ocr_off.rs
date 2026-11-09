//! The "nothing silent" exit criterion with the `ocr` profile off (ROADMAP v0.2): every scanned and
//! photographed eval page waits as an open `pending_document` item, which is what
//! `list_pending_documents` lists for an outside agent, and no value is read from it.
mod eval;
#[path = "../../ownhealth-ingest-pdf/tests/support/mod.rs"]
mod support;

use serde_json::Value;
use support::env::{env, fake_normalizer};

#[tokio::test]
async fn with_ocr_off_every_eval_page_waits_for_an_agent() -> anyhow::Result<()> {
    let e = env().await?;
    fake_normalizer();
    let mut c = e.conn().await?;
    let specs = eval::eval_set();
    assert!(specs.len() >= 40, "{}", specs.len());
    let (mut scans, mut photos) = (0, 0);
    for spec in &specs {
        let doc = spec.build();
        let id = eval::import_doc(&e, &mut c, &doc, false).await?;
        let items: Vec<Value> = sqlx::query_scalar(
            "select payload from review_item where document_id = $1 and kind = 'pending_document' and status = 'open'",
        )
        .bind(id)
        .fetch_all(&mut *c)
        .await?;
        assert_eq!(items.len(), 1, "{}: {items:?}", doc.name);
        let want: Vec<usize> = (1..=doc.pages).collect();
        assert_eq!(items[0]["pages"], serde_json::json!(want), "{}", doc.name);
        let state: String = sqlx::query_scalar("select ocr_state from document where id = $1")
            .bind(id)
            .fetch_one(&mut *c)
            .await?;
        assert_eq!(state, "off", "{}", doc.name);
        let o = eval::outcomes(&mut c, &doc.name, id, &doc.truth).await?;
        assert_eq!(o.extracted, 0, "{}", doc.name);
        assert_eq!(
            o.pending.len(),
            o.truth,
            "{}: every truth row waits with its page",
            doc.name
        );
        match doc.kind {
            "scan" => scans += doc.pages,
            _ => photos += 1,
        }
    }
    assert!(scans >= 30 && photos >= 10, "{scans} scanned pages, {photos} photos");
    let jobs: i64 = sqlx::query_scalar("select count(*) from job where queue = 'ocr'")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(jobs, 0, "with OCR off nothing is queued for it");
    Ok(())
}
