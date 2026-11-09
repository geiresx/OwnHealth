//! The OCR exit criteria with the real engine (ROADMAP v0.2): on the scanned and photographed eval
//! set, on CPU, row F1 >= 0.90, and with the `ocr` profile on every ground-truth row has exactly one
//! outcome, accepted or in the review queue (row by row, or with its whole page when OCR found no table
//! on it): 0 dropped, 0 duplicated.
//! No row OCR misread is committed without a click. The OCR-only injection fixtures (ours, drawn faint
//! on a scan, and `fixtures/labslips/injection/inj-ocr-only.jpg`) stay contained.
//!
//! Needs the models and an optimised build (the image work in oar-ocr is slow unoptimised):
//!
//! ```sh
//! bash scripts/fetch-ocr-models.sh /path/to/models
//! OWNHEALTH_OCR_MODELS_DIR=/path/to/models \
//!   cargo test -p ownhealth-ocr --features engine --release --test exit_criteria -- --ignored --nocapture
//! ```
mod eval;
#[path = "../../ownhealth-ingest-pdf/tests/support/mod.rs"]
mod support;

use support::env::env;

#[tokio::test]
#[ignore = "needs OWNHEALTH_OCR_MODELS_DIR and a --release build; see the file header"]
async fn ocr_exit_criteria_on_the_scanned_and_photographed_set() -> anyhow::Result<()> {
    let models = std::env::var("OWNHEALTH_OCR_MODELS_DIR").expect("OWNHEALTH_OCR_MODELS_DIR: the OCR models");
    let mut e = env().await?;
    e.settings.ocr_models_dir = models.into();
    ownhealth_ocr::register();
    // the system as it ships: lane A's normalizer
    ownhealth_ingest_pdf::use_normalizer(None);
    let mut c = e.conn().await?;
    let mut all = Vec::new();
    for spec in eval::eval_set() {
        let doc = spec.build();
        let id = eval::import_doc(&e, &mut c, &doc, true).await?;
        all.push(eval::outcomes(&mut c, &doc.name, id, &doc.truth).await?);
    }
    let t = eval::totals(&all);
    println!("{}{}", eval::report(&all), serde_json::to_string_pretty(&t)?);
    assert!(t.row_f1 >= 0.90, "row F1 {:.3}", t.row_f1);
    // a page whose table OCR could not find waits whole, as one pending_document item in the review
    // queue: its rows are not dropped, and each is still covered exactly once
    assert_eq!((t.dropped, t.duplicated), (0, 0), "{t:?}");
    assert_eq!(t.accepted + t.review + t.pending, t.truth, "{t:?}");
    assert_eq!(t.false_auto, 0, "{t:?}");

    let (name, pdf, truth) = eval::ocr_injection_fixture();
    let scan = eval::EvalDoc {
        name: name.clone(),
        kind: "scan",
        mime: "application/pdf",
        bytes: eval::image_pdf(
            &eval::render_pages(&pdf, eval::DPI)
                .iter()
                .map(|p| eval::degrade(p, "scan", &name))
                .collect::<Vec<_>>(),
        ),
        truth,
        pages: 1,
    };
    let id = eval::import_doc(&e, &mut c, &scan, true).await?;
    eval::assert_injection_contained(&mut c, id).await?;

    // the committed OCR-only fixture: a photo with the instruction printed into the image
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/labslips/injection");
    let photo = eval::EvalDoc {
        name: "inj-ocr-only".into(),
        kind: "photo",
        mime: "image/jpeg",
        bytes: std::fs::read(dir.join("inj-ocr-only.jpg"))?,
        truth: serde_json::from_slice(&std::fs::read(dir.join("inj-ocr-only.truth"))?)?,
        pages: 1,
    };
    let id = eval::import_doc(&e, &mut c, &photo, true).await?;
    eval::assert_injection_contained(&mut c, id).await?;
    Ok(())
}
