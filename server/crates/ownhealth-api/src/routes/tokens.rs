//! Lane G: `api/tokens.py`. Personal access tokens for MCP clients: list, create (shown once,
//! step-up), revoke.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use ownhealth_auth::tokens::{self, DEFAULT_DAYS, MAX_DAYS, TokenListed};
use ownhealth_core::Runtime;
use ownhealth_core::access::PAT_DEFAULT_SCOPES;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::pyjson::pydantic_datetime_utc;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Body, Check, Fail, Meta, R, StepUpCaller, answer, detail, ok};
use crate::extract::{Caller, SessionCaller};

async fn slugs(conn: &mut PgConnection, ids: &[Uuid]) -> R<HashMap<Uuid, String>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as("select id, slug from profile where id = any($1)")
        .bind(ids)
        .fetch_all(conn)
        .await?;
    Ok(rows.into_iter().collect())
}

fn ts(t: &Option<chrono::DateTime<chrono::Utc>>) -> Value {
    t.as_ref().map_or(Value::Null, |t| json!(pydantic_datetime_utc(t)))
}

fn token_out(t: &TokenListed, slugs: &HashMap<Uuid, String>) -> Value {
    let mut scopes = t.scopes.clone();
    scopes.sort();
    let profiles: Vec<&String> = t.profile_ids.iter().filter_map(|p| slugs.get(p)).collect();
    json!({
        "id": t.id,
        "label": t.label,
        "prefix": t.prefix,
        "scopes": scopes,
        "profiles": profiles,
        "default_profile": t.default_profile.and_then(|p| slugs.get(&p)),
        "identity_mode": t.identity_mode,
        "lang": t.lang,
        "units": t.units,
        "limits": {
            "rate_per_min": t.rate_per_min,
            "egress_facts_per_day": t.egress_facts_per_day,
            "egress_bytes_per_day": t.egress_bytes_per_day,
            "genome_raw_lookups_per_day": t.genome_raw_lookups_per_day,
            "document_pages_per_day": t.document_pages_per_day,
            "proposals_per_day": t.proposals_per_day,
        },
        "created_at": pydantic_datetime_utc(&t.created_at),
        "expires_at": pydantic_datetime_utc(&t.expires_at),
        "last_used_at": ts(&t.last_used_at),
        "revoked_at": ts(&t.revoked_at),
    })
}

async fn list_out(conn: &mut PgConnection, user_id: Uuid) -> R<Vec<(Uuid, Value)>> {
    let rows = tokens::list_tokens(&mut *conn, user_id).await?;
    let mut ids: Vec<Uuid> = rows.iter().flat_map(|r| r.profile_ids.iter().copied()).collect();
    ids.sort();
    ids.dedup();
    let slugs = slugs(conn, &ids).await?;
    Ok(rows.iter().map(|r| (r.id, token_out(r, &slugs))).collect())
}

fn user_of(p: &ownhealth_core::access::Principal) -> R<Uuid> {
    p.user_id
        .ok_or_else(|| Fail::Api(crate::error::ApiError::unauthenticated()))
}

async fn list_my_tokens(State(rt): State<Runtime>, SessionCaller(p): SessionCaller) -> R {
    let uid = user_of(&p)?;
    let mut conn = rt.pool.acquire().await?;
    let items: Vec<Value> = list_out(&mut conn, uid).await?.into_iter().map(|(_, v)| v).collect();
    ok(json!({"items": items}))
}

#[derive(Debug, Deserialize)]
struct TokenIn {
    label: String,
    #[serde(default = "default_scopes")]
    scopes: Vec<String>,
    profiles: Vec<String>,
    #[serde(default)]
    default_profile: Option<String>,
    /// pydantic's lax `int`: `"30"`, `30.0` and `true` are accepted as 0.1.1 did.
    #[serde(default = "default_days")]
    days: Value,
    #[serde(default = "full")]
    identity_mode: String,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default = "si")]
    units: String,
}

fn default_scopes() -> Vec<String> {
    PAT_DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect()
}

fn default_days() -> Value {
    json!(DEFAULT_DAYS)
}

fn full() -> String {
    "full".into()
}

fn si() -> String {
    "si".into()
}

/// The plaintext is in this response and nowhere else, ever.
async fn create_my_token(
    State(rt): State<Runtime>,
    StepUpCaller(p): StepUpCaller,
    meta: Meta,
    Body(body): Body<TokenIn>,
) -> R {
    let mut check = Check::body();
    check
        .len("label", Some(&body.label), 1, 80)
        .items("scopes", body.scopes.len(), 1, usize::MAX)
        .items("profiles", body.profiles.len(), 1, 20);
    let days = match crate::validate::lax_int(&body.days) {
        Ok(d) => {
            check.range("days", d, body.days.clone(), 1, MAX_DAYS);
            d
        }
        Err((kind, msg)) => {
            check.push(kind, "days", msg, body.days.clone());
            DEFAULT_DAYS
        }
    };
    check
        .literal(
            "identity_mode",
            Some(&body.identity_mode),
            &["full", "pseudonymous", "minimal"],
        )
        .literal("lang", body.lang.as_deref(), &["es", "en"])
        .literal("units", Some(&body.units), &["si", "conventional", "as_reported"])
        .done()?;
    let uid = user_of(&p)?;
    let caller = Caller(p.clone());
    let mut tx = rt.pool.begin().await?;
    let mut ids = Vec::new();
    for r in &body.profiles {
        ids.push(caller.require(&mut tx, r, None, None).await?.profile.id);
    }
    let default = match body.default_profile.as_deref().filter(|d| !d.is_empty()) {
        Some(d) => Some(caller.require(&mut tx, d, None, None).await?.profile.id),
        None => None,
    };
    let mut unique = Vec::new();
    for id in &ids {
        if !unique.contains(id) {
            unique.push(*id);
        }
    }
    let lang = body.lang.clone().unwrap_or_else(|| p.lang.clone());
    let new = tokens::create_token(
        &mut tx,
        &tokens::TokenIn {
            user_id: uid,
            label: body.label.trim(),
            profile_ids: unique,
            scopes: body.scopes.clone(),
            default_profile: default,
            days,
            identity_mode: &body.identity_mode,
            lang: &lang,
            units: &body.units,
        },
    )
    .await
    .map_err(|e| match e {
        ownhealth_core::Error::Invalid(m) => detail(StatusCode::UNPROCESSABLE_ENTITY, "bad_input", &m),
        e => e.into(),
    })?;
    let mut scopes = body.scopes.clone();
    scopes.sort();
    scopes.dedup();
    write_audit(
        &mut tx,
        &AuditEvent {
            actor_id: Some(uid),
            token_id: Some(new.id),
            tool: Some("POST /api/tokens".into()),
            resource_type: Some("token".into()),
            resource_ids: Some(vec![new.id.to_string()]),
            args_summary: Some(format!(
                "scopes={};profiles={};days={}",
                scopes.join(","),
                ids.len(),
                body.days
            )),
            ip: meta.ip.clone(),
            ..AuditEvent::new("user", "ui", "token_create")
        },
    )
    .await?;
    let token = list_out(&mut tx, uid)
        .await?
        .into_iter()
        .find(|(id, _)| *id == new.id)
        .map(|(_, v)| v)
        .unwrap_or(Value::Null);
    tx.commit().await?;
    Ok(answer(
        StatusCode::CREATED,
        json!({"token": token, "plaintext": new.plaintext}),
        &[],
    ))
}

/// Revoking only takes access away, so it needs no step-up.
async fn revoke_my_token(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Path(token_id): Path<String>,
) -> R {
    let mut c = Check::path();
    let token_id = super::auth::uuid_param(&mut c, "token_id", Some(&token_id));
    c.done()?;
    let Some(token_id) = token_id else {
        return Err(Fail::Api(crate::error::ApiError::not_found()));
    };
    let uid = user_of(&p)?;
    let mut tx = rt.pool.begin().await?;
    if !tokens::revoke_token(&mut tx, uid, token_id).await? {
        return Err(detail(
            StatusCode::NOT_FOUND,
            "unknown",
            "Unknown or already revoked token.",
        ));
    }
    write_audit(
        &mut tx,
        &AuditEvent {
            actor_id: Some(uid),
            token_id: Some(token_id),
            tool: Some("DELETE /api/tokens".into()),
            resource_type: Some("token".into()),
            resource_ids: Some(vec![token_id.to_string()]),
            ..AuditEvent::new("user", "ui", "token_revoke")
        },
    )
    .await?;
    tx.commit().await?;
    ok(json!({"id": token_id, "revoked": true}))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/tokens", get(list_my_tokens).post(create_my_token))
        .route("/tokens/{token_id}", delete(revoke_my_token))
}
