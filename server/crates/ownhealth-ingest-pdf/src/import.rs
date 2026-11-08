//! `import_pdf` and the corrections around it (`ingest/pdf/__init__.py`): pages, guesses, candidate
//! rows, draw grouping, amended reports, pending documents, and moving a document to another profile.

use std::collections::{BTreeSet, HashMap};
use std::sync::RwLock;

use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use futures::future::BoxFuture;
use ownhealth_core::acceptance::{Check, DEFAULT_THRESHOLD, failed_reasons};
use ownhealth_core::blobs::{BlobStore, sha256_hex_to_bytes};
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::jobs::{EnqueueOptions, enqueue};
use ownhealth_core::observations::{ObservationIn, confirm_observation, supersede_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, region_label};
use ownhealth_core::review::{
    CandidateIn, ReviewItem, ReviewItemIn, create_extraction_run, create_item, dismiss, finish_extraction_run,
    submit_candidate,
};
use ownhealth_core::{Error, Result};
use ownhealth_normalize::{Normalized, ResolveQuery, RowIn};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row as _};
use uuid::Uuid;

use crate::classify::{fold, identity_match, name_tokens};
use crate::extract::{Extraction, extract};
use crate::tables::Row;
use crate::textlayer::{ENGINE, ENGINE_VERSION, PageModel, injection_hits};

const GROUND_PAD: f64 = 4.0;

fn fts_config(lang: Option<&str>) -> &'static str {
    match lang {
        Some("es") => "spanish",
        Some("en") => "english",
        Some("de") => "german",
        Some("fr") => "french",
        _ => "simple",
    }
}

// ------------------------------------------------------------- normalizer ----

/// Lane A's `normalize_row`, or a stand-in (the synthetic eval and lane B's tests use the
/// catalog-based one of `fixtures/labslips`, as the Python tests monkeypatched it).
pub type Normalizer = for<'c> fn(&'c mut PgConnection, &'c RowIn<'c>) -> BoxFuture<'c, Result<Normalized>>;

fn lane_a<'c>(conn: &'c mut PgConnection, row: &'c RowIn<'c>) -> BoxFuture<'c, Result<Normalized>> {
    Box::pin(ownhealth_normalize::normalize_row(conn, row))
}

static NORMALIZER: RwLock<Option<Normalizer>> = RwLock::new(None);

/// Replace lane A's normalizer for this process (tests and the eval only).
pub fn use_normalizer(n: Option<Normalizer>) {
    if let Ok(mut g) = NORMALIZER.write() {
        *g = n;
    }
}

fn normalizer() -> Normalizer {
    NORMALIZER.read().ok().and_then(|g| *g).unwrap_or(lane_a)
}

// ---------------------------------------------------------------- retract ----

/// `ingest/retract.py retract_document_facts(conn, document_id, reason)`, lane C's.
pub use ownhealth_ingest_structured::retract::retract_document_facts;

// ------------------------------------------------------------------ import ----

struct Doc {
    id: Uuid,
    household_id: Uuid,
    profile_id: Option<Uuid>,
    status: String,
    mime: String,
    blob_sha256: Vec<u8>,
    doc_type: Option<String>,
    created_at: DateTime<Utc>,
}

async fn lock_document(conn: &mut PgConnection, document_id: Uuid) -> Result<Option<Doc>> {
    let row = sqlx::query(
        "select id, household_id, profile_id, status, mime, blob_sha256, doc_type, created_at from document \
         where id = $1 for update",
    )
    .bind(document_id)
    .fetch_optional(conn)
    .await?;
    row.map(|r| -> Result<Doc> {
        Ok(Doc {
            id: r.try_get("id")?,
            household_id: r.try_get("household_id")?,
            profile_id: r.try_get("profile_id")?,
            status: r.try_get("status")?,
            mime: r.try_get("mime")?,
            blob_sha256: r.try_get("blob_sha256")?,
            doc_type: r.try_get("doc_type")?,
            created_at: r.try_get("created_at")?,
        })
    })
    .transpose()
}

struct Profile {
    id: Uuid,
    household_id: Uuid,
    display_name: String,
    birth_date: Option<chrono::NaiveDate>,
}

/// One stored PDF or image -> pages, guesses, candidate rows (threshold: the default 0.95).
pub async fn import_pdf(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    force: bool,
) -> Result<ImportSummary> {
    import_pdf_with(conn, blobs, document_id, force, DEFAULT_THRESHOLD).await
}

/// `import_pdf` with the auto-accept threshold of the settings. An unknown document is
/// `Error::NotFound` (the job handler makes it a permanent failure).
pub async fn import_pdf_with(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    force: bool,
    threshold: f64,
) -> Result<ImportSummary> {
    let doc = lock_document(&mut *conn, document_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("document {document_id} not found")))?;
    let mut summary = ImportSummary::new(document_id.to_string());
    let Some(profile_id) = doc.profile_id.filter(|_| doc.status == "active") else {
        summary.reasons.insert("not_assigned_or_retracted".into(), 1);
        return Ok(summary);
    };
    if !force {
        let done: Option<i32> = sqlx::query_scalar(
            "select 1 from extraction_run where document_id = $1 and status = 'done' and extractor <> 'agent'",
        )
        .bind(document_id)
        .fetch_optional(&mut *conn)
        .await?;
        if done.is_some() {
            summary.reasons.insert("already_imported".into(), 1);
            return Ok(summary);
        }
    }
    let data = blobs.get_bytes(&hex::encode(&doc.blob_sha256))?;
    if doc.mime != "application/pdf" {
        pending_document(&mut *conn, &doc, &[1], "image").await?;
        summary.queued += 1;
        summary.reasons.insert("pending_document".into(), 1);
        return Ok(summary);
    }

    let ext = tokio_blocking_extract(data).await?;
    let r = sqlx::query("select id, household_id, display_name, birth_date from profile where id = $1")
        .bind(profile_id)
        .fetch_one(&mut *conn)
        .await?;
    let profile = Profile {
        id: r.try_get("id")?,
        household_id: r.try_get("household_id")?,
        display_name: r.try_get("display_name")?,
        birth_date: r.try_get("birth_date")?,
    };
    store_pages(&mut *conn, blobs, document_id, &ext).await?;
    let cls = &ext.cls;
    let identity = identity_match(
        cls.patient_name.as_deref(),
        cls.birth_date,
        &profile.display_name,
        profile.birth_date,
    );
    let guess = if identity != "matched" {
        guess_profile(&mut *conn, profile.household_id, &ext).await?
    } else {
        None
    };
    let sample_hash = cls
        .sample_id
        .as_deref()
        .map(|s| hex::encode(Sha256::digest(fold(s).as_bytes()))[..16].to_string());
    update_document(&mut *conn, &doc, &ext, identity, guess, sample_hash.as_deref()).await?;
    if let Some(g) = guess
        && g != profile.id
    {
        identity_item(&mut *conn, &doc, g).await?;
        summary.queued += 1;
    }

    let lab = cls.doc_type == "lab_report";
    let results = if lab { ext.results() } else { Vec::new() };
    let mut unread = ext.pending_pages.clone();
    if lab {
        unread.extend(&ext.no_table_pages);
    }
    if !unread.is_empty() {
        let why = if ext.pending_pages.is_empty() {
            "no_table"
        } else {
            "no_text_layer"
        };
        pending_document(&mut *conn, &doc, &unread, why).await?;
        summary.queued += 1;
        summary.reasons.insert("pending_document".into(), 1);
    }
    if results.is_empty() && ext.pending().is_empty() {
        return Ok(summary);
    }

    let config_hash = ext.config_hash();
    let run_id = create_extraction_run(
        &mut *conn,
        document_id,
        ext.extractor(),
        ext.extractor_version(),
        &config_hash,
        Some(ENGINE),
        Some(ENGINE_VERSION),
    )
    .await?;
    let (effective_at, date_check) = effective_at(&ext, &profile);
    let report_key = report_key(&ext, sample_hash.as_deref(), document_id);
    let provider = ext.template.as_ref().map(|t| t.provider_name.clone());
    let pages: HashMap<i32, &PageModel> = ext.pdf.pages.iter().map(|p| (p.page_no, p)).collect();
    let normalize = normalizer();

    let mut normalized: Vec<(f64, &Row, ObservationIn, Vec<Check>)> = Vec::new();
    for (spec, row) in &results {
        let row_in = RowIn {
            profile_id: profile.id,
            printed_name: row.text("name"),
            printed_value: row.text("value"),
            printed_unit: Some(row.text("unit")).filter(|s| !s.is_empty()),
            printed_range: Some(row.text("ref_range")).filter(|s| !s.is_empty()),
            printed_flag: Some(row.text("flag")).filter(|s| !s.is_empty()),
            effective_at,
            lang: ext.locale(),
            specimen: row.specimen.as_deref(),
            provider: provider.as_deref(),
            loinc: None,
            method: None,
            document_id: Some(document_id),
        };
        let n = normalize(&mut *conn, &row_in).await?;
        let obs = ObservationIn {
            report_key: Some(report_key.clone()),
            document_id: Some(document_id),
            ..n.observation
        };
        let mut checks = n.checks;
        if let Some(page) = pages.get(&row.page_no) {
            checks.extend(row_checks(row, page));
        }
        checks.push(date_check.clone());
        normalized.push((spec.prior, *row, obs, checks));
    }

    let groups: BTreeSet<String> = normalized.iter().map(|(_, _, o, _)| o.alias_group.clone()).collect();
    let earlier = earlier_edition(
        &mut *conn,
        &doc,
        profile_id,
        &ext,
        sample_hash.as_deref(),
        &effective_at,
        &groups,
    )
    .await?;
    if let Some(e) = earlier {
        sqlx::query("update document set supersedes_document = $1 where id = $2")
            .bind(e)
            .bind(document_id)
            .execute(&mut *conn)
            .await?;
    }
    let old_rows = match earlier {
        Some(e) => observations_of(&mut *conn, e).await?,
        None => HashMap::new(),
    };
    let provisional = cls.report_status == "preliminary";

    for (prior, row, obs, checks) in normalized {
        let mut cand = CandidateIn {
            page_no: row.page_no,
            bbox: row.bbox(),
            cells: row.cells.as_dict(),
            row_conf: row.conf(prior),
            observation: obs,
            checks,
        };
        if let Some(old) = old_rows.get(&cand.observation.alias_group) {
            if same_value(old, &cand.observation) {
                insert_candidate(&mut *conn, run_id, &cand, "confirmed", Some(old.id)).await?;
                if old.status == "provisional" && !provisional {
                    confirm_observation(&mut *conn, old.id).await?;
                }
                summary.skipped += 1;
                bump(&mut summary, "identical_in_earlier_edition");
            } else {
                amended_item(&mut *conn, run_id, &doc, &cand, old, provisional).await?;
                summary.queued += 1;
                bump(&mut summary, "amended_value");
            }
            continue;
        }
        let dup = duplicate(&mut *conn, &cand.observation, document_id).await?;
        if let Some(d) = &dup {
            if same_value(d, &cand.observation) {
                insert_candidate(&mut *conn, run_id, &cand, "confirmed", Some(d.id)).await?;
                summary.skipped += 1;
                bump(&mut summary, "duplicate");
                continue;
            }
            cand.checks.push(Check {
                name: "conflict".into(),
                ok: false,
                reason: Some("conflict".into()),
                detail: Some(format!(
                    "another document holds {} for this date",
                    d.printed_value.as_deref().unwrap_or("None")
                )),
            });
        }
        let allowance = if prior >= 1.0 { "template_text" } else { "rules_text" };
        let result = submit_candidate(&mut *conn, run_id, &cand, allowance, identity, threshold, provisional).await?;
        if result.state == "auto_accepted" {
            summary.accepted += 1;
        } else {
            summary.queued += 1;
            for r in &result.reasons {
                bump(&mut summary, r);
            }
        }
    }

    sqlx::query("delete from expected_test where report_document_id = $1")
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    for row in ext.pending() {
        let group = alias_group(&mut *conn, row, ext.locale()).await?;
        sqlx::query(
            "insert into expected_test (report_document_id, printed_name, alias_group, reason) values ($1, $2, $3, $4)",
        )
        .bind(document_id)
        .bind(row.text("name"))
        .bind(group)
        .bind(row.text("value"))
        .execute(&mut *conn)
        .await?;
    }
    finish_extraction_run(&mut *conn, run_id, "done", None).await?;
    Ok(summary)
}

fn bump(s: &mut ImportSummary, reason: &str) {
    *s.reasons.entry(reason.to_string()).or_default() += 1;
}

/// PDF parsing is CPU work: off the async worker threads.
async fn tokio_blocking_extract(data: Vec<u8>) -> Result<Extraction> {
    match tokio::task::spawn_blocking(move || extract(&data)).await {
        Ok(r) => r,
        Err(e) => Err(Error::Other(anyhow::anyhow!("PDF extraction panicked: {e}"))),
    }
}

// ------------------------------------------------------------------- pages ----

async fn store_pages(conn: &mut PgConnection, blobs: &BlobStore, document_id: Uuid, ext: &Extraction) -> Result<()> {
    let cfg = fts_config(ext.cls.language.as_deref());
    for page in &ext.pdf.pages {
        let layer = json!({
            "engine": format!("{ENGINE} {ENGINE_VERSION}"),
            "width": page.width,
            "height": page.height,
            "words": page.words.iter().map(|w| w.as_dict()).collect::<Vec<_>>(),
        });
        let words_sha = blobs.put_bytes(&serde_json::to_vec(&layer)?)?;
        sqlx::query(
            "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, words_blob, \
             flags, text_content, fts) values ($1, $2, $3, $4, $5, $6, $7, $8, to_tsvector(cast($9 as regconfig), $10)) \
             on conflict (document_id, page_no) do update set width_pt = excluded.width_pt, \
             height_pt = excluded.height_pt, text_layer_ok = excluded.text_layer_ok, \
             words_blob = excluded.words_blob, flags = excluded.flags, text_content = excluded.text_content, \
             fts = excluded.fts",
        )
        .bind(document_id)
        .bind(page.page_no)
        .bind(page.width as f32)
        .bind(page.height as f32)
        .bind(page.text_layer_ok)
        .bind(sha256_hex_to_bytes(&words_sha)?)
        .bind(Value::Object(page.flags.clone()))
        .bind(page.text(false))
        .bind(cfg)
        .bind(page.text(true))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Python `datetime.isoformat()` of a naive datetime.
fn naive_iso(d: &NaiveDateTime) -> String {
    let s = d.format("%Y-%m-%dT%H:%M:%S").to_string();
    match d.and_utc().timestamp_subsec_micros() {
        0 => s,
        us => format!("{s}.{us:06}"),
    }
}

async fn update_document(
    conn: &mut PgConnection,
    doc: &Doc,
    ext: &Extraction,
    identity: &str,
    guess: Option<Uuid>,
    sample_hash: Option<&str>,
) -> Result<()> {
    let c = &ext.cls;
    let mut flags: Map<String, Value> = ext.pdf.flags.clone();
    if !ext.pdf.metadata.is_empty() || !ext.pdf.annotations.is_empty() {
        let mut m: Map<String, Value> = ext.pdf.metadata.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        if !ext.pdf.annotations.is_empty() {
            m.insert("annotations".into(), json!(ext.pdf.annotations));
        }
        flags.insert("metadata_text".into(), Value::Object(m));
    }
    let profile_guess = match guess {
        Some(g) => Some(g.to_string()),
        None if identity == "matched" => doc.profile_id.map(|p| p.to_string()),
        None => None,
    };
    flags.insert(
        "guess".into(),
        json!({
            "doc_type": c.doc_type,
            "profile_id": profile_guess,
            "collected_at": c.collected_at.as_ref().map(naive_iso),
            "provider": ext.template.as_ref().map(|t| t.provider_name.clone()),
            "template_score": ext.template_score,
            "confirmed": false,
        }),
    );
    if let Some(h) = sample_hash {
        flags.insert("sample_hash".into(), json!(h));
    }
    let name_hash = c.patient_name.as_deref().map(|n| {
        let toks: Vec<String> = name_tokens(n).into_iter().collect();
        Sha256::digest(toks.join(" ").as_bytes()).to_vec()
    });
    sqlx::query(
        "update document set pages = $1, doc_type = coalesce(doc_type, $2), language = $3, \
         provider_name = coalesce(provider_name, $4), provider_fingerprint = $5, report_status = $6, \
         collected_at = $7, reported_at = $8, identity_name_hash = $9, identity_birth_date = $10, \
         identity_match = $11, flags = flags || $12 where id = $13",
    )
    .bind(ext.pdf.pages.len() as i32)
    .bind(&c.doc_type)
    .bind(&c.language)
    .bind(ext.template.as_ref().map(|t| t.provider_name.clone()))
    .bind(ext.template.as_ref().map(|t| t.id.clone()))
    .bind((c.doc_type == "lab_report").then(|| c.report_status.clone()))
    .bind(c.collected_at.map(|d| d.and_utc()))
    .bind(c.reported_at.map(|d| d.and_utc()))
    .bind(name_hash)
    .bind(c.birth_date)
    .bind(identity)
    .bind(Value::Object(flags))
    .bind(doc.id)
    .execute(conn)
    .await?;
    Ok(())
}

async fn guess_profile(conn: &mut PgConnection, household_id: Uuid, ext: &Extraction) -> Result<Option<Uuid>> {
    let rows = sqlx::query("select id, display_name, birth_date from profile where household_id = $1")
        .bind(household_id)
        .fetch_all(conn)
        .await?;
    let mut matched = Vec::new();
    for r in rows {
        let name: String = r.try_get("display_name")?;
        let birth: Option<chrono::NaiveDate> = r.try_get("birth_date")?;
        if identity_match(ext.cls.patient_name.as_deref(), ext.cls.birth_date, &name, birth) == "matched" {
            matched.push(r.try_get::<Uuid, _>("id")?);
        }
    }
    Ok(if matched.len() == 1 {
        matched.first().copied()
    } else {
        None
    })
}

// ------------------------------------------------------------- review items ----

/// One review item: this document waits for an outside agent (MCP propose) or manual entry.
async fn pending_document(conn: &mut PgConnection, doc: &Doc, pages: &[i32], why: &str) -> Result<Uuid> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "select id from review_item where document_id = $1 and kind = 'pending_document' and status = 'open'",
    )
    .bind(doc.id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = existing {
        return Ok(id);
    }
    create_item(
        conn,
        &ReviewItemIn {
            kind: "pending_document".into(),
            household_id: Some(doc.household_id),
            profile_id: doc.profile_id,
            payload: json!({"document_id": doc.id.to_string(), "pages": pages, "why": why}),
            document_id: Some(doc.id),
            ref_table: Some("document".into()),
            ref_id: Some(doc.id),
            ..ReviewItemIn::default()
        },
    )
    .await
}

async fn identity_item(conn: &mut PgConnection, doc: &Doc, guess: Uuid) -> Result<Uuid> {
    create_item(
        conn,
        &ReviewItemIn {
            kind: "document_identity".into(),
            household_id: Some(doc.household_id),
            profile_id: doc.profile_id,
            payload: json!({
                "document_id": doc.id.to_string(),
                "uploaded_to": doc.profile_id.map(|p| p.to_string()),
                "suggested_profile_id": guess.to_string(),
            }),
            reasons: vec!["identity_mismatch".into()],
            document_id: Some(doc.id),
            ref_table: Some("profile".into()),
            ref_id: Some(guess),
            ..ReviewItemIn::default()
        },
    )
    .await
}

/// A candidate row stored as it is, linked (`confirmed`) or waiting (`in_review`).
async fn insert_candidate(
    conn: &mut PgConnection,
    run_id: Uuid,
    cand: &CandidateIn,
    state: &str,
    obs_id: Option<Uuid>,
) -> Result<Uuid> {
    let bbox: Vec<f32> = cand.bbox.iter().map(|v| *v as f32).collect();
    Ok(sqlx::query_scalar(
        "insert into candidate_row (extraction_run_id, page_no, bbox, cells, row_conf, reasons, state, \
         observation_id) values ($1, $2, $3, $4, $5, $6, $7, $8) returning id",
    )
    .bind(run_id)
    .bind(cand.page_no)
    .bind(&bbox)
    .bind(&cand.cells)
    .bind(cand.row_conf as f32)
    .bind(failed_reasons(&cand.checks))
    .bind(state)
    .bind(obs_id)
    .fetch_one(conn)
    .await?)
}

async fn amended_item(
    conn: &mut PgConnection,
    run_id: Uuid,
    doc: &Doc,
    cand: &CandidateIn,
    old: &OldObs,
    provisional: bool,
) -> Result<Uuid> {
    let row_id = insert_candidate(&mut *conn, run_id, cand, "in_review", None).await?;
    let obs = ObservationIn {
        candidate_row_id: Some(row_id),
        ..cand.observation.clone()
    };
    let src = SrcIn {
        profile_id: Some(obs.profile_id),
        document_id: Some(doc.id),
        page_no: Some(cand.page_no),
        bbox: Some(cand.bbox),
        label: Some(region_label(&mut *conn, doc.id, cand.page_no, Some(&obs.effective_at)).await?),
        extraction_run_id: Some(run_id),
        candidate_row_id: Some(row_id),
        ..SrcIn::new("document_region")
    }
    .validate()?;
    let mut reasons: BTreeSet<String> = failed_reasons(&cand.checks).into_iter().collect();
    reasons.insert("amended_value".into());
    create_item(
        conn,
        &ReviewItemIn {
            kind: "amended_report".into(),
            household_id: Some(doc.household_id),
            profile_id: doc.profile_id,
            payload: json!({
                "observation": serde_json::to_value(&obs)?,
                "src": serde_json::to_value(&src)?,
                "checks": serde_json::to_value(&cand.checks)?,
                "cells": cand.cells,
                "page_no": cand.page_no,
                "bbox": cand.bbox,
                "old_observation_id": old.id.to_string(),
                "old_printed_value": old.printed_value,
                "old_document_id": old.document_id.map(|d| d.to_string()),
                "provisional": provisional,
            }),
            reasons: reasons.into_iter().collect(),
            document_id: Some(doc.id),
            candidate_row_id: Some(row_id),
            observation_id: Some(old.id),
            row_conf: Some(cand.row_conf),
            ..ReviewItemIn::default()
        },
    )
    .await
}

fn merge(base: &Value, over: Option<&Value>) -> Value {
    let mut m = base.as_object().cloned().unwrap_or_default();
    if let Some(Value::Object(o)) = over {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

/// Accepting an amended value supersedes the earlier edition's observation.
pub(crate) async fn accept_amended(
    conn: &mut PgConnection,
    item: &ReviewItem,
    user_id: Uuid,
    edits: Option<&Value>,
) -> Result<Vec<Uuid>> {
    let p = &item.payload;
    let obs = ObservationIn::from_json(merge(&p["observation"], edits))?;
    let src_v = merge(
        &p["src"],
        Some(&json!({"actor_kind": "user", "actor_id": user_id.to_string()})),
    );
    let src: SrcIn = serde_json::from_value::<SrcIn>(src_v)
        .map_err(|e| Error::Invalid(e.to_string()))?
        .validate()?;
    let old_id = p["old_observation_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Review("amended_report item without old_observation_id".into()))?;
    let committed = supersede_observation(
        &mut *conn,
        old_id,
        &obs,
        &SrcRef::New(src),
        "person",
        "amended report",
        Some(user_id),
    )
    .await?;
    if let Some(row) = item.candidate_row_id {
        sqlx::query("update candidate_row set state = 'confirmed', observation_id = $1 where id = $2")
            .bind(committed.id)
            .bind(row)
            .execute(conn)
            .await?;
    }
    Ok(vec![committed.id])
}

/// Accepting the suggestion moves the document to the profile named on its pages and re-imports it.
pub(crate) async fn accept_identity(
    conn: &mut PgConnection,
    item: &ReviewItem,
    user_id: Uuid,
    edits: Option<&Value>,
) -> Result<Vec<Uuid>> {
    let edited = edits
        .and_then(|e| e.get("profile_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let target = edited
        .or_else(|| item.payload["suggested_profile_id"].as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid("document_identity item without a profile".into()))?;
    let document_id = item
        .document_id
        .ok_or_else(|| Error::Review("document_identity item without a document".into()))?;
    assign_document(conn, document_id, target, user_id, Some(item.id)).await?;
    Ok(Vec::new())
}

/// Structured files run their own import again; everything else (PDF, image) the PDF one.
fn reimport_kind(mime: &str) -> &'static str {
    match mime {
        "application/fhir+json" | "application/fhir+ndjson" => "import.fhir_bundle",
        "application/zip" => "import.apple_health",
        _ => "import.pdf",
    }
}

/// Confirm or change a document's profile. Open items of the old reading are dismissed, values
/// already committed from it are retracted, and the import runs again for the new profile.
/// Everything the old reading brought in (observations, records, wearable samples) is withdrawn.
/// Returns the new import job id (None when the profile did not change). A refusal (another
/// household, the file already on that profile, a genome-facts file) is `Error::Review` (409).
pub async fn assign_document(
    conn: &mut PgConnection,
    document_id: Uuid,
    profile_id: Uuid,
    user_id: Uuid,
    skip_item: Option<Uuid>,
) -> Result<Option<i64>> {
    let doc = lock_document(&mut *conn, document_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("document {document_id} not found")))?;
    let target: Option<Uuid> = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(profile_id)
        .fetch_optional(&mut *conn)
        .await?;
    if target != Some(doc.household_id) {
        return Err(Error::Review("the profile is not in this document's household".into()));
    }
    sqlx::query(
        "update document set flags = jsonb_set(flags, '{guess,confirmed}', 'true') where id = $1 and flags ? 'guess'",
    )
    .bind(document_id)
    .execute(&mut *conn)
    .await?;
    if doc.profile_id == Some(profile_id) {
        return Ok(None);
    }
    let clash: Option<i32> =
        sqlx::query_scalar("select 1 from document where profile_id = $1 and blob_sha256 = $2 and id <> $3")
            .bind(profile_id)
            .bind(&doc.blob_sha256)
            .bind(document_id)
            .fetch_optional(&mut *conn)
            .await?;
    if clash.is_some() {
        return Err(Error::Review("that profile already holds this file".into()));
    }
    let open: Vec<Uuid> = sqlx::query_scalar(
        "select id from review_item where document_id = $1 and status = 'open' and id is distinct from $2",
    )
    .bind(document_id)
    .bind(skip_item)
    .fetch_all(&mut *conn)
    .await?;
    for item_id in open {
        dismiss(&mut *conn, item_id, user_id, Some("document moved to another profile")).await?;
    }
    let kind = reimport_kind(&doc.mime);
    if doc.doc_type.as_deref() == Some("genomics") {
        return Err(Error::Review(
            "a genome-facts file cannot move; retract it and upload it to the right profile".into(),
        ));
    }
    retract_document_facts(&mut *conn, document_id, "document moved to another profile").await?;
    sqlx::query("update document set profile_id = $1, identity_match = null where id = $2")
        .bind(profile_id)
        .bind(document_id)
        .execute(&mut *conn)
        .await?;
    let key = format!("{kind}:{document_id}:{profile_id}");
    Ok(enqueue(
        conn,
        kind,
        &json!({
            "document_id": document_id.to_string(),
            "profile_id": profile_id.to_string(),
            "household_id": doc.household_id.to_string(),
            "force": true,
        }),
        EnqueueOptions::dedupe(&key),
    )
    .await?)
}

// ------------------------------------------------------------------ checks ----

fn inside(inner: &[f64; 4], outer: &[f64; 4], pad: f64) -> bool {
    inner[0] >= outer[0] - pad && inner[1] >= outer[1] - pad && inner[2] <= outer[2] + pad && inner[3] <= outer[3] + pad
}

fn row_checks(row: &Row, page: &PageModel) -> Vec<Check> {
    let mut checks = Vec::new();
    let value = row.cells.get("value");
    let rb = row.bbox();
    let grounded = value
        .and_then(|v| v.bbox.filter(|_| !v.text.is_empty()))
        .is_some_and(|b| inside(&b, &rb, GROUND_PAD));
    checks.push(Check {
        name: "grounding".into(),
        ok: grounded,
        reason: (!grounded).then(|| "not_grounded".into()),
        detail: (!grounded).then(|| "no value token inside the row".into()),
    });
    let name = row.cells.get("name");
    let on_line = match (
        name.filter(|n| !n.text.is_empty()).and_then(|n| n.bbox),
        value.and_then(|v| v.bbox),
    ) {
        (Some(n), Some(v)) => n[3].min(v[3]) - n[1].max(v[1]) > 0.0,
        _ => false,
    };
    checks.push(Check {
        name: "name_on_line".into(),
        ok: on_line,
        reason: (!on_line).then(|| "name_not_on_line".into()),
        detail: None,
    });
    let row_text = row
        .cells
        .0
        .iter()
        .map(|(_, c)| c.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let hidden_inside = page
        .words
        .iter()
        .any(|w| w.hidden.is_some() && inside(&w.bbox(), &rb, 0.0));
    let hits = injection_hits(&row_text);
    if !hits.is_empty() || hidden_inside {
        checks.push(Check {
            name: "injection".into(),
            ok: false,
            reason: Some("possible_injection".into()),
            detail: Some(if hidden_inside {
                "hidden text inside the row".into()
            } else {
                hits.join(", ")
            }),
        });
    }
    checks
}

fn effective_at(ext: &Extraction, profile: &Profile) -> (DateTime<FixedOffset>, Check) {
    let c = &ext.cls;
    let Some(when) = c.collected_at.or(c.reported_at) else {
        return (
            Utc::now().fixed_offset(),
            Check {
                name: "dates".into(),
                ok: false,
                reason: Some("date_invalid".into()),
                detail: Some("no collection date found".into()),
            },
        );
    };
    let eff = when.and_utc().fixed_offset();
    let mut problems = Vec::new();
    if eff > Utc::now().fixed_offset() {
        problems.push("collection date in the future");
    }
    if profile.birth_date.is_some_and(|b| eff.date_naive() < b) {
        problems.push("collection date before birth");
    }
    if let (Some(a), Some(b)) = (c.collected_at, c.reported_at)
        && a > b
    {
        problems.push("collection date after report date");
    }
    if c.collected_at.is_none() {
        problems.push("only a report date was found");
    }
    if problems.is_empty() {
        return (eff, Check::pass("dates"));
    }
    (
        eff,
        Check {
            name: "dates".into(),
            ok: false,
            reason: Some("date_invalid".into()),
            detail: Some(problems.join("; ")),
        },
    )
}

fn report_key(ext: &Extraction, sample_hash: Option<&str>, document_id: Uuid) -> String {
    let when = ext.cls.collected_at.or(ext.cls.reported_at);
    let provider = ext.template.as_ref().map_or("generic", |t| t.id.as_str());
    let day = when.map_or("nodate".to_string(), |w| w.date().format("%Y-%m-%d").to_string());
    let tail = sample_hash.map_or_else(|| document_id.to_string()[..8].to_string(), String::from);
    format!("{provider}:{day}:{tail}")
}

/// The columns of an existing observation the edition and duplicate checks compare.
struct OldObs {
    id: Uuid,
    status: String,
    value_num: Option<Decimal>,
    unit_ucum: Option<String>,
    printed_value: Option<String>,
    document_id: Option<Uuid>,
}

fn old_obs(r: &sqlx::postgres::PgRow) -> Result<OldObs> {
    Ok(OldObs {
        id: r.try_get("id")?,
        status: r.try_get("status")?,
        value_num: r.try_get("value_num")?,
        unit_ucum: r.try_get("unit_ucum")?,
        printed_value: r.try_get("printed_value")?,
        document_id: r.try_get("document_id")?,
    })
}

async fn observations_of(conn: &mut PgConnection, document_id: Uuid) -> Result<HashMap<String, OldObs>> {
    let rows = sqlx::query(
        "select id, alias_group, status, value_num, unit_ucum, printed_value, document_id from observation \
         where document_id = $1 and status in ('current', 'provisional')",
    )
    .bind(document_id)
    .fetch_all(conn)
    .await?;
    let mut out = HashMap::new();
    for r in &rows {
        out.insert(r.try_get::<String, _>("alias_group")?, old_obs(r)?);
    }
    Ok(out)
}

/// The report this one amends or finalises: same profile and collection day, and the same sample
/// id, or >= 50% of its analyte groups on that day (ingestion.md 11).
async fn earlier_edition(
    conn: &mut PgConnection,
    doc: &Doc,
    profile_id: Uuid,
    ext: &Extraction,
    sample_hash: Option<&str>,
    effective_at: &DateTime<FixedOffset>,
    groups: &BTreeSet<String>,
) -> Result<Option<Uuid>> {
    if ext.cls.collected_at.is_none() {
        return Ok(None);
    }
    let cands = sqlx::query(
        "select d.id, d.flags->>'sample_hash' as sh from document d where d.profile_id = $1 and d.id <> $2 \
         and d.status = 'active' and d.collected_at::date = $3 and d.created_at <= $4 order by d.created_at desc, d.id",
    )
    .bind(profile_id)
    .bind(doc.id)
    .bind(effective_at.date_naive())
    .bind(doc.created_at)
    .fetch_all(&mut *conn)
    .await?;
    for c in cands {
        let id: Uuid = c.try_get("id")?;
        let sh: Option<String> = c.try_get("sh")?;
        if sample_hash.is_some() && sh.as_deref() == sample_hash {
            return Ok(Some(id));
        }
        let theirs: BTreeSet<String> = observations_of(&mut *conn, id).await?.into_keys().collect();
        if !theirs.is_empty() && !groups.is_empty() {
            let inter = theirs.intersection(groups).count() as f64;
            let uni = theirs.union(groups).count() as f64;
            if inter / uni >= 0.5 {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}

async fn duplicate(conn: &mut PgConnection, obs: &ObservationIn, document_id: Uuid) -> Result<Option<OldObs>> {
    let row = sqlx::query(
        "select id, alias_group, status, value_num, unit_ucum, printed_value, document_id from observation \
         where profile_id = $1 and alias_group = $2 and status in ('current', 'provisional') \
         and effective_at::date = $3 and document_id is distinct from $4 order by recorded_at desc limit 1",
    )
    .bind(obs.profile_id)
    .bind(&obs.alias_group)
    .bind(obs.effective_at.date_naive())
    .bind(document_id)
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(old_obs).transpose()
}

fn same_value(old: &OldObs, new: &ObservationIn) -> bool {
    if let (Some(a), Some(b)) = (old.value_num, new.value_num) {
        return a == b && old.unit_ucum.as_deref().unwrap_or("") == new.unit_ucum.as_deref().unwrap_or("");
    }
    let a = fold(old.printed_value.as_deref().unwrap_or(""));
    let b = fold(&new.printed_value);
    a.trim() == b.trim()
}

/// The analyte group of a pending line, while lane A can resolve one.
async fn alias_group(conn: &mut PgConnection, row: &Row, lang: &str) -> Result<Option<String>> {
    let q = ResolveQuery {
        lang,
        specimen: row.specimen.as_deref(),
        ..ResolveQuery::default()
    };
    Ok(Some(
        ownhealth_normalize::resolve_analyte(Some(conn), row.text("name"), &q)
            .await?
            .alias_group,
    ))
}
