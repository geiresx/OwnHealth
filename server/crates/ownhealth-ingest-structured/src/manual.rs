//! Values a person types in (`ingest/manual.py`). Provenance kind `manual`; accepted_by `person`,
//! never the auto-accept rule. A correction is a new version that supersedes the old one; nothing is
//! overwritten.

use chrono::{DateTime, FixedOffset};
use ownhealth_core::Result;
use ownhealth_core::observations::{CommitOptions, Committed, commit_observation, supersede_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_normalize as normalize;
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ManualObservation<'a> {
    pub profile_id: Uuid,
    pub user_id: Uuid,
    pub name: &'a str,
    pub value: &'a str,
    pub unit: Option<&'a str>,
    pub effective_at: DateTime<FixedOffset>,
    pub note: Option<&'a str>,
    pub category: Option<&'a str>,
    pub loinc: Option<&'a str>,
    /// The observation this one replaces (with `reason`).
    pub corrects: Option<Uuid>,
    pub reason: Option<&'a str>,
}

/// Commit one typed value. `corrects` names the observation this one replaces (with `reason`).
pub async fn add_manual_observation(conn: &mut PgConnection, m: &ManualObservation<'_>) -> Result<Committed> {
    let lang: Option<String> = sqlx::query_scalar("select locale from app_user where id = $1")
        .bind(m.user_id)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    let lang = lang.filter(|l| !l.is_empty()).unwrap_or_else(|| "es".into());
    let normalized = normalize::normalize_row(
        &mut *conn,
        &normalize::RowIn {
            profile_id: m.profile_id,
            printed_name: m.name,
            printed_value: m.value,
            printed_unit: m.unit,
            printed_range: None,
            printed_flag: None,
            effective_at: m.effective_at,
            lang: &lang,
            specimen: None,
            provider: None,
            // a typed code drives the alias group and canonical unit, not only the column
            loinc: m.loinc,
            method: None,
            document_id: None,
        },
    )
    .await;
    let mut obs = normalized?.observation;
    obs.source_kind = "self_reported".into();
    obs.document_id = None;
    obs.candidate_row_id = None;
    if let Some(c) = m.category.filter(|c| !c.is_empty()) {
        obs.category = c.into();
    }
    if let Some(l) = m.loinc.filter(|l| !l.is_empty()) {
        obs.loinc = Some(l.into());
    }
    let src = SrcRef::New(SrcIn {
        profile_id: Some(m.profile_id),
        label: Some("Entered by hand".into()),
        ..SrcIn::manual(Some(m.user_id))
    });
    let committed = match m.corrects {
        Some(old) => {
            let reason = m.reason.filter(|r| !r.is_empty()).unwrap_or("corrected");
            supersede_observation(&mut *conn, old, &obs, &src, "person", reason, Some(m.user_id)).await?
        }
        None => {
            commit_observation(
                &mut *conn,
                &obs,
                &src,
                CommitOptions {
                    reviewed_by: Some(m.user_id),
                    ..CommitOptions::new("person")
                },
            )
            .await?
        }
    };
    if let Some(note) = m.note.map(str::trim).filter(|n| !n.is_empty()) {
        let body: String = note.chars().take(4000).collect();
        sqlx::query(
            "insert into note (profile_id, kind, author_kind, author_id, lang, attach_to, body, effective_at, src_id) \
             values ($1, 'journal', 'person', $2, $3, $4, $5, $6, $4)",
        )
        .bind(m.profile_id)
        .bind(m.user_id)
        .bind(&lang)
        .bind(&committed.src_id)
        .bind(body)
        .bind(m.effective_at)
        .execute(conn)
        .await?;
    }
    Ok(committed)
}
