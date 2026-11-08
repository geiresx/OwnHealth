//! Builders shared by the core tests (`tests/factories.py`). Synthetic values only.
#![allow(dead_code)]

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::demo_pdf::{BBox, LabReport, LabRow, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::observations::ObservationIn;
use ownhealth_core::provenance::SrcIn;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

pub type R = anyhow::Result<()>;

pub async fn family(conn: &mut PgConnection) -> anyhow::Result<Family> {
    Ok(create_rivera_family(conn, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?)
}

pub fn obs(profile_id: Uuid, value: &str) -> ObservationIn {
    obs_with(profile_id, value, "ldl_c", "13457-7", "2026-09-14")
}

pub fn obs_with(profile_id: Uuid, value: &str, group: &str, loinc: &str, day: &str) -> ObservationIn {
    let v = json!({
        "profile_id": profile_id,
        "printed_name": "Colesterol LDL",
        "printed_value": value,
        "printed_unit": "mg/dL",
        "printed_ref_range": "< 130",
        "value_num": value,
        "ref_high": "130",
        "loinc": loinc,
        "alias_group": group,
        "unit_ucum": "mg/dL",
        "value_canonical": value,
        "unit_canonical": "mg/dL",
        "derivation_version": "test",
        "effective_at": format!("{day}T08:00:00+00:00"),
    });
    ObservationIn::from_json(v).unwrap_or_else(|e| panic!("bad test observation: {e}"))
}

pub fn manual_src() -> SrcIn {
    SrcIn::manual(None)
}

/// A one-page synthetic LabNorte PDF stored as a document. Returns (document_id, bboxes).
pub async fn lab_document(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    fam: &Family,
    slug: &str,
) -> anyhow::Result<(Uuid, Vec<BBox>)> {
    let (pdf, boxes) = lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id: "LN-T",
        pages: vec![vec![
            LabRow::new("Colesterol LDL", "97", "mg/dL", "< 130"),
            LabRow::new("Vitamina D", "24", "ng/mL", "30 - 100").flagged("L"),
        ]],
    });
    let sha = blobs.put_bytes(&pdf)?;
    let r = create_document(
        conn,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            ..NewDocument::upload(
                fam.household_id,
                Some(fam.profile(slug)),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?;
    Ok((r.id, boxes.into_iter().next().unwrap_or_default()))
}
