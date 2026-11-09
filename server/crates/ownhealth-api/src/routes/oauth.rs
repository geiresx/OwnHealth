//! Lane E (v0.2): what the UI needs for OAuth clients (`ownhealth_auth::oauth`). The protocol
//! endpoints themselves (`/.well-known/oauth-authorization-server`, `/oauth/authorize`,
//! `/oauth/token`) are `ownhealth_mcp::oauth`, outside `/api`.
//!
//! | Route | What |
//! |---|---|
//! | `GET /oauth/requests/{id}` | the pending authorization the consent screen shows: client name and CIMD URL, the scopes it asked for, the caller's profiles |
//! | `POST /oauth/requests/{id}/approve` | `{scopes, profile_ids, default_profile?, identity_mode, days?}`; step-up; -> `{redirect}` |
//! | `POST /oauth/requests/{id}/deny` | -> `{redirect}` with `error=access_denied` |
//! | `GET /oauth/consents` | the caller's approved clients |
//! | `DELETE /oauth/consents/{id}` | revoke (no step-up: it only narrows access) |
//!
//! Only a person in the browser reaches these (`Caller` refuses access tokens); approving needs a
//! login in the last five minutes, as creating a PAT does.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use chrono::{DateTime, Utc};
use ownhealth_auth::oauth::{self, DEFAULT_SCOPE, DEFAULT_TOKEN_DAYS, GrantIn, MAX_TOKEN_DAYS};
use ownhealth_core::Runtime;
use ownhealth_core::access::Principal;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::pyjson::pydantic_datetime_utc;
use serde_json::{Value, json};
use uuid::Uuid;

use super::auth::{Body, Fail, Meta, R, StepUpCaller, detail, ok};
use crate::error::ApiError;
use crate::extract::{Caller, SessionCaller};

fn user_of(p: &Principal) -> R<Uuid> {
    p.user_id.ok_or_else(|| Fail::Api(ApiError::unauthenticated()))
}

fn id_of(raw: &str) -> R<Uuid> {
    Uuid::parse_str(raw).map_err(|_| Fail::Api(ApiError::not_found()))
}

fn ts(t: &Option<DateTime<Utc>>) -> Value {
    t.as_ref().map_or(Value::Null, |t| json!(pydantic_datetime_utc(t)))
}

fn issuer(rt: &Runtime) -> R<String> {
    let base = rt.settings.base_url.trim_end_matches('/').to_string();
    if !oauth::enabled(&base) {
        return Err(detail(
            StatusCode::NOT_FOUND,
            "oauth_disabled",
            "OAuth needs OWNHEALTH_BASE_URL to be an https address.",
        ));
    }
    Ok(base)
}

async fn get_request(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    issuer(&rt)?;
    let uid = user_of(&p)?;
    let id = id_of(&id)?;
    let mut conn = rt.pool.acquire().await?;
    let r = oauth::get_request(&mut conn, id).await?;
    let profiles: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "select p.id, p.slug, p.display_name, g.role from profile_grant g join profile p on p.id = g.profile_id \
         where g.user_id = $1 order by (g.role = 'owner') desc, p.display_name",
    )
    .bind(uid)
    .fetch_all(&mut *conn)
    .await?;
    let mut offered: Vec<String> = r.scopes.clone();
    if !offered.iter().any(|s| s == DEFAULT_SCOPE) {
        offered.insert(0, DEFAULT_SCOPE.into());
    }
    let redirect_host = oauth::url_host(&r.redirect_uri);
    ok(json!({
        "id": r.id,
        "client": {
            "client_id": r.client.client_id,
            // the document's own words: shown next to its URL, never instead of it
            "client_name": r.client.client_name,
            "client_uri": r.client.client_uri,
        },
        // flat copies for the consent screen (ui lib/api/inbox.ts OAuthRequestOut)
        "client_id": r.client.client_id,
        "client_name": r.client.client_name,
        "redirect_uri": r.redirect_uri,
        "redirect_host": redirect_host,
        "requested_scopes": r.scopes,
        "scopes": offered.iter().map(|s| json!({
            "scope": s,
            "requested": r.scopes.contains(s),
            "checked": s == DEFAULT_SCOPE,
        })).collect::<Vec<_>>(),
        "profiles": profiles.iter().map(|(id, slug, name, role)| json!({
            "id": id, "slug": slug, "display_name": name, "role": role,
        })).collect::<Vec<_>>(),
        "default_days": DEFAULT_TOKEN_DAYS,
        "max_days": MAX_TOKEN_DAYS,
        "expires_at": pydantic_datetime_utc(&r.expires_at),
    }))
}

async fn approve_request(
    State(rt): State<Runtime>,
    StepUpCaller(p): StepUpCaller,
    meta: Meta,
    Path(id): Path<String>,
    Body(grant): Body<GrantIn>,
) -> R {
    let iss = issuer(&rt)?;
    let uid = user_of(&p)?;
    let id = id_of(&id)?;
    let mut tx = rt.pool.begin().await?;
    let (consent_id, redirect) = oauth::approve_request(&mut tx, uid, id, &grant, &iss)
        .await
        .map_err(|e| match e {
            ownhealth_core::Error::Invalid(m) => detail(StatusCode::UNPROCESSABLE_ENTITY, "bad_input", &m),
            e => e.into(),
        })?;
    let mut scopes = grant.scopes.clone();
    scopes.sort();
    scopes.dedup();
    write_audit(
        &mut tx,
        &AuditEvent {
            actor_id: Some(uid),
            tool: Some("POST /api/oauth/requests/approve".into()),
            resource_type: Some("oauth_consent".into()),
            resource_ids: Some(vec![consent_id.to_string()]),
            args_summary: Some(format!(
                "scopes={};profiles={};days={}",
                scopes.join(","),
                grant.profile_ids.len(),
                grant.days.unwrap_or(DEFAULT_TOKEN_DAYS)
            )),
            ip: meta.ip.clone(),
            ..AuditEvent::new("user", "ui", "oauth_approve")
        },
    )
    .await?;
    tx.commit().await?;
    ok(json!({"consent_id": consent_id, "redirect": redirect}))
}

async fn deny_request(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let iss = issuer(&rt)?;
    let uid = user_of(&p)?;
    let id = id_of(&id)?;
    let mut tx = rt.pool.begin().await?;
    let redirect = oauth::deny_request(&mut tx, uid, id, &iss).await?;
    tx.commit().await?;
    ok(json!({"redirect": redirect}))
}

async fn list_consents(State(rt): State<Runtime>, SessionCaller(p): SessionCaller) -> R {
    let uid = user_of(&p)?;
    let mut conn = rt.pool.acquire().await?;
    let rows = oauth::list_consents(&mut conn, uid).await?;
    let ids: Vec<Uuid> = rows.iter().flat_map(|r| r.profile_ids.iter().copied()).collect();
    let slugs: Vec<(Uuid, String)> = sqlx::query_as("select id, slug from profile where id = any($1)")
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?;
    let slug = |id: &Uuid| slugs.iter().find(|(i, _)| i == id).map(|(_, s)| s.clone());
    let items: Vec<Value> = rows
        .iter()
        .map(|c| {
            let mut scopes = c.scopes.clone();
            scopes.sort();
            json!({
                "id": c.id,
                "client_id": c.client_id,
                "client_name": c.client_name,
                "scopes": scopes,
                "profiles": c.profile_ids.iter().filter_map(slug).collect::<Vec<_>>(),
                "profile_ids": c.profile_ids,
                "default_profile": c.default_profile.as_ref().and_then(slug),
                "identity_mode": c.identity_mode,
                "created_at": pydantic_datetime_utc(&c.created_at),
                "expires_at": pydantic_datetime_utc(&c.expires_at),
                "revoked_at": ts(&c.revoked_at),
                "last_used_at": ts(&c.last_used_at),
            })
        })
        .collect();
    ok(json!({"items": items}))
}

/// Revoking only takes access away, so it needs no step-up.
async fn revoke_consent(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Path(id): Path<String>) -> R {
    let uid = user_of(&p)?;
    let id = id_of(&id)?;
    let mut tx = rt.pool.begin().await?;
    if !oauth::revoke_consent(&mut tx, uid, id).await? {
        return Err(detail(
            StatusCode::NOT_FOUND,
            "unknown",
            "Unknown or already revoked client.",
        ));
    }
    write_audit(
        &mut tx,
        &AuditEvent {
            actor_id: Some(uid),
            tool: Some("DELETE /api/oauth/consents".into()),
            resource_type: Some("oauth_consent".into()),
            resource_ids: Some(vec![id.to_string()]),
            ..AuditEvent::new("user", "ui", "oauth_revoke")
        },
    )
    .await?;
    tx.commit().await?;
    ok(json!({"id": id, "revoked": true, "ok": true}))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/oauth/requests/{id}", get(get_request))
        .route("/oauth/requests/{id}/approve", post(approve_request))
        .route("/oauth/requests/{id}/deny", post(deny_request))
        .route("/oauth/consents", get(list_consents))
        .route("/oauth/consents/{id}", delete(revoke_consent))
}
