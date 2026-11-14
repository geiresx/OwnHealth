//! Lane C: `api/records.py`. Records and manual entry: conditions, medications, allergies, vaccines,
//! procedures, visits, and observations typed by hand. A correction is a new version; the old one
//! stays, superseded.
//!
//! Reads need `read:records` and write one audit row. Writes need the role action `write` and a
//! browser session: a personal access token never reaches these routes (`Caller` refuses it; agents
//! write only through MCP `propose`, into the review queue).

use axum::extract::multipart::MultipartRejection;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Utc};
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, PrincipalKind};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::observations::get_observation;
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::pyjson::pydantic_datetime_utc;

use crate::validate::pydantic_datetimes;
use ownhealth_core::records::{self as rec, RecordIn, RecordOptions};
use ownhealth_ingest_structured::manual::{ManualObservation, add_manual_observation, refusal as manual_refusal};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::Caller;

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/records/{kind}", get(list).post(add))
        .route("/profiles/{p}/records/{kind}/{record_id}/correct", post(correct))
        .route("/profiles/{p}/records/{kind}/{record_id}/retract", post(retract))
        .route("/profiles/{p}/manual-observations", post(manual_observation))
        .route(
            "/profiles/{p}/records/medication/{record_id}/label",
            post(add_label).layer(DefaultBodyLimit::max(LABEL_MAX_BYTES + 64 * 1024)),
        )
}

const KINDS: [&str; 6] = [
    "condition",
    "medication",
    "allergy",
    "immunization",
    "procedure",
    "visit",
];
const HIDDEN: [&str; 8] = [
    "profile_id",
    "src_id",
    "fhir",
    "supersedes",
    "superseded_at",
    "status",
    "status_reason",
    "id",
];
const CATEGORIES: [&str; 5] = ["laboratory", "vital-signs", "survey", "exam", "activity"];

/// An error either way FastAPI answers: `{"detail": {code, message}}` or the validation list
/// `{"detail": [{type, loc, msg, input}]}`.
pub enum Failure {
    Api(ApiError),
    Validation(Vec<Value>),
}

impl<E: Into<ApiError>> From<E> for Failure {
    fn from(e: E) -> Failure {
        Failure::Api(e.into())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Failure::Api(e) => e.into_response(),
            Failure::Validation(errors) => {
                (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "detail": errors }))).into_response()
            }
        }
    }
}

type Answer = Result<(StatusCode, Json<Value>), Failure>;

fn invalid(kind: &str, loc: &[&str], msg: &str, input: Value) -> Failure {
    Failure::Validation(vec![json!({"type": kind, "loc": loc, "msg": msg, "input": input})])
}

fn bad_input(message: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bad_input", message)
}

fn unknown(message: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown", message)
}

fn kind_param(kind: &str) -> Result<&'static str, Failure> {
    KINDS.iter().find(|k| **k == kind).copied().ok_or_else(|| {
        invalid(
            "literal_error",
            &["path", "kind"],
            "Input should be 'condition', 'medication', 'allergy', 'immunization', 'procedure' or 'visit'",
            json!(kind),
        )
    })
}

fn uuid_param(loc: &[&str], v: &str) -> Result<Uuid, Failure> {
    Uuid::parse_str(v).map_err(|_| {
        invalid(
            "uuid_parsing",
            loc,
            "Input should be a valid UUID, invalid character: expected an optional prefix of `urn:uuid:` followed by [0-9a-fA-F-], found `x` at 1",
            json!(v),
        )
    })
}

fn body_object(body: Result<Json<Value>, JsonRejection>) -> Result<Map<String, Value>, Failure> {
    match body {
        Ok(Json(Value::Object(m))) => Ok(m),
        Ok(Json(other)) => Err(invalid(
            "dict_type",
            &["body"],
            "Input should be a valid dictionary",
            other,
        )),
        Err(e) => Err(invalid(
            "json_invalid",
            &["body"],
            "JSON decode error",
            json!(e.body_text()),
        )),
    }
}

fn title(kind: &str, r: &Map<String, Value>) -> Value {
    let s = |k: &str| r.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    match kind {
        "medication" => json!(
            [s("name"), s("strength")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ")
        ),
        "allergy" => r.get("substance").cloned().unwrap_or(Value::Null),
        "immunization" => r.get("vaccine").cloned().unwrap_or(Value::Null),
        // the reason, else the clinic; the day is in the detail
        "visit" => json!(s("reason_text").or(s("provider_name")).unwrap_or_default()),
        _ => r.get("text").cloned().unwrap_or(Value::Null),
    }
}

fn record_out(kind: &str, r: &Map<String, Value>) -> Value {
    let field = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
    let detail: Map<String, Value> = r
        .iter()
        .filter(|(k, _)| !HIDDEN.contains(&k.as_str()) && k.as_str() != "recorded_at")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // every records route is annotated `-> dict`: pydantic writes the row datetimes with `Z`
    let mut detail = Value::Object(detail);
    pydantic_datetimes(&mut detail, &["effective_at", "period_start", "period_end"]);
    let mut out = json!({
        "id": field("id"),
        "kind": kind,
        "title": title(kind, r),
        "detail": detail,
        "status": field("status"),
        "status_reason": field("status_reason"),
        "supersedes": field("supersedes"),
        "src": field("src_id"),
        "recorded_at": field("recorded_at"),
    });
    pydantic_datetimes(&mut out, &["recorded_at"]);
    out
}

fn via(p: &Principal) -> &'static str {
    if p.kind == PrincipalKind::Token { "api" } else { "ui" }
}

async fn audit(
    conn: &mut PgConnection,
    principal: &Principal,
    perm: &Permission,
    action: &str,
    rtype: &str,
    ids: &[String],
) -> Result<(), ApiError> {
    let mut ev = AuditEvent::new(principal.actor_kind(), via(principal), action);
    ev.actor_id = principal.user_id;
    ev.token_id = principal.token_id;
    ev.tool = Some("records".into());
    ev.profile_id = Some(perm.profile.id);
    ev.resource_type = Some(rtype.into());
    ev.resource_ids = Some(ids.to_vec());
    ev.rows_out = Some(i32::try_from(ids.len()).unwrap_or(i32::MAX));
    write_audit(conn, &ev).await?;
    Ok(())
}

fn id_of(r: &Map<String, Value>) -> String {
    r.get("id").and_then(Value::as_str).unwrap_or_default().to_string()
}

async fn writer(conn: &mut PgConnection, caller: &Caller, p: &str) -> Result<Permission, ApiError> {
    caller.require(conn, p, None, Some("write")).await
}

fn model(kind: &str, profile_id: Uuid, body: &Map<String, Value>) -> Result<RecordIn, Failure> {
    let mut data: Map<String, Value> = body
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "profile_id" | "fhir" | "reason"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    data.insert("profile_id".into(), json!(profile_id.to_string()));
    // a medicine read and sent back as it came keeps its reason: the marked text is unwrapped
    if let Some(text) = data
        .get("decision_reason")
        .filter(|v| ownhealth_core::text::is_untrusted(v))
        .and_then(|v| v.get("text"))
        .cloned()
    {
        data.insert("decision_reason".into(), text);
    }
    let refused = |errors: Vec<Value>| Failure::Api(bad_input("Invalid record.").with("errors", json!(errors)));
    lax_record(kind, &mut data).map_err(refused)?;
    RecordIn::from_json(kind, Value::Object(data)).map_err(|e| refused(vec![json!({"msg": e.to_string()})]))
}

/// A field of a record model as pydantic types it.
#[derive(Clone, Copy)]
enum Field {
    Str,
    OptStr,
    OptDate,
    OptDatetime,
    OptInt,
    Literal(&'static [&'static str]),
    OptLiteral(&'static [&'static str]),
    Sensitivity,
    /// `[{system?, code, display?}]`, the codings of a medicine as received.
    OptCodes,
}

const COMMON: [(&str, Field, bool); 2] = [
    ("effective_at", Field::OptDatetime, false),
    ("sensitivity", Field::Sensitivity, false),
];

/// The fields of `core/records.py`'s models after `_Common`, in model order: (name, type, required).
fn record_fields(kind: &str) -> &'static [(&'static str, Field, bool)] {
    use Field::*;
    const CODED: [(&str, Field, bool); 2] = [("code_system", OptStr, false), ("code", OptStr, false)];
    match kind {
        "condition" => &[
            ("text", Str, true),
            CODED[0],
            CODED[1],
            ("clinical_status", Str, false),
            (
                "verification_status",
                Literal(&[
                    "provisional",
                    "confirmed",
                    "refuted",
                    "unconfirmed",
                    "differential",
                    "entered-in-error",
                ]),
                false,
            ),
            ("onset", OptDate, false),
            ("abatement", OptDate, false),
        ],
        "medication" => &[
            ("kind", Literal(&["medication", "supplement"]), false),
            ("name", Str, true),
            ("ingredient", OptStr, false),
            ("strength", OptStr, false),
            ("dose_text", OptStr, false),
            CODED[0],
            CODED[1],
            ("started", OptDate, false),
            ("stopped", OptDate, false),
            ("med_status", Literal(&["active", "stopped", "on_hold"]), false),
            ("reason_text", OptStr, false),
            ("schedule_text", OptStr, false),
            ("prescriber", OptStr, false),
            ("codes", OptCodes, false),
            ("decision", OptLiteral(&rec::DECISIONS), false),
            ("decision_reason", OptStr, false),
            ("decision_on", OptDate, false),
        ],
        "allergy" => &[
            ("substance", Str, true),
            CODED[0],
            CODED[1],
            ("criticality", OptLiteral(&["low", "high", "unable-to-assess"]), false),
            ("reaction_text", OptStr, false),
            ("clinical_status", Str, false),
            ("verification_status", Str, false),
        ],
        "immunization" => &[
            ("vaccine", Str, true),
            CODED[0],
            CODED[1],
            ("occurred", OptDate, false),
            ("dose_number", OptInt, false),
            ("lot", OptStr, false),
        ],
        "procedure" => &[("text", Str, true), CODED[0], CODED[1], ("performed", OptDate, false)],
        "visit" => &[
            ("class", OptStr, false),
            ("provider_name", OptStr, false),
            ("period_start", OptDatetime, true),
            ("period_end", OptDatetime, false),
            ("reason_text", OptStr, false),
        ],
        _ => &[],
    }
}

/// pydantic's lax validation of a record body: every failing field as `{type, loc, msg}` (what
/// 0.1.1 sent, without input and ctx), in model order. A valid body has its lax values made exact
/// (`"2"` -> 2, a midnight datetime -> its date) for the model's own parse.
fn lax_record(kind: &str, data: &mut Map<String, Value>) -> Result<(), Vec<Value>> {
    let mut errors = Vec::new();
    let mut fail = |loc: Vec<Value>, kind: &str, msg: &str| errors.push(json!({"type": kind, "loc": loc, "msg": msg}));
    for (name, field, required) in COMMON.iter().chain(record_fields(kind)) {
        let loc = vec![json!(name)];
        let Some(v) = data.get_mut(*name) else {
            if *required {
                fail(loc, "missing", "Field required");
            }
            continue;
        };
        let optional = matches!(
            field,
            Field::OptStr
                | Field::OptDate
                | Field::OptDatetime
                | Field::OptInt
                | Field::OptLiteral(_)
                | Field::OptCodes
        );
        if v.is_null() && optional {
            continue;
        }
        match field {
            Field::Str | Field::OptStr => {
                if !v.is_string() {
                    fail(loc, "string_type", "Input should be a valid string");
                }
            }
            Field::Literal(allowed) | Field::OptLiteral(allowed) => {
                if !v.as_str().is_some_and(|s| allowed.contains(&s)) {
                    let msg = format!("Input should be {}", super::auth::literal_expected(allowed));
                    fail(loc, "literal_error", &msg);
                }
            }
            Field::OptDate => match v.as_str().map(crate::validate::parse_date) {
                Some(Ok(d)) => *v = json!(d.format("%Y-%m-%d").to_string()),
                Some(Err((kind, msg, _))) => fail(loc, kind, &msg),
                None => fail(loc, "date_type", "Input should be a valid date"),
            },
            Field::OptDatetime => match v.as_str().map(crate::validate::datetime_date_refusal) {
                Some(None) => {}
                Some(Some((kind, msg, _))) => fail(loc, kind, &msg),
                None => fail(loc, "datetime_type", "Input should be a valid datetime"),
            },
            Field::OptInt => match crate::validate::lax_int(v) {
                Ok(n) => *v = json!(n),
                Err((kind, msg)) => fail(loc, kind, msg),
            },
            Field::OptCodes => match v.as_array() {
                None => fail(loc, "list_type", "Input should be a valid list"),
                Some(items) => {
                    for (i, c) in items.iter().enumerate() {
                        let at = |k: &str| vec![json!(name), json!(i), json!(k)];
                        let Some(c) = c.as_object() else {
                            fail(
                                vec![json!(name), json!(i)],
                                "dict_type",
                                "Input should be a valid dictionary",
                            );
                            continue;
                        };
                        match c.get("code") {
                            Some(Value::String(s)) if !s.trim().is_empty() => {}
                            Some(Value::String(_)) => fail(
                                at("code"),
                                "string_too_short",
                                "String should have at least 1 character",
                            ),
                            None => fail(at("code"), "missing", "Field required"),
                            Some(_) => fail(at("code"), "string_type", "Input should be a valid string"),
                        }
                        for k in ["system", "display"] {
                            if c.get(k).is_some_and(|x| !x.is_null() && !x.is_string()) {
                                fail(at(k), "string_type", "Input should be a valid string");
                            }
                        }
                    }
                }
            },
            Field::Sensitivity => match v.as_array() {
                None => fail(loc, "list_type", "Input should be a valid list"),
                Some(items) => {
                    let allowed = &ownhealth_core::observations::SENSITIVITY;
                    for (i, s) in items.iter().enumerate() {
                        if !s.as_str().is_some_and(|s| allowed.contains(&s)) {
                            let msg = format!("Input should be {}", super::auth::literal_expected(allowed));
                            fail(vec![json!(name), json!(i)], "literal_error", &msg);
                        }
                    }
                }
            },
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn src(principal: &Principal, profile_id: Uuid) -> SrcRef {
    SrcRef::New(SrcIn {
        profile_id: Some(profile_id),
        label: Some("manual".into()), // a code: the UI says "entered by hand" in its language
        ..SrcIn::manual(principal.user_id)
    })
}

async fn owned(
    conn: &mut PgConnection,
    kind: &str,
    record_id: Uuid,
    perm: &Permission,
) -> Result<Map<String, Value>, ApiError> {
    match rec::get_record(conn, kind, record_id).await? {
        Some(row) if row.get("profile_id") == Some(&json!(perm.profile.id.to_string())) => Ok(row),
        _ => Err(unknown("No such record.")),
    }
}

async fn fetch(conn: &mut PgConnection, kind: &str, id: Uuid) -> Result<Map<String, Value>, ApiError> {
    rec::get_record(conn, kind, id)
        .await?
        .ok_or_else(|| unknown("No such record."))
}

/// Records as the routes answer them. A medicine or supplement also lists its label photos
/// (`labels`: document ids, newest first).
async fn records_out(conn: &mut PgConnection, kind: &str, rows: &[Map<String, Value>]) -> Result<Vec<Value>, ApiError> {
    let mut out: Vec<Value> = rows.iter().map(|r| record_out(kind, r)).collect();
    if kind == "medication" {
        let ids: Vec<Uuid> = rows.iter().filter_map(|r| Uuid::parse_str(&id_of(r)).ok()).collect();
        let labels = rec::labels_of(conn, &ids).await?;
        for (row, o) in rows.iter().zip(out.iter_mut()) {
            // the person's reason is free text: a reader (an agent over the API too) gets it marked
            if let Some(r) = row
                .get("decision_reason")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
            {
                o["detail"]["decision_reason"] = json!({"trust": "untrusted_text", "origin": "person_note", "text": r});
            }
            let mine = labels
                .iter()
                .find(|(m, _)| m.to_string() == id_of(row))
                .map(|(_, d)| d.iter().map(Uuid::to_string).collect::<Vec<_>>())
                .unwrap_or_default();
            o["labels"] = json!(mine);
        }
    }
    Ok(out)
}

async fn one_out(conn: &mut PgConnection, kind: &str, id: Uuid) -> Result<Value, ApiError> {
    let row = fetch(conn, kind, id).await?;
    let mut out = records_out(conn, kind, std::slice::from_ref(&row)).await?;
    Ok(out.pop().unwrap_or(Value::Null))
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// pydantic's lax bool for a query parameter.
fn bool_param(q: &[(String, String)], key: &str) -> Result<bool, Failure> {
    match param(q, key).map(str::to_ascii_lowercase).as_deref() {
        None => Ok(false),
        Some("1" | "true" | "t" | "yes" | "y" | "on") => Ok(true),
        Some("0" | "false" | "f" | "no" | "n" | "off") => Ok(false),
        Some(_) => Err(invalid(
            "bool_parsing",
            &["query", key],
            "Input should be a valid boolean, unable to interpret input",
            json!(param(q, key)),
        )),
    }
}

// ---------------------------------------------------------------- records ----

/// Current records, newest first; `history=true` adds superseded and retracted versions.
async fn list(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, kind)): Path<(String, String)>,
    Query(q): Query<Vec<(String, String)>>,
) -> Answer {
    let kind = kind_param(&kind)?;
    let history = bool_param(&q, "history")?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    let mut rows = rec::list_records(&mut tx, kind, perm.profile.id, None, perm.has("read:sensitive")).await?;
    if history {
        let seen: Vec<String> = rows.iter().map(id_of).collect();
        let mut older = Vec::new();
        for r in &rows {
            let Ok(id) = Uuid::parse_str(&id_of(r)) else { continue };
            for h in rec::record_history(&mut tx, kind, id).await? {
                if !seen.contains(&id_of(&h)) {
                    older.push(h);
                }
            }
        }
        rows.extend(older);
    }
    let ids: Vec<String> = rows.iter().map(id_of).collect();
    audit(&mut tx, &caller.0, &perm, "read", kind, &ids).await?;
    let items = records_out(&mut tx, kind, &rows).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(json!({ "items": items }))))
}

async fn add(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, kind)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let kind = kind_param(&kind)?;
    let body = body_object(body)?;
    let mut tx = rt.pool.begin().await?;
    let perm = writer(&mut tx, &caller, &p).await?;
    let record = model(kind, perm.profile.id, &body)?;
    let committed = rec::add_record(
        &mut tx,
        &record,
        &src(&caller.0, perm.profile.id),
        RecordOptions::new("person"),
    )
    .await?;
    audit(&mut tx, &caller.0, &perm, "write", kind, &[committed.id.to_string()]).await?;
    let row = one_out(&mut tx, kind, committed.id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// Body: the full new version plus `reason`. The old version becomes superseded.
async fn correct(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, kind, record_id)): Path<(String, String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let kind = kind_param(&kind)?;
    let record_id = uuid_param(&["path", "record_id"], &record_id)?;
    let body = body_object(body)?;
    let mut tx = rt.pool.begin().await?;
    let perm = writer(&mut tx, &caller, &p).await?;
    owned(&mut tx, kind, record_id, &perm).await?;
    let record = model(kind, perm.profile.id, &body)?;
    let reason = body
        .get("reason")
        .filter(|r| ownhealth_ingest_structured::py::truthy(Some(r)))
        .map_or_else(
            || "corrected".to_string(),
            |r| ownhealth_ingest_structured::py::py_str(Some(r)),
        );
    let committed = rec::supersede_record(
        &mut tx,
        record_id,
        &record,
        &src(&caller.0, perm.profile.id),
        "person",
        &reason,
    )
    .await?;
    let ids = [record_id.to_string(), committed.id.to_string()];
    audit(&mut tx, &caller.0, &perm, "correct", kind, &ids).await?;
    let row = one_out(&mut tx, kind, committed.id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `RetractIn.reason`: a string of 1 to 500 characters.
fn retract_reason(body: &Map<String, Value>) -> Result<String, Failure> {
    match body.get("reason") {
        None => Err(invalid(
            "missing",
            &["body", "reason"],
            "Field required",
            Value::Object(body.clone()),
        )),
        Some(Value::String(s)) if s.chars().count() < 1 => Err(invalid(
            "string_too_short",
            &["body", "reason"],
            "String should have at least 1 character",
            json!(s),
        )),
        Some(Value::String(s)) if s.chars().count() > 500 => Err(invalid(
            "string_too_long",
            &["body", "reason"],
            "String should have at most 500 characters",
            json!(s),
        )),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(invalid(
            "string_type",
            &["body", "reason"],
            "Input should be a valid string",
            other.clone(),
        )),
    }
}

async fn retract(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, kind, record_id)): Path<(String, String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let kind = kind_param(&kind)?;
    let record_id = uuid_param(&["path", "record_id"], &record_id)?;
    let reason = retract_reason(&body_object(body)?)?;
    let mut tx = rt.pool.begin().await?;
    let perm = writer(&mut tx, &caller, &p).await?;
    owned(&mut tx, kind, record_id, &perm).await?;
    rec::retract_record(&mut tx, kind, record_id, reason.trim()).await?;
    audit(&mut tx, &caller.0, &perm, "retract", kind, &[record_id.to_string()]).await?;
    let row = one_out(&mut tx, kind, record_id).await?;
    tx.commit().await?;
    Ok((StatusCode::OK, Json(row)))
}

// ------------------------------------------------------------ label photos ----

/// A label photo is at most 15 MiB (a phone photo is 2 to 6).
const LABEL_MAX_BYTES: usize = 15 * 1024 * 1024;

/// The image type of a label photo from its first bytes: PNG, JPEG or WebP, which every browser shows.
fn label_mime(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// `POST /profiles/{p}/records/medication/{id}/label`, multipart with one `file`: store a photo of the
/// label as a document (doc_type `other`, no extraction job, no review item) and link it to the
/// current version of the record. A later version gets the link too.
async fn add_label(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, record_id)): Path<(String, String)>,
    form: Result<Multipart, MultipartRejection>,
) -> Answer {
    let record_id = uuid_param(&["path", "record_id"], &record_id)?;
    let too_large = || {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "A label photo is at most 15 MiB.",
        )
    };
    // a body past the route's limit fails while it is read: that is the size answer, not a parse error
    let bad_body = |e: axum::extract::multipart::MultipartError| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            too_large()
        } else {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "There was an error parsing the body",
            )
        }
    };
    let mut file: Option<(String, Vec<u8>)> = None;
    if let Ok(mut form) = form {
        while let Some(field) = form.next_field().await.map_err(bad_body)? {
            if field.name() != Some("file") || file.is_some() {
                continue;
            }
            let name = field
                .file_name()
                .filter(|f| !f.is_empty())
                .unwrap_or("label")
                .to_string();
            let bytes = field.bytes().await.map_err(bad_body)?;
            if bytes.len() > LABEL_MAX_BYTES {
                return Err(too_large().into());
            }
            file = Some((name, bytes.to_vec()));
        }
    }
    let Some((filename, bytes)) = file else {
        return Err(invalid("missing", &["body", "file"], "Field required", Value::Null));
    };
    let Some(mime) = label_mime(&bytes) else {
        return Err(ownhealth_core::Error::Upload("A label photo is a PNG, JPEG or WebP image.".into()).into());
    };
    let mut tx = rt.pool.begin().await?;
    let perm = writer(&mut tx, &caller, &p).await?;
    owned(&mut tx, "medication", record_id, &perm).await?;
    // only the current version takes a new photo; older versions keep the ones they had
    ownhealth_core::lifecycle::lock_current(&mut tx, "medication_statement", record_id).await?;
    let blobs = rt.blobs.clone();
    let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let sha = tokio::task::spawn_blocking(move || blobs.put_bytes(&bytes))
        .await
        .map_err(|e| ownhealth_core::Error::Other(e.into()))?
        .map_err(|e| ownhealth_core::Error::Other(e.into()))?;
    let doc = ownhealth_core::documents::create_document(
        &mut tx,
        &ownhealth_core::documents::NewDocument {
            filename: Some(&filename),
            doc_type: Some("other"),
            ..ownhealth_core::documents::NewDocument::upload(
                perm.profile.household_id,
                Some(perm.profile.id),
                &sha,
                mime,
                size,
            )
        },
    )
    .await?;
    rec::add_label(&mut tx, record_id, doc.id).await?;
    audit(
        &mut tx,
        &caller.0,
        &perm,
        "write",
        "medication_label",
        &[record_id.to_string(), doc.id.to_string()],
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"medication_id": record_id, "document_id": doc.id})),
    ))
}

// ------------------------------------------------------ manual observations ----

struct ManualIn {
    name: String,
    value: String,
    unit: Option<String>,
    effective_at: Option<DateTime<FixedOffset>>,
    note: Option<String>,
    category: Option<String>,
    loinc: Option<String>,
    corrects: Option<Uuid>,
    reason: Option<String>,
}

/// `^\d{1,7}-\d$`
fn loinc_shape(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    match s.split_once('-') {
        Some((a, b)) => {
            (1..=7).contains(&a.len())
                && a.chars().all(|c| c.is_ascii_digit())
                && b.len() == 1
                && b.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Parse a datetime as pydantic does: `None` when it is valid but naive (no offset).
fn datetime_field(s: &str) -> Result<Option<DateTime<FixedOffset>>, ()> {
    if let Ok(dt) = ownhealth_core::pyjson::parse_datetime(s) {
        return Ok(Some(dt));
    }
    let t = s.trim().replacen(' ', "T", 1);
    if NaiveDateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
        || NaiveDateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M").is_ok()
        || NaiveDate::parse_from_str(&t, "%Y-%m-%d").is_ok()
    {
        return Ok(None);
    }
    Err(())
}

/// `ManualObservationIn`: every field checked, every failure listed as pydantic lists them.
fn manual_in(body: &Map<String, Value>) -> Result<ManualIn, Failure> {
    let mut errors: Vec<Value> = Vec::new();
    let mut err = |kind: &str, field: &str, msg: &str, input: Value| {
        errors.push(json!({"type": kind, "loc": ["body", field], "msg": msg, "input": input}));
    };
    let mut text = |field: &str, strip: bool, min: usize, max: usize, required: bool| -> Option<String> {
        match body.get(field) {
            None if required => {
                err("missing", field, "Field required", Value::Object(body.clone()));
                None
            }
            None => None,
            Some(Value::Null) if !required => None,
            Some(Value::String(s)) => {
                let s = if strip { s.trim().to_string() } else { s.clone() };
                let n = s.chars().count();
                if n < min {
                    err(
                        "string_too_short",
                        field,
                        &format!("String should have at least {min} character"),
                        json!(s),
                    );
                    None
                } else if n > max {
                    err(
                        "string_too_long",
                        field,
                        &format!("String should have at most {max} characters"),
                        json!(s),
                    );
                    None
                } else {
                    Some(s)
                }
            }
            Some(other) => {
                err("string_type", field, "Input should be a valid string", other.clone());
                None
            }
        }
    };
    let name = text("name", true, 1, 200, true);
    let value = text("value", true, 1, 100, true);
    let unit = text("unit", false, 0, 50, false);
    let note = text("note", false, 0, 4000, false);
    let reason = text("reason", false, 0, 500, false);
    let category = text("category", false, 0, usize::MAX, false);
    let loinc = text("loinc", false, 0, usize::MAX, false);
    let corrects = text("corrects", false, 0, usize::MAX, false);
    let effective = match body.get("effective_at") {
        None => {
            err("missing", "effective_at", "Field required", Value::Object(body.clone()));
            None
        }
        Some(Value::String(s)) => match datetime_field(s) {
            Ok(dt) => Some(dt),
            Err(()) => {
                err(
                    "datetime_from_date_parsing",
                    "effective_at",
                    "Input should be a valid datetime or date, invalid character in year",
                    json!(s),
                );
                None
            }
        },
        Some(other) => {
            err(
                "datetime_type",
                "effective_at",
                "Input should be a valid datetime",
                other.clone(),
            );
            None
        }
    };
    if let Some(c) = &category
        && !CATEGORIES.contains(&c.as_str())
    {
        err(
            "literal_error",
            "category",
            "Input should be 'laboratory', 'vital-signs', 'survey', 'exam' or 'activity'",
            json!(c),
        );
    }
    if let Some(l) = &loinc
        && !loinc_shape(l)
    {
        err(
            "string_pattern_mismatch",
            "loinc",
            "String should match pattern '^\\d{1,7}-\\d$'",
            json!(l),
        );
    }
    let corrects_id = match &corrects {
        Some(c) => match Uuid::parse_str(c) {
            Ok(u) => Some(u),
            Err(_) => {
                err("uuid_parsing", "corrects", "Input should be a valid UUID", json!(c));
                None
            }
        },
        None => None,
    };
    if !errors.is_empty() {
        return Err(Failure::Validation(errors));
    }
    let (Some(name), Some(value), Some(effective_at)) = (name, value, effective) else {
        return Err(Failure::Validation(Vec::new()));
    };
    Ok(ManualIn {
        name,
        value,
        unit,
        effective_at,
        note,
        category,
        loinc,
        corrects: corrects_id,
        reason,
    })
}

async fn manual_observation(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Answer {
    let body = manual_in(&body_object(body)?)?;
    let mut tx = rt.pool.begin().await?;
    let perm = writer(&mut tx, &caller, &p).await?;
    let Some(effective_at) = body.effective_at else {
        return Err(bad_input("effective_at needs a UTC offset.").into());
    };
    if let Some(why) = manual_refusal(body.value.trim(), effective_at, Utc::now()) {
        return Err(bad_input(why).into());
    }
    if let Some(old) = body.corrects {
        match get_observation(&mut tx, old).await? {
            Some(o) if o.profile_id == perm.profile.id => {}
            _ => return Err(unknown("No such observation.").into()),
        }
    }
    let Some(user_id) = caller.0.user_id else {
        return Err(ApiError::unauthenticated().into());
    };
    let unit = body.unit.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let committed = add_manual_observation(
        &mut tx,
        &ManualObservation {
            profile_id: perm.profile.id,
            user_id,
            name: body.name.trim(),
            value: body.value.trim(),
            unit,
            effective_at,
            note: body.note.as_deref(),
            category: body.category.as_deref(),
            loinc: body.loinc.as_deref(),
            corrects: body.corrects,
            reason: body.reason.as_deref(),
        },
    )
    .await?;
    audit(
        &mut tx,
        &caller.0,
        &perm,
        "write",
        "observation",
        &[committed.id.to_string()],
    )
    .await?;
    let row = get_observation(&mut tx, committed.id)
        .await?
        .ok_or_else(|| unknown("No such observation."))?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": row.id,
            "src": row.src_id,
            "status": row.status,
            "supersedes": row.supersedes,
            "printed_name": row.printed_name,
            "printed_value": row.printed_value,
            "printed_unit": row.printed_unit,
            "alias_group": row.alias_group,
            "effective_at": pydantic_datetime_utc(&row.effective_at),
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loinc_pattern() {
        assert!(loinc_shape("2276-4") && loinc_shape("1234567-8"));
        assert!(!loinc_shape("12345678-1") && !loinc_shape("2276-44") && !loinc_shape("-4") && !loinc_shape("x-1"));
    }

    #[test]
    fn naive_and_aware_datetimes() {
        assert!(matches!(datetime_field("2026-11-06T08:00:00+01:00"), Ok(Some(_))));
        assert!(matches!(datetime_field("2026-11-06T08:00:00"), Ok(None)));
        assert!(datetime_field("yesterday").is_err());
    }
}
