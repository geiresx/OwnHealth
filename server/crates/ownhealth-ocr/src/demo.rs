//! Demo data for lane C (RUST-CONTRACTS 10.6, order 1030): one scanned page waiting for OCR. It is a
//! phone-photo-like PNG of Lucía's latest synthetic LabNorte report, stored as an image document and
//! sent through `import.pdf`: with the `ocr` profile on it waits on the `ocr` queue, with it off it
//! waits as a `pending_document` for an outside agent.

use ownhealth_core::demo::DemoContext;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::{Error, Result};
use ownhealth_ingest_pdf::{ImportOptions, import_pdf_opts, render_page_png};
use sqlx::PgConnection;

/// Low enough to look like a quick phone capture, high enough for OCR.
const DEMO_DPI: f64 = 150.0;

pub async fn seed_demo(conn: &mut PgConnection, ctx: &mut DemoContext) -> Result<()> {
    let Some(report) = ctx
        .documents
        .iter()
        .filter(|(k, _)| k.starts_with("lucia:"))
        .max_by(|a, b| a.0.cmp(b.0))
        .map(|(_, id)| *id)
    else {
        return Ok(());
    };
    let sha: Vec<u8> = sqlx::query_scalar("select blob_sha256 from document where id = $1")
        .bind(report)
        .fetch_one(&mut *conn)
        .await?;
    let blobs = ctx.blobs.clone();
    let png = tokio::task::spawn_blocking(move || -> Result<(String, i64)> {
        let png = render_page_png(&blobs, &hex::encode(sha), 1, DEMO_DPI / 72.0)?;
        let size = png.len() as i64;
        Ok((blobs.put_bytes(&png)?, size))
    })
    .await
    .map_err(|e| Error::Io(std::io::Error::other(format!("demo render task failed: {e}"))))??;
    let doc = create_document(
        &mut *conn,
        &NewDocument {
            filename: Some("foto-analitica-lucia.png"),
            ..NewDocument::upload(
                ctx.family.household_id,
                Some(ctx.family.profile("lucia")),
                &png.0,
                "image/png",
                png.1,
            )
        },
    )
    .await?;
    import_pdf_opts(
        conn,
        &ctx.blobs,
        doc.id,
        false,
        ImportOptions {
            threshold: ctx.settings.auto_accept_threshold,
            ocr: ctx.settings.ocr,
        },
    )
    .await?;
    Ok(())
}
