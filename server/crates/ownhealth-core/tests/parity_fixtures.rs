//! Byte-for-byte checks against outputs the Python backend produced (tests/fixtures/).

use ownhealth_core::canonical::{canonical_json, state_hash};
use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use serde_json::Value;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

#[test]
fn canonical_json_and_state_hash_match_python() -> anyhow::Result<()> {
    let fx: Value = serde_json::from_str(&std::fs::read_to_string(format!("{FIXTURES}/canonical.json"))?)?;
    let input = &fx["input"];
    let mut body = input.clone();
    if let Some(m) = body.as_object_mut() {
        m.shift_remove("state_hash");
    }
    assert_eq!(
        String::from_utf8(canonical_json(&body)?)?,
        fx["canonical"].as_str().unwrap_or_default()
    );
    assert_eq!(state_hash(input)?, fx["state_hash"].as_str().unwrap_or_default());
    Ok(())
}

#[test]
fn the_demo_pdf_writer_matches_python() -> anyhow::Result<()> {
    let (pdf, boxes) = lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe (análisis)",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id: "LN-T",
        pages: vec![
            vec![
                LabRow::new("Colesterol LDL", "97", "mg/dL", "< 130"),
                LabRow::new("Vitamina D", "24", "ng/mL", "30 - 100").flagged("L"),
                LabRow::new("Ácido úrico €", "5,1", "mg/dL", "3.5 - 7.2"),
            ],
            vec![LabRow::new("TSH", "2.1", "mUI/L", "0.4 - 4.0")],
        ],
    });
    assert!(
        pdf == std::fs::read(format!("{FIXTURES}/demo-report.pdf"))?,
        "PDF bytes differ from Python's"
    );
    let want: Vec<Vec<[f64; 4]>> =
        serde_json::from_str(&std::fs::read_to_string(format!("{FIXTURES}/demo-report-boxes.json"))?)?;
    assert_eq!(boxes, want);
    Ok(())
}
