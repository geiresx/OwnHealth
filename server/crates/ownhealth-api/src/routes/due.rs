//! The Due screen: the ownhealth.prevention/v1 document of one profile in one language, and what a
//! person decides and answers about it (migration 1130).
//!
//! ```text
//! GET    /api/profiles/{p}/due?as_of=&lang=              read:summary
//! POST   /api/profiles/{p}/due/{item_key}/decision      read:records + write   {kind, until?, reason?, evidence?}
//! DELETE /api/profiles/{p}/due/{item_key}/decision      read:records + write   (undo: the item is as before)
//! GET    /api/profiles/{p}/prevention-answers?lang=     read:records
//! PUT    /api/profiles/{p}/prevention-answers?lang=     read:records + write   {answer_key: "yes" | "no" | null}
//! ```
//!
//! Every item is a record, so a caller without read:records gets an empty list, and an item a lab
//! value shows also needs read:labs. What the scopes leave out is counted in `withheld`. The
//! `prevention_hash` covers the whole document and is the same for every caller and language; a
//! caller without read:records gets none (the audit row keeps it).
//!
//! A decision is `snooze` (until a date, shown again the day after), `dismiss` (a reason is required)
//! or `done` (evidence: a current record or an active document of the same profile, dated today or
//! earlier). A lab gap needs read:labs to act on, as to see it. A new decision
//! supersedes the current one; undo retracts it. Answers are "yes" or "no" to the closed list of
//! `data/prevention/pack.yaml`; null retracts one. The audit rows carry ids and codes only: never a
//! reason or an answer value.

use std::collections::{BTreeMap, HashMap};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::NaiveDate;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, PrincipalKind, today_utc};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::provenance::{SrcIn, create_src};
use ownhealth_rules::prevention::{self, EVIDENCE_TABLES};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Check, R, ok};
use super::status::{event, parse_date};
use crate::error::ApiError;
use crate::extract::Caller;

/// The longest reason a person may give, in characters (the column allows 500).
const REASON_MAX: usize = 500;
/// The furthest a snooze may reach, in days.
const SNOOZE_MAX_DAYS: i64 = 3660;

pub async fn due(
    rt: &Runtime,
    principal: &Principal,
    p: &str,
    as_of: Option<chrono::NaiveDate>,
    lang: Option<&str>,
) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:summary"), None)
        .await?;
    let doc =
        ownhealth_rules::prevention::build_document(&mut tx, perm.profile.id, as_of.unwrap_or_else(today_utc)).await?;
    let mut e = event(principal, perm.profile.id, "GET /api/profiles/{p}/due");
    e.resource_type = Some("prevention".into());
    e.resource_ids = Some(vec![doc["prevention_hash"].as_str().unwrap_or_default().to_string()]);
    write_audit(&mut tx, &e).await?;
    // the person's own words stay out of the hashed document; the screen shows them beside the item
    let reasons: Vec<(String, String)> = sqlx::query_as(
        "select item_key, reason from prevention_decision \
         where profile_id = $1 and status = 'current' and reason is not null",
    )
    .bind(perm.profile.id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let pack = ownhealth_rules::load_pack().map_err(ownhealth_core::Error::from)?;
    let mut out = ownhealth_rules::prevention::render(&doc, &pack, lang.unwrap_or(&principal.lang), &perm.scopes)?;
    let reasons: HashMap<String, String> = reasons.into_iter().collect();
    for list in ["items", "hidden"] {
        for it in out.get_mut(list).and_then(Value::as_array_mut).into_iter().flatten() {
            let key = it["key"].as_str().unwrap_or_default().to_string();
            if let (Some(r), Some(d)) = (reasons.get(&key), it.get_mut("decision").and_then(Value::as_object_mut)) {
                d.insert("reason".into(), json!(r));
            }
        }
    }
    Ok(out)
}

async fn get_due(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let mut check = Check::query();
    let as_of = parse_date(&q, &mut check);
    check.done()?;
    let lang = q.get("lang").filter(|v| !v.is_empty()).map(String::as_str);
    ok(due(&rt, &c, &p, as_of, lang).await?)
}

// ------------------------------------------------------------ person actions ----

type Answer = Result<(StatusCode, Json<Value>), ApiError>;

fn bad_input(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

fn body_object(body: Result<Json<Value>, JsonRejection>) -> Result<Map<String, Value>, ApiError> {
    match body {
        Ok(Json(Value::Object(m))) => Ok(m),
        Ok(_) => Err(bad_input("bad_input", "The body must be a JSON object.")),
        Err(e) => Err(bad_input("bad_input", format!("JSON decode error: {}", e.body_text()))),
    }
}

/// One audit row: ids and codes only (`args` is a kind and an item key, or answer keys).
async fn audit(
    conn: &mut PgConnection,
    principal: &Principal,
    perm: &Permission,
    resource_type: &str,
    ids: &[Uuid],
    args: String,
) -> Result<(), ApiError> {
    let via = if principal.kind == PrincipalKind::Token {
        "api"
    } else {
        "ui"
    };
    let mut ev = AuditEvent::new(principal.actor_kind(), via, "write");
    ev.actor_id = principal.user_id;
    ev.token_id = principal.token_id;
    ev.tool = Some("due".into());
    ev.profile_id = Some(perm.profile.id);
    ev.resource_type = Some(resource_type.into());
    ev.resource_ids = Some(ids.iter().map(Uuid::to_string).collect());
    ev.args_summary = Some(args);
    ev.rows_out = Some(i32::try_from(ids.len()).unwrap_or(i32::MAX));
    write_audit(conn, &ev).await?;
    Ok(())
}

/// The reason, trimmed; None when absent or blank.
fn reason_of(body: &Map<String, Value>) -> Result<Option<String>, ApiError> {
    match body.get("reason") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.chars().count() > REASON_MAX {
                return Err(bad_input(
                    "bad_input",
                    format!("reason: at most {REASON_MAX} characters."),
                ));
            }
            Ok((!s.is_empty()).then(|| s.to_string()))
        }
        Some(_) => Err(bad_input("bad_input", "reason: a string.")),
    }
}

fn until_of(body: &Map<String, Value>, today: NaiveDate) -> Result<NaiveDate, ApiError> {
    let d = body
        .get("until")
        .and_then(Value::as_str)
        .and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok())
        .ok_or_else(|| bad_input("until_required", "A snooze needs `until`, a date (YYYY-MM-DD)."))?;
    // the screen offers the person's local today, which west of UTC is still yesterday here
    if d < today - chrono::Duration::days(1) || (d - today).num_days() > SNOOZE_MAX_DAYS {
        return Err(bad_input("bad_until", "until: a date from today on, within ten years."));
    }
    Ok(d)
}

/// `evidence`: {table: immunization | procedure | encounter | document, id}.
fn evidence_of(body: &Map<String, Value>) -> Result<(String, Uuid), ApiError> {
    let refused = || {
        bad_input(
            "evidence_required",
            "Done needs `evidence`: {table: immunization | procedure | encounter | document, id}.",
        )
    };
    let e = body.get("evidence").and_then(Value::as_object).ok_or_else(refused)?;
    let table = e.get("table").and_then(Value::as_str).unwrap_or_default();
    if table != "document" && !EVIDENCE_TABLES.contains(&table) {
        return Err(refused());
    }
    let id = e
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(refused)?;
    Ok((table.to_string(), id))
}

/// Whether the profile's document today lists the item (shown or hidden) and the caller may see it:
/// a lab gap the caller's scopes withhold is unknown to them, so they can neither learn of it nor
/// hide it.
async fn known_item(conn: &mut PgConnection, perm: &Permission, key: &str) -> Result<bool, ApiError> {
    let doc = prevention::build_document(conn, perm.profile.id, today_utc()).await?;
    Ok(["items", "hidden"].iter().any(|l| {
        doc[*l].as_array().is_some_and(|a| {
            a.iter()
                .any(|i| i["key"].as_str() == Some(key) && prevention::visible(i, &perm.scopes))
        })
    }))
}

/// Whether the key names a lab gap (a pack rule never measured, or `stale:<rule>:<group>`), which a
/// caller without read:labs may not see.
fn lab_key(key: &str) -> Result<bool, ApiError> {
    let pack = ownhealth_rules::load_pack().map_err(ownhealth_core::Error::from)?;
    Ok(key.starts_with("stale:") || pack.rule(key).is_some())
}

fn unknown_item() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown", "No such due item for this profile.")
}

/// The current decision about one item, locked.
async fn current_decision(conn: &mut PgConnection, profile_id: Uuid, key: &str) -> Result<Option<Uuid>, ApiError> {
    Ok(sqlx::query_scalar(
        "select id from prevention_decision where profile_id = $1 and item_key = $2 and status = 'current' \
         for update",
    )
    .bind(profile_id)
    .bind(key)
    .fetch_optional(conn)
    .await?)
}

async fn decide(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, key)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let body = body_object(body)?;
    let kind = body.get("kind").and_then(Value::as_str).unwrap_or_default();
    if !["snooze", "dismiss", "done"].contains(&kind) {
        return Err(bad_input("bad_input", "kind: snooze, dismiss or done."));
    }
    let reason = reason_of(&body)?;
    if kind == "dismiss" && reason.is_none() {
        return Err(bad_input("reason_required", "A dismissal needs a reason."));
    }
    let today = today_utc();
    let until = if kind == "snooze" {
        Some(until_of(&body, today)?)
    } else {
        None
    };
    let evidence = if kind == "done" {
        Some(evidence_of(&body)?)
    } else {
        None
    };
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), Some("write")).await?;
    let pid = perm.profile.id;
    if !known_item(&mut tx, &perm, &key).await? {
        return Err(unknown_item());
    }
    if let Some((table, id)) = &evidence {
        let Some(found) = prevention::evidence(&mut tx, pid, table, *id).await? else {
            return Err(bad_input(
                "evidence_refused",
                "The evidence is not a current, non-sensitive record or an active document of this profile.",
            ));
        };
        // an undated record proves nothing on any day, and a later one not yet
        if found.day.is_none_or(|d| d > today) {
            return Err(bad_input(
                "evidence_undated",
                "The evidence needs the day it was done, today or earlier.",
            ));
        }
    }
    let old = current_decision(&mut tx, pid, &key).await?;
    if let Some(old) = old {
        sqlx::query(
            "update prevention_decision set status = 'superseded', superseded_at = clock_timestamp() where id = $1",
        )
        .bind(old)
        .execute(&mut *tx)
        .await?;
    }
    let (record, document) = match &evidence {
        Some((t, id)) if t == "document" => (None, Some(*id)),
        Some((t, id)) => (Some((t.clone(), *id)), None),
        None => (None, None),
    };
    let id: Uuid = sqlx::query_scalar(
        "insert into prevention_decision (profile_id, item_key, kind, until, reason, evidence_table, evidence_id, \
         document_id, decided_by, supersedes) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning id",
    )
    .bind(pid)
    .bind(&key)
    .bind(kind)
    .bind(until)
    .bind(&reason)
    .bind(record.as_ref().map(|(t, _)| t.clone()))
    .bind(record.as_ref().map(|(_, id)| *id))
    .bind(document)
    .bind(caller.0.user_id)
    .bind(old)
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        &caller.0,
        &perm,
        "prevention_decision",
        &[id],
        format!("{kind} {key}"),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": id, "item_key": key, "kind": kind, "until": until})),
    ))
}

async fn undo(State(rt): State<Runtime>, caller: Caller, Path((p, key)): Path<(String, String)>) -> Answer {
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), Some("write")).await?;
    // a lab gap the caller's scopes withhold has no decision they can see
    let hidden = lab_key(&key)? && !perm.scopes.contains("read:labs");
    let id = if hidden {
        None
    } else {
        current_decision(&mut tx, perm.profile.id, &key).await?
    }
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown", "No decision about this item."))?;
    sqlx::query(
        "update prevention_decision set status = 'retracted', status_reason = 'undone', \
         superseded_at = clock_timestamp() where id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        &caller.0,
        &perm,
        "prevention_decision",
        &[id],
        format!("undo {key}"),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(json!({"item_key": key, "retracted": id}))))
}

// ------------------------------------------------------------------- answers ----

type AnswerRow = (String, String, String, chrono::DateTime<chrono::Utc>);

/// The questions that apply to the profile (its country's rows, for its sex and age) and every key it
/// already answered, with the current answers.
async fn answers_out(conn: &mut PgConnection, perm: &Permission, lang: &str) -> Result<Value, ApiError> {
    let pack = ownhealth_rules::load_pack().map_err(ownhealth_core::Error::from)?;
    let prev = prevention::load(&pack)?;
    let rows: Vec<AnswerRow> = sqlx::query_as(
        "select key, value, src_id, recorded_at from prevention_answer where profile_id = $1 and status = 'current'",
    )
    .bind(perm.profile.id)
    .fetch_all(conn)
    .await?;
    let given: BTreeMap<String, AnswerRow> = rows.into_iter().map(|r| (r.0.clone(), r)).collect();
    let applies = prevention::answer_keys_for(
        prev,
        perm.profile.country.as_deref(),
        perm.profile.sex_at_birth.as_deref(),
        perm.profile.age_on(today_utc()),
    );
    let items: Vec<Value> = prev
        .answer_keys
        .iter()
        .filter(|k| applies.contains(k) || given.contains_key(*k))
        .map(|k| {
            let a = given.get(k);
            json!({
                "key": k,
                "question": prevention::question(prev, k, lang),
                "value": a.map(|a| &a.1),
                "src": a.map(|a| &a.2),
                "recorded_at": a.map(|a| a.3),
            })
        })
        .collect();
    Ok(json!({"items": items}))
}

async fn list_answers(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Answer {
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    let lang = q.get("lang").map_or(caller.0.lang.as_str(), String::as_str);
    let out = answers_out(&mut tx, &perm, lang).await?;
    let mut ev = event(&caller.0, perm.profile.id, "GET /api/profiles/{p}/prevention-answers");
    ev.resource_type = Some("prevention_answer".into());
    write_audit(&mut tx, &ev).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(out)))
}

async fn put_answers(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let body = body_object(body)?;
    let pack = ownhealth_rules::load_pack().map_err(ownhealth_core::Error::from)?;
    let prev = prevention::load(&pack)?;
    let mut wanted: Vec<(String, Option<String>)> = Vec::new();
    for (k, v) in &body {
        if !prev.answer_keys.contains(k) {
            return Err(bad_input(
                "unknown_answer",
                format!("{k}: not one of {}.", prev.answer_keys.join(", ")),
            ));
        }
        let v = match v {
            Value::Null => None,
            Value::String(s) if s == "yes" || s == "no" => Some(s.clone()),
            _ => return Err(bad_input("bad_input", format!("{k}: \"yes\", \"no\" or null."))),
        };
        wanted.push((k.clone(), v));
    }
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), Some("write")).await?;
    let pid = perm.profile.id;
    let mut ids = Vec::new();
    let mut keys = Vec::new();
    for (key, value) in wanted {
        let old: Option<(Uuid, String)> = sqlx::query_as(
            "select id, value from prevention_answer where profile_id = $1 and key = $2 and status = 'current' \
             for update",
        )
        .bind(pid)
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;
        if old.as_ref().map(|o| &o.1) == value.as_ref() {
            continue;
        }
        if let Some((old_id, _)) = &old {
            let status = if value.is_some() { "superseded" } else { "retracted" };
            sqlx::query(
                "update prevention_answer set status = $2, status_reason = $3, superseded_at = clock_timestamp() \
                 where id = $1",
            )
            .bind(old_id)
            .bind(status)
            .bind(value.is_none().then_some("cleared"))
            .execute(&mut *tx)
            .await?;
            ids.push(*old_id);
        }
        if let Some(v) = value {
            let src = create_src(
                &mut tx,
                &SrcIn {
                    profile_id: Some(pid),
                    label: Some("manual".into()),
                    ..SrcIn::manual(caller.0.user_id)
                },
            )
            .await?;
            let id: Uuid = sqlx::query_scalar(
                "insert into prevention_answer (profile_id, key, value, src_id, supersedes) \
                 values ($1, $2, $3, $4, $5) returning id",
            )
            .bind(pid)
            .bind(&key)
            .bind(&v)
            .bind(&src)
            .bind(old.map(|o| o.0))
            .fetch_one(&mut *tx)
            .await?;
            ids.push(id);
        }
        keys.push(key);
    }
    if !ids.is_empty() {
        audit(&mut tx, &caller.0, &perm, "prevention_answer", &ids, keys.join(" ")).await?;
    }
    // the reply speaks the screen's language, as the GET does
    let lang = q.get("lang").map_or(caller.0.lang.as_str(), String::as_str);
    let out = answers_out(&mut tx, &perm, lang).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(out)))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/due", get(get_due))
        .route("/profiles/{p}/due/{item_key}/decision", post(decide).delete(undo))
        .route("/profiles/{p}/prevention-answers", get(list_answers).put(put_answers))
}
