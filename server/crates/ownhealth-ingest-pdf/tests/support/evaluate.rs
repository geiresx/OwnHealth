//! The eval of the lab PDF import on the synthetic set (0.1.1's `fixtures/labslips/evaluate.py`; ingestion.md
//! 13, the ROADMAP v0.1 exit criteria). Each slip is imported end to end into its own synthetic
//! household and scored:
//!
//! - row F1: extracted candidate rows vs truth, matched on page, printed name and value
//! - auto-accept rate: truth rows committed without a click / truth rows
//! - false auto-accept: auto-accepted rows whose value, unit or LOINC differ from truth (gate: 0)
//! - dropped: truth rows that reached neither the record nor the review queue (gate: 0)
//! - page+bbox: auto-accepted rows whose src has the page and a bbox around the truth row

use std::collections::HashMap;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::ingest::ImportSummary;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::{PgConnection, Row as _};
use uuid::Uuid;

use super::matching::{Extracted, TruthRow, inside, key, score_rows};

#[derive(Debug, Default)]
pub struct SlipResult {
    pub slip: String,
    pub truth: usize,
    pub extracted: usize,
    pub tp: usize,
    pub auto: usize,
    pub false_auto: Vec<String>,
    pub dropped: Vec<String>,
    pub auto_with_bbox: usize,
    pub pending_items: i64,
    pub summary: Value,
}

fn family_key(slip: &str) -> String {
    match slip.rsplit_once("-amend-") {
        Some((head, _)) => format!("{head}-amend"),
        None => slip.to_string(),
    }
}

pub async fn import_slip(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    family: &Family,
    name: &str,
    pdf: &[u8],
    truth: &Value,
) -> anyhow::Result<(Uuid, ImportSummary)> {
    let sha = blobs.put_bytes(pdf)?;
    let person = truth["person"].as_str().unwrap_or("alex");
    let filename = format!("{name}.pdf");
    let doc = create_document(
        &mut *conn,
        &NewDocument {
            filename: Some(&filename),
            ..NewDocument::upload(
                family.household_id,
                family.profiles.get(person).copied(),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?;
    let summary = ownhealth_ingest_pdf::import_pdf(&mut *conn, blobs, doc.id, false).await?;
    Ok((doc.id, summary))
}

fn f64s(v: Option<Vec<f32>>) -> Vec<f64> {
    v.unwrap_or_default().into_iter().map(f64::from).collect()
}

pub async fn score_slip(
    conn: &mut PgConnection,
    name: &str,
    truth: &Value,
    document_id: Uuid,
    summary: &ImportSummary,
) -> anyhow::Result<SlipResult> {
    let rows = sqlx::query(
        "select c.page_no, c.bbox, c.cells, c.state, o.value_num, o.unit_ucum, o.loinc, \
         s.page_no as src_page, s.bbox as src_bbox from candidate_row c join extraction_run r \
         on r.id = c.extraction_run_id left join observation o on o.id = c.observation_id \
         left join src s on s.id = o.src_id where r.document_id = $1",
    )
    .bind(document_id)
    .fetch_all(&mut *conn)
    .await?;
    let truth_rows: Vec<TruthRow> = truth["rows"]
        .as_array()
        .map(|a| a.iter().map(TruthRow::from_json).collect())
        .unwrap_or_default();
    let mut extracted = Vec::new();
    for r in &rows {
        let cells: Value = r.try_get("cells")?;
        let text = |k: &str| cells.get(k).and_then(|c| c["text"].as_str()).unwrap_or("").to_string();
        extracted.push(Extracted {
            page: i64::from(r.try_get::<i32, _>("page_no")?),
            name: text("name"),
            value: text("value"),
            unit: text("unit"),
            bbox: f64s(r.try_get("bbox")?),
        });
    }
    let s = score_rows(&truth_rows, &extracted);
    let mut res = SlipResult {
        slip: name.into(),
        truth: truth_rows.len(),
        extracted: extracted.len(),
        tp: s.tp,
        summary: summary.to_value(),
        ..SlipResult::default()
    };
    let mut by_key: HashMap<_, Vec<&TruthRow>> = HashMap::new();
    for t in &truth_rows {
        by_key.entry(key(t.page, &t.name, &t.value)).or_default().push(t);
    }
    for (e, r) in extracted.iter().zip(&rows) {
        let state: String = r.try_get("state")?;
        if state != "auto_accepted" {
            continue;
        }
        res.auto += 1;
        let label = format!("p{} {}={}", e.page, e.name, e.value);
        let Some(t) = by_key.get(&key(e.page, &e.name, &e.value)).and_then(|c| c.first()) else {
            res.false_auto.push(format!("{label} (no such truth row)"));
            continue;
        };
        let want: Option<Decimal> = t.value_num.as_deref().and_then(|v| v.parse().ok());
        let got: Option<Decimal> = r.try_get("value_num")?;
        let ucum: Option<String> = r.try_get("unit_ucum")?;
        let loinc: Option<String> = r.try_get("loinc")?;
        let ucum = ucum.filter(|u| !u.is_empty());
        if got != want || ucum != t.ucum.clone().filter(|u| !u.is_empty()) || loinc.as_deref() != Some(&t.loinc) {
            res.false_auto.push(format!(
                "{label} got {got:?} {ucum:?} {loinc:?}, truth {want:?} {:?} {}",
                t.ucum, t.loinc
            ));
        }
        let src_page: Option<i32> = r.try_get("src_page")?;
        let src_bbox = f64s(r.try_get("src_bbox")?);
        if src_page.map(i64::from) == Some(t.page) && !src_bbox.is_empty() && inside(&t.bbox, &src_bbox) {
            res.auto_with_bbox += 1;
        }
    }
    for t in &truth_rows {
        if !extracted.iter().any(|e| e.page == t.page && inside(&t.bbox, &e.bbox)) {
            res.dropped.push(format!("p{} {}={}", t.page, t.name, t.value));
        }
    }
    res.pending_items =
        sqlx::query_scalar("select count(*) from review_item where document_id = $1 and kind = 'pending_document'")
            .bind(document_id)
            .fetch_one(&mut *conn)
            .await?;
    Ok(res)
}

pub async fn evaluate(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    slips: &[(String, Vec<u8>, Value)],
) -> anyhow::Result<Vec<SlipResult>> {
    let mut families: HashMap<String, Family> = HashMap::new();
    let mut out = Vec::new();
    for (name, pdf, truth) in slips {
        let fk = family_key(name);
        if !families.contains_key(&fk) {
            let fam = create_rivera_family(&mut *conn, &format!("Eval {fk}")).await?;
            families.insert(fk.clone(), fam);
        }
        let (doc, summary) = import_slip(&mut *conn, blobs, &families[&fk], name, pdf, truth).await?;
        out.push(score_slip(&mut *conn, name, truth, doc, &summary).await?);
    }
    Ok(out)
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Totals {
    pub slips: usize,
    pub truth_rows: usize,
    pub extracted_rows: usize,
    pub row_f1: f64,
    pub auto_accepted: usize,
    pub auto_accept_rate: f64,
    pub false_auto_accepts: usize,
    pub dropped: usize,
    pub auto_with_page_bbox: usize,
}

fn r4(x: f64) -> f64 {
    format!("{x:.4}").parse().unwrap_or(x)
}

pub fn totals(results: &[SlipResult]) -> Totals {
    let truth: usize = results.iter().map(|r| r.truth).sum();
    let ext: usize = results.iter().map(|r| r.extracted).sum();
    let tp: usize = results.iter().map(|r| r.tp).sum();
    let auto: usize = results.iter().map(|r| r.auto).sum();
    let p = if ext > 0 { tp as f64 / ext as f64 } else { 0.0 };
    let rc = if truth > 0 { tp as f64 / truth as f64 } else { 0.0 };
    Totals {
        slips: results.len(),
        truth_rows: truth,
        extracted_rows: ext,
        row_f1: if tp > 0 { r4(2.0 * p * rc / (p + rc)) } else { 0.0 },
        auto_accepted: auto,
        auto_accept_rate: if truth > 0 { r4(auto as f64 / truth as f64) } else { 0.0 },
        false_auto_accepts: results.iter().map(|r| r.false_auto.len()).sum(),
        dropped: results.iter().map(|r| r.dropped.len()).sum(),
        auto_with_page_bbox: results.iter().map(|r| r.auto_with_bbox).sum(),
    }
}

pub fn report(results: &[SlipResult]) -> String {
    let mut s = String::new();
    for r in results {
        s += &format!(
            "{:26} truth {:3} extracted {:3} tp {:3} auto {:3} false_auto {} dropped {} pending {}\n",
            r.slip,
            r.truth,
            r.extracted,
            r.tp,
            r.auto,
            r.false_auto.len(),
            r.dropped.len(),
            r.pending_items
        );
        for x in r
            .false_auto
            .iter()
            .cloned()
            .chain(r.dropped.iter().map(|d| format!("DROPPED {d}")))
        {
            s += &format!("     {x}\n");
        }
    }
    s
}
