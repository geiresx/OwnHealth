//! Lane I: `api/core.py`. Who am I, which profiles can I see, where did a value come from.
//!
//! ```text
//! GET /api/me                  the user, household, auth kind, profiles, open review count
//! GET /api/profiles            the profile summaries alone
//! GET /api/sources/{src_id}    provenance of one value (?include_text=true adds the page text)
//! ```

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::NaiveDate;
use ownhealth_core::access::{Permission, Principal, accessible_profiles, effective_permission};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::config::Settings;
use ownhealth_core::documents::document_is_sensitive;
use ownhealth_core::provenance::resolve_src;
use ownhealth_core::{Error, Runtime, review};
use ownhealth_mcp::gate::{self, Why};
use serde_json::{Map, Value, json};
use sqlx::Row;
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::auth::{Fail, R};
use crate::validate::{Params, bool_param, pydantic_datetimes};

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/me", get(me))
        .route("/profiles", get(profiles))
        .route("/sources/{src_id}", get(source))
}

/// Python's `date.today()`: the server's local date.
pub fn local_today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// One profile as the UI's profile switcher reads it.
pub fn profile_summary(perm: &Permission, principal: &Principal, on: NaiveDate, age_of_majority: i32) -> Value {
    let p = &perm.profile;
    let initials: String = p
        .display_name
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .collect::<String>()
        .to_uppercase();
    let mut scopes: Vec<&String> = perm.scopes.iter().collect();
    scopes.sort();
    json!({
        "id": p.id,
        "slug": p.slug,
        "display_name": p.display_name,
        "initials": initials,
        "role": perm.role,
        "relationship": p.relationship,
        "age": p.age_on(on),
        "sex_at_birth": p.sex_at_birth,
        "country": p.country,
        "is_minor": p.is_minor(on, age_of_majority),
        "is_self": p.user_id.is_some() && p.user_id == principal.user_id,
        "unit_pref": p.unit_pref,
        "genome_sensitive_ui": p.genome_sensitive_ui,
        "scopes": scopes,
    })
}

fn relationship_rank(r: Option<&str>) -> u8 {
    match r {
        Some("self") => 0,
        Some("partner") => 1,
        Some("child") => 2,
        Some("parent") => 3,
        _ => 4,
    }
}

/// The caller first, then the household as the mockup lists it: partner, children, parents, others.
pub fn summaries(perms: &[Permission], principal: &Principal, settings: &Settings) -> Vec<Value> {
    let today = local_today();
    let mut rows: Vec<(bool, u8, String, Value)> = perms
        .iter()
        .map(|p| {
            let v = profile_summary(p, principal, today, settings.age_of_majority);
            let is_self = v["is_self"].as_bool().unwrap_or(false);
            (
                !is_self,
                relationship_rank(p.profile.relationship.as_deref()),
                p.profile.display_name.clone(),
                v,
            )
        })
        .collect();
    rows.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    rows.into_iter().map(|r| r.3).collect()
}

async fn me(State(rt): State<Runtime>, caller: crate::Caller) -> Result<Json<Value>, ApiError> {
    let principal = &caller.0;
    let mut tx = rt.pool.begin().await?;
    let user = sqlx::query(
        "select u.id, u.display_name, u.locale, h.id as household_id, h.name as household_name \
         from app_user u join household h on h.id = u.household_id where u.id = $1",
    )
    .bind(principal.user_id)
    .fetch_optional(&mut *tx)
    .await?
    // Python failed with a TypeError here: a 500
    .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Internal error."))?;
    let household_id: Uuid = user.try_get("household_id")?;
    let perms = accessible_profiles(&mut tx, principal).await?;
    let pids: Vec<Uuid> = perms.iter().map(|p| p.profile.id).collect();
    let open = review::count_open(&mut tx, household_id, Some(&pids)).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "user": {
            "id": user.try_get::<Uuid, _>("id")?,
            "display_name": user.try_get::<String, _>("display_name")?,
            "locale": user.try_get::<Option<String>, _>("locale")?,
        },
        "household": {"id": household_id, "name": user.try_get::<String, _>("household_name")?},
        "auth": {"kind": principal.kind},
        "profiles": summaries(&perms, principal, &rt.settings),
        "review_open": open,
        "demo": rt.settings.demo,
    })))
}

async fn profiles(State(rt): State<Runtime>, caller: crate::Caller) -> Result<Json<Value>, ApiError> {
    let mut tx = rt.pool.begin().await?;
    let perms = accessible_profiles(&mut tx, &caller.0).await?;
    tx.commit().await?;
    Ok(Json(Value::Array(summaries(&perms, &caller.0, &rt.settings))))
}

fn unknown_source() -> Fail {
    Fail::Api(ApiError::new(StatusCode::NOT_FOUND, "unknown", "Unknown source."))
}

/// Provenance of one value: document, page, bbox, extraction, and the facts that cite it.
///
/// An id on a profile the caller cannot read answers 404, the same as an id that never existed.
async fn source(
    State(rt): State<Runtime>,
    caller: crate::Caller,
    Path(src_id): Path<String>,
    Query(q): Query<Params>,
) -> R<Json<Value>> {
    let include_text = bool_param(&q, "include_text", false)?;
    let principal = &caller.0;
    let mut tx = rt.pool.begin().await?;
    let resolved = match resolve_src(&mut tx, &src_id).await? {
        Some(r) => r,
        None => return Err(unknown_source()),
    };
    let Some(profile_id) = resolved.profile_id else {
        return Err(unknown_source());
    };
    let perm = match effective_permission(&mut tx, principal, profile_id).await {
        Ok(p) => p,
        Err(Error::AccessDenied { .. }) => return Err(unknown_source()),
        Err(e) => return Err(e.into()),
    };
    // the scope of the fact behind the source decides, as in MCP get_source (mcp.md 5.7)
    if !gate::READ_SCOPES.iter().any(|s| perm.has(s)) {
        return Err(unknown_source());
    }
    let g = gate::genome_gate(&perm, local_today(), rt.settings.age_of_majority);
    let hidden = gate::hidden_reasons(&mut tx, &perm, g, std::slice::from_ref(&src_id)).await?;
    match hidden.get(&src_id) {
        Some(Why::Scope(label)) if label != "genome" => {
            return Err(Error::access_denied("insufficient_scope", format!("This needs read:{label}.")).into());
        }
        Some(_) => return Err(unknown_source()),
        None => {}
    }
    let s = &resolved.src;
    let document_id = s
        .get("document_id")
        .and_then(Value::as_str)
        .and_then(|d| Uuid::parse_str(d).ok());
    let page_no = s.get("page_no").and_then(Value::as_i64).filter(|n| *n != 0);
    let mut page_text: Option<String> = None;
    if include_text
        && let (Some(doc), Some(page)) = (document_id, page_no)
        && perm.has("read:documents_text")
        && (perm.has("read:sensitive") || !document_is_sensitive(&mut tx, doc).await?)
    {
        page_text = sqlx::query_scalar::<_, Option<String>>(
            "select text_content from document_page where document_id = $1 and page_no = $2",
        )
        .bind(doc)
        .bind(i32::try_from(page).unwrap_or(i32::MAX))
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
    }
    let mut audit = AuditEvent::new(principal.actor_kind(), "ui", "read");
    audit.actor_id = principal.user_id;
    audit.token_id = principal.token_id;
    audit.tool = Some("GET /api/sources".into());
    audit.profile_id = Some(profile_id);
    audit.resource_type = Some("src".into());
    audit.src_ids = Some(vec![src_id.clone()]);
    write_audit(&mut tx, &audit).await?;
    tx.commit().await?;

    let status = resolved
        .facts
        .first()
        .and_then(|f| f.get("status").cloned())
        .unwrap_or_else(|| json!("gone"));
    let facts: Vec<Value> = resolved
        .facts
        .iter()
        .map(|f| {
            let mut m = Map::new();
            for k in ["table", "id", "status", "status_reason"] {
                m.insert(k.into(), f.get(k).cloned().unwrap_or(Value::Null));
            }
            Value::Object(m)
        })
        .collect();
    let field = |k: &str| s.get(k).cloned().unwrap_or(Value::Null);
    let location = if page_no.is_some() {
        json!({"page": field("page_no"), "bbox": field("bbox"), "units": "pt"})
    } else {
        Value::Null
    };
    let text = match page_text.filter(|t| !t.is_empty()) {
        Some(t) => json!({"trust": "untrusted_text", "origin": "pdf_text_layer", "text": t}),
        None => Value::Null,
    };
    let mut document = resolved.document.map_or(Value::Null, Value::Object);
    pydantic_datetimes(&mut document, &["collected_at", "reported_at"]);
    let mut out = json!({
        "src_id": src_id,
        "kind": field("kind"),
        "label": field("label"),
        "status": status,
        "facts": facts,
        "document": document,
        "location": location,
        "import_ref": field("import_ref"),
        "created_at": field("created_at"),
        "text": text,
    });
    // `-> dict`: pydantic's JSON mode writes the datetimes with `Z`
    pydantic_datetimes(&mut out, &["created_at"]);
    Ok(Json(out))
}
