//! The benchmark harness (lane C; ROADMAP v0.2 "Benchmark harness", ingestion.md 13): pages/s and row
//! F1 per engine on the scanned and photographed eval set, printed as JSON and as the table in
//! `docs/ocr-benchmark.md`. Run it with
//! `OWNHEALTH_OCR_MODELS_DIR=<models> cargo run --release -p ownhealth-ocr --features engine --example bench`.

use serde::Serialize;

/// One engine's numbers on one slice of the eval set.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BenchReport {
    pub engine: String,
    pub engine_version: String,
    /// scan | photo | all
    pub set: String,
    pub pages: usize,
    /// engine time only: render, detection and recognition of one page
    pub mean_seconds_per_page: f64,
    pub max_seconds_per_page: f64,
    pub pages_per_second: f64,
    pub row_precision: f64,
    pub row_recall: f64,
    pub row_f1: f64,
    pub accepted: usize,
    pub review: usize,
    /// rows on a page OCR found no table on: the page waits whole, as one `pending_document`
    pub pending: usize,
    pub dropped: usize,
    pub duplicated: usize,
}

impl BenchReport {
    /// The markdown table of `docs/ocr-benchmark.md`.
    pub fn markdown(reports: &[BenchReport]) -> String {
        let mut s = String::from(
            "| Engine | Set | Pages | Row F1 | Precision | Recall | Mean s/page | Max s/page | Pages/s | Accepted | Review | Pending page | Dropped | Duplicated |\n\
             |---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        for r in reports {
            s.push_str(&format!(
                "| {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.2} | {:.2} | {:.2} | {} | {} | {} | {} | {} |\n",
                r.engine_version,
                r.set,
                r.pages,
                r.row_f1,
                r.row_precision,
                r.row_recall,
                r.mean_seconds_per_page,
                r.max_seconds_per_page,
                r.pages_per_second,
                r.accepted,
                r.review,
                r.pending,
                r.dropped,
                r.duplicated
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_one_row_per_report() {
        let r = BenchReport {
            engine_version: "v".into(),
            set: "scan".into(),
            pages: 3,
            row_f1: 0.9512,
            pages_per_second: 0.5,
            ..BenchReport::default()
        };
        let md = BenchReport::markdown(&[
            r.clone(),
            BenchReport {
                set: "photo".into(),
                ..r
            },
        ]);
        assert_eq!(md.lines().count(), 4);
        assert!(md.contains("| v | scan | 3 | 0.951 |"), "{md}");
    }
}
