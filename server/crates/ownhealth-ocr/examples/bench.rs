//! The OCR benchmark (lane C): every document of the scanned and photographed eval set goes through
//! `import.pdf` and `ocr.document` with the real engine, into a throwaway database (scripts/test-db.sh).
//! Prints one line per document, then the JSON reports and the markdown table for
//! `docs/ocr-benchmark.md`.
//!
//! ```sh
//! OWNHEALTH_OCR_MODELS_DIR=<models> cargo run --release -p ownhealth-ocr --features engine --example bench
//! ```
#[path = "../tests/eval/mod.rs"]
mod eval;
#[path = "../../ownhealth-ingest-pdf/tests/support/mod.rs"]
mod support;

use ownhealth_ocr::bench::BenchReport;

fn report(set: &str, outcomes: &[&eval::Outcomes], seconds: &[f64]) -> BenchReport {
    let t = eval::totals(outcomes.iter().copied());
    let total: f64 = seconds.iter().sum();
    BenchReport {
        engine: ownhealth_ocr::ENGINE.into(),
        engine_version: ownhealth_ocr::ENGINE_VERSION.into(),
        set: set.into(),
        pages: seconds.len(),
        mean_seconds_per_page: total / seconds.len().max(1) as f64,
        max_seconds_per_page: seconds.iter().copied().fold(0.0, f64::max),
        pages_per_second: seconds.len() as f64 / total.max(1e-9),
        row_precision: t.row_precision,
        row_recall: t.row_recall,
        row_f1: t.row_f1,
        accepted: t.accepted,
        review: t.review,
        pending: t.pending,
        dropped: t.dropped,
        duplicated: t.duplicated,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let models =
        std::env::var("OWNHEALTH_OCR_MODELS_DIR").map_err(|_| anyhow::anyhow!("set OWNHEALTH_OCR_MODELS_DIR"))?;
    let mut e = support::env::env().await?;
    e.settings.ocr_models_dir = models.into();
    ownhealth_ocr::register();
    ownhealth_ingest_pdf::use_normalizer(None);
    let mut c = e.conn().await?;
    let mut rows: Vec<(String, eval::Outcomes, Vec<f64>)> = Vec::new();
    for spec in eval::eval_set() {
        let doc = spec.build();
        let id = eval::import_doc(&e, &mut c, &doc, true).await?;
        let o = eval::outcomes(&mut c, &doc.name, id, &doc.truth).await?;
        let secs: Vec<f32> = sqlx::query_scalar(
            "select seconds from ocr_run where document_id = $1 and status = 'done' order by page_no",
        )
        .bind(id)
        .fetch_all(&mut *c)
        .await?;
        let secs: Vec<f64> = secs.into_iter().map(f64::from).collect();
        eprintln!("{}", eval::report(std::slice::from_ref(&o)).trim_end());
        rows.push((doc.kind.to_string(), o, secs));
    }
    let mut reports = Vec::new();
    for set in ["scan", "photo", "all"] {
        let pick: Vec<&(String, eval::Outcomes, Vec<f64>)> =
            rows.iter().filter(|(k, _, _)| set == "all" || k == set).collect();
        let outcomes: Vec<&eval::Outcomes> = pick.iter().map(|(_, o, _)| o).collect();
        let seconds: Vec<f64> = pick.iter().flat_map(|(_, _, s)| s.iter().copied()).collect();
        reports.push(report(set, &outcomes, &seconds));
    }
    println!("{}", serde_json::to_string_pretty(&reports)?);
    println!("{}", BenchReport::markdown(&reports));
    Ok(())
}
