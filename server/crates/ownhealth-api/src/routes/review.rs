//! Lane I: `api/review.py`. The review queue the split view works from.
//!
//! ```text
//! GET  /api/review                     ?status=open&profile=&kind=&document_id=&limit=100&offset=0
//! GET  /api/review/{id}
//! POST /api/review/{id}/accept         {"edits": {...} | null}
//! POST /api/review/{id}/reject         {"reason": "..."}
//! POST /api/review/{id}/reopen
//! POST /api/review/{id}/dismiss        optional {"reason": "..."}
//! POST /api/review/bulk-accept         {"document_id": uuid, "min_row_conf": 0.0}
//! ```
//!
//! An item on a profile the caller cannot review, or whose held values the caller's scopes would not
//! show, answers 404 like an unknown id.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use ownhealth_core::access::{Permission, Principal, accessible_profiles, effective_permission};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::review::{self, ItemQuery, ReviewItem};
use ownhealth_core::{Error, Runtime};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Caller;
use crate::error::ApiError;
use crate::routes::auth::{Fail, R};
use crate::validate::{Errors, Params, body_str, json_object, param, pydantic_datetimes, uuid_path};

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/review", get(list))
        .route("/review/bulk-accept", post(bulk_accept))
        .route("/review/{item_id}", get(get_one))
        .route("/review/{item_id}/accept", post(accept))
        .route("/review/{item_id}/reject", post(reject))
        .route("/review/{item_id}/reopen", post(reopen))
        .route("/review/{item_id}/dismiss", post(dismiss))
}

const OUT_KEYS: [&str; 17] = [
    "id",
    "kind",
    "profile_id",
    "status",
    "reasons",
    "document_id",
    "candidate_row_id",
    "observation_id",
    "payload",
    "row_conf",
    "proposed_by_token",
    "context_src_ids",
    "decided_by",
    "decided_at",
    "decision_reason",
    "result_ids",
    "created_at",
];

fn item_out(item: &ReviewItem) -> R<Value> {
    let full = serde_json::to_value(item).map_err(Error::from)?;
    let mut out = Map::new();
    for k in OUT_KEYS {
        out.insert(k.into(), full.get(k).cloned().unwrap_or(Value::Null));
    }
    // v0.2: the documents the proposing token read before the proposal; only on items that have them,
    // so the 0.2.0 item shape is unchanged everywhere else
    if let Some(docs) = full.get("context_documents").filter(|v| !v.is_null()) {
        out.insert("context_documents".into(), docs.clone());
    }
    Ok(Value::Object(out))
}

fn obs_scope(category: Option<&str>) -> &'static str {
    match category {
        Some("vital-signs" | "survey" | "exam") => "read:vitals",
        Some("activity") => "read:wearables",
        _ => "read:labs",
    }
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// A held payload carries the values it would write: show it only to someone whose scopes would
/// show the committed fact (its category scope, and read:sensitive for a sensitive one).
fn visible(item: &ReviewItem, perm: &Permission) -> bool {
    let empty = Map::new();
    let p = item.payload.as_object().unwrap_or(&empty);
    let mut observations: Vec<&Map<String, Value>> = Vec::new();
    if let Some(Value::Object(o)) = p.get("observation") {
        observations.push(o);
    }
    if let Some(Value::Array(rows)) = p.get("rows") {
        observations.extend(
            rows.iter()
                .filter_map(|r| r.get("observation"))
                .filter_map(Value::as_object),
        );
    }
    let record = p.get("record").and_then(Value::as_object);
    for fact in observations.iter().copied().chain(record) {
        if truthy(fact.get("sensitivity")) && !perm.has("read:sensitive") {
            return false;
        }
    }
    if record.is_some() && !perm.has("read:records") {
        return false;
    }
    observations.iter().all(|o| {
        let category = o.get("category").and_then(Value::as_str).filter(|c| !c.is_empty());
        perm.has(obs_scope(category))
    })
}

fn no_item() -> Fail {
    Fail::Api(ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "No such review item.",
    ))
}

fn cannot_review(perm: &Permission) -> Fail {
    Error::access_denied(
        "forbidden",
        format!("The role {} cannot review on this profile.", perm.role),
    )
    .into()
}

async fn check(conn: &mut PgConnection, principal: &Principal, item: Option<ReviewItem>) -> R<ReviewItem> {
    let item = item.ok_or_else(no_item)?;
    let Some(profile_id) = item.profile_id else {
        if item.household_id != principal.household_id {
            return Err(no_item());
        }
        return Ok(item);
    };
    let perm = match effective_permission(conn, principal, profile_id).await {
        Ok(p) => p,
        Err(Error::AccessDenied { .. }) => return Err(no_item()),
        Err(e) => return Err(e.into()),
    };
    if !perm.can("review") {
        return Err(cannot_review(&perm));
    }
    if !visible(&item, &perm) {
        return Err(no_item());
    }
    Ok(item)
}

async fn audit(conn: &mut PgConnection, principal: &Principal, item: &ReviewItem, action: &str) -> R<()> {
    let mut e = AuditEvent::new(principal.actor_kind(), "ui", action);
    e.actor_id = principal.user_id;
    e.tool = Some("review".into());
    e.profile_id = item.profile_id;
    e.resource_type = Some("review_item".into());
    e.resource_ids = Some(vec![item.id.to_string()]);
    write_audit(conn, &e).await?;
    Ok(())
}

async fn list(State(rt): State<Runtime>, caller: Caller, Query(q): Query<Params>) -> R<Json<Value>> {
    let principal = &caller.0;
    let mut e = Errors::new();
    let limit = e.int("query", &q, "limit", 100, Some(0), Some(500));
    let offset = e.int("query", &q, "offset", 0, Some(0), None);
    let document_id = param(&q, "document_id").and_then(|d| e.uuid("query", "document_id", d));
    e.done()?;
    let status = param(&q, "status").unwrap_or("open");
    let profile = param(&q, "profile").filter(|p| !p.is_empty());
    let kind = param(&q, "kind").filter(|k| !k.is_empty());

    let mut tx = rt.pool.begin().await?;
    let perms = accessible_profiles(&mut tx, principal).await?;
    let pids: Vec<Uuid> = perms
        .iter()
        .filter(|p| p.can("review") && profile.is_none_or(|slug| p.profile.slug == slug))
        .map(|p| p.profile.id)
        .collect();
    let items = review::list_items(
        &mut tx,
        &ItemQuery {
            household_id: principal.household_id,
            profile_ids: Some(pids.clone()),
            status: Some(status.to_string()).filter(|s| !s.is_empty()),
            kind: kind.map(str::to_string),
            document_id,
            limit,
            offset,
        },
    )
    .await?;
    let by_profile: HashMap<Uuid, &Permission> = perms.iter().map(|p| (p.profile.id, p)).collect();
    let shown: Vec<&ReviewItem> = items
        .iter()
        .filter(|i| match i.profile_id {
            None => true,
            Some(pid) => by_profile.get(&pid).is_some_and(|p| visible(i, p)),
        })
        .collect();
    let household = principal.household_id.unwrap_or_default();
    let open = review::count_open(&mut tx, household, Some(&pids)).await?;
    tx.commit().await?;
    // `-> dict`: pydantic's JSON mode writes the datetimes with `Z` (GET /review/{id} did not)
    let mut out = shown.iter().map(|i| item_out(i)).collect::<R<Vec<_>>>()?;
    for i in &mut out {
        pydantic_datetimes(i, &["decided_at", "created_at"]);
    }
    Ok(Json(json!({
        "items": out,
        "open": open,
        "withheld": items.len() - shown.len(),
    })))
}

async fn get_one(State(rt): State<Runtime>, caller: Caller, Path(item_id): Path<String>) -> R<Json<Value>> {
    let item_id = uuid_path("item_id", &item_id)?;
    let mut tx = rt.pool.begin().await?;
    let found = review::get_item(&mut tx, item_id).await?;
    let item = check(&mut tx, &caller.0, found).await?;
    tx.commit().await?;
    Ok(Json(item_out(&item)?))
}

async fn accept(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(item_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> R<Json<Value>> {
    let principal = &caller.0;
    let item_id = uuid_path("item_id", &item_id)?;
    let body = json_object(&headers, &bytes, true)?.unwrap_or_default();
    let edits: Option<Value> = match body.get("edits") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Object(_)) => Some(v.clone()),
        Some(other) => {
            let mut e = Errors::new();
            e.push(
                "dict_type",
                vec![json!("body"), json!("edits")],
                "Input should be a valid dictionary",
                other.clone(),
                None,
            );
            e.done()?;
            None
        }
    };
    let mut tx = rt.pool.begin().await?;
    let found = review::get_item(&mut tx, item_id).await?;
    let item = check(&mut tx, principal, found).await?;
    if item.kind == "document_identity" {
        // accepting moves the document: the target needs the same check as POST /documents/{id}/assign
        let from_edits = edits
            .as_ref()
            .and_then(|e| e.get("profile_id"))
            .filter(|v| truthy(Some(v)));
        let target = from_edits.or_else(|| item.payload.get("suggested_profile_id").filter(|v| truthy(Some(v))));
        if let Some(t) = target {
            let t = t.as_str().map_or_else(|| t.to_string(), str::to_string);
            caller.require(&mut tx, &t, None, Some("import")).await?;
        }
    }
    let created = review::accept(&mut tx, item_id, principal.user_id.unwrap_or_default(), edits.as_ref()).await?;
    audit(&mut tx, principal, &item, "review_accept").await?;
    tx.commit().await?;
    Ok(Json(
        json!({"id": item_id, "status": "accepted", "result_ids": created}),
    ))
}

async fn reject(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(item_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> R<Json<Value>> {
    let principal = &caller.0;
    let item_id = uuid_path("item_id", &item_id)?;
    let body = json_object(&headers, &bytes, true)?.unwrap_or_default();
    let mut e = Errors::new();
    let reason = body_str(&mut e, &body, "reason");
    e.done()?;
    let reason = reason.unwrap_or_default();
    let mut tx = rt.pool.begin().await?;
    let found = review::get_item(&mut tx, item_id).await?;
    let item = check(&mut tx, principal, found).await?;
    review::reject(&mut tx, item_id, principal.user_id.unwrap_or_default(), &reason).await?;
    audit(&mut tx, principal, &item, "review_reject").await?;
    tx.commit().await?;
    Ok(Json(json!({"id": item_id, "status": "rejected"})))
}

async fn reopen(State(rt): State<Runtime>, caller: Caller, Path(item_id): Path<String>) -> R<Json<Value>> {
    let principal = &caller.0;
    let item_id = uuid_path("item_id", &item_id)?;
    let mut tx = rt.pool.begin().await?;
    let found = review::get_item(&mut tx, item_id).await?;
    let item = check(&mut tx, principal, found).await?;
    review::reopen(&mut tx, item_id, principal.user_id.unwrap_or_default()).await?;
    audit(&mut tx, principal, &item, "review_reopen").await?;
    tx.commit().await?;
    Ok(Json(json!({"id": item_id, "status": "open"})))
}

async fn dismiss(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(item_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> R<Json<Value>> {
    let principal = &caller.0;
    let item_id = uuid_path("item_id", &item_id)?;
    let reason = match json_object(&headers, &bytes, false)? {
        None => None,
        Some(body) => {
            let mut e = Errors::new();
            let r = body_str(&mut e, &body, "reason");
            e.done()?;
            r
        }
    };
    let mut tx = rt.pool.begin().await?;
    let found = review::get_item(&mut tx, item_id).await?;
    let item = check(&mut tx, principal, found).await?;
    review::dismiss(
        &mut tx,
        item_id,
        principal.user_id.unwrap_or_default(),
        reason.as_deref(),
    )
    .await?;
    audit(&mut tx, principal, &item, "review_dismiss").await?;
    tx.commit().await?;
    Ok(Json(json!({"id": item_id, "status": "dismissed"})))
}

/// pydantic's lax float: a JSON number, or a string that parses as one.
fn body_float(e: &mut Errors, body: &Map<String, Value>, key: &str, default: f64) -> f64 {
    let loc = vec![json!("body"), json!(key)];
    match body.get(key) {
        None => default,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::String(s)) => match s.trim().parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                e.push(
                    "float_parsing",
                    loc,
                    "Input should be a valid number, unable to parse string as a number",
                    json!(s),
                    None,
                );
                default
            }
        },
        Some(other) => {
            e.push("float_type", loc, "Input should be a valid number", other.clone(), None);
            default
        }
    }
}

async fn bulk_accept(State(rt): State<Runtime>, caller: Caller, headers: HeaderMap, bytes: Bytes) -> R<Json<Value>> {
    let principal = &caller.0;
    let body = json_object(&headers, &bytes, true)?.unwrap_or_default();
    let mut e = Errors::new();
    let document_id = match body.get("document_id") {
        None => {
            e.missing(vec![json!("body"), json!("document_id")], Value::Object(body.clone()));
            None
        }
        Some(Value::String(s)) => e.uuid("body", "document_id", s),
        Some(other) => {
            e.push(
                "uuid_type",
                vec![json!("body"), json!("document_id")],
                "UUID input should be a string, bytes or UUID object",
                other.clone(),
                None,
            );
            None
        }
    };
    let min_row_conf = body_float(&mut e, &body, "min_row_conf", 0.0);
    e.done()?;
    let document_id = document_id.unwrap_or_default();

    let mut tx = rt.pool.begin().await?;
    let doc: Option<Option<Uuid>> = sqlx::query_scalar("select profile_id from document where id = $1")
        .bind(document_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(Some(profile_id)) = doc else {
        return Err(Fail::Api(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "No such document.",
        )));
    };
    let perm = effective_permission(&mut tx, principal, profile_id).await?;
    if !perm.can("review") {
        return Err(cannot_review(&perm));
    }
    let accepted = review::bulk_accept(
        &mut tx,
        document_id,
        principal.user_id.unwrap_or_default(),
        min_row_conf,
    )
    .await?;
    let mut a = AuditEvent::new(principal.actor_kind(), "ui", "review_bulk_accept");
    a.actor_id = principal.user_id;
    a.tool = Some("review".into());
    a.profile_id = Some(profile_id);
    a.resource_type = Some("document".into());
    a.resource_ids = Some(vec![document_id.to_string()]);
    a.rows_out = Some(i32::try_from(accepted.len()).unwrap_or(i32::MAX));
    write_audit(&mut tx, &a).await?;
    tx.commit().await?;
    Ok(Json(json!({"accepted": accepted})))
}
