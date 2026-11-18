//! Values a person types in (`ingest/manual.py`). Provenance kind `manual`; accepted_by `person`,
//! never the auto-accept rule. A correction is a new version that supersedes the old one; nothing is
//! overwritten.

use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use ownhealth_core::observations::{CommitOptions, Committed, commit_observation, supersede_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::{Error, Result};
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
    /// A coded answer (SNOMED CT) to the `loinc` question, such as a smoking status; `value` then holds
    /// the words the person saw, not a number.
    pub value_code: Option<&'a str>,
    /// The observation this one replaces (with `reason`).
    pub corrects: Option<Uuid>,
    pub reason: Option<&'a str>,
}

/// Why a typed value cannot be stored, or None when it can: the value must read as a number (a
/// decimal comma is fine, as is a leading `<`, `>`, `<=` or `>=` copied from a report), and the date
/// may not be later than tomorrow (a person in a time zone ahead of the server).
pub fn refusal(value: &str, effective_at: DateTime<FixedOffset>, now: DateTime<Utc>) -> Option<&'static str> {
    let v = value.trim();
    let v = ["<=", ">=", "≤", "≥", "<", ">"]
        .iter()
        .find_map(|c| v.strip_prefix(c))
        .unwrap_or(v)
        .trim();
    if !v.replace(',', ".").parse::<f64>().is_ok_and(f64::is_finite) {
        return Some("The value is not a number. Type it as it is printed, for example 5,4.");
    }
    future(effective_at, now)
}

/// The date may not be later than tomorrow.
pub fn future(effective_at: DateTime<FixedOffset>, now: DateTime<Utc>) -> Option<&'static str> {
    (effective_at.with_timezone(&Utc) > now + TimeDelta::days(1))
        .then_some("The date is in the future. Use the day the value was measured.")
}

/// Commit one typed value. `corrects` names the observation this one replaces (with `reason`).
pub async fn add_manual_observation(conn: &mut PgConnection, m: &ManualObservation<'_>) -> Result<Committed> {
    let why = match m.value_code {
        Some(_) => future(m.effective_at, Utc::now()),
        None => refusal(m.value, m.effective_at, Utc::now()),
    };
    if let Some(why) = why {
        return Err(Error::Invalid(why.into()));
    }
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
    if let Some(code) = m.value_code {
        obs.value_code = Some(code.into());
        obs.value_num = None;
        obs.value_cmp = None;
        obs.value_canonical = None;
        obs.unit_canonical = None;
        obs.interpretation = None;
    }
    let src = SrcRef::New(SrcIn {
        profile_id: Some(m.profile_id),
        label: Some("manual".into()), // a code: the UI says "entered by hand" in its language
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn refuses_words_and_future_dates() {
        let now: DateTime<Utc> = "2026-11-10T12:00:00Z".parse().unwrap();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap();
        let today = at("2026-11-10T08:00:00+01:00");
        for ok in ["81,4", "97", "5.4", "-2", "< 0,5", ">=200"] {
            assert_eq!(refusal(ok, today, now), None, "{ok}");
        }
        for bad in ["abc", "", "1,2,3", "NaN", "inf", "12 mg"] {
            assert!(refusal(bad, today, now).is_some(), "{bad}");
        }
        assert_eq!(refusal("97", at("2026-11-11T10:00:00+01:00"), now), None);
        assert!(refusal("97", at("2031-01-01T08:00:00Z"), now).is_some());
    }
}
