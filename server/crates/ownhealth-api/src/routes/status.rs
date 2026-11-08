//! Lane E: `api/status.py`. The deterministic status (the same body `get_health_status` serves), the
//! dashboard page and the installed rule pack.
//!
//! ```text
//! GET /api/profiles/{p}/status?as_of=&lang=&units=&identity_mode=   read:summary
//! GET /api/profiles/{p}/dashboard?as_of=&lang=                      read:summary
//! GET /api/rules                                                    any signed-in caller
//! ```
//!
//! The handlers are thin: each calls a `pub` function taking the principal, which is how the tests
//! reach a viewer as Python's did by overriding `get_principal`.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use chrono::NaiveDate;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, PrincipalKind, today_utc};
use ownhealth_core::audit::{AuditEvent, write_audit};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Check, R, ok};
use crate::extract::Caller;

/// The query parameters of the status and dashboard pages; empty means unset, as `x or default`.
#[derive(Debug, Clone, Default)]
pub struct StatusParams {
    pub as_of: Option<NaiveDate>,
    pub lang: Option<String>,
    pub units: Option<String>,
    pub identity_mode: Option<String>,
}

/// `as_of: date | None = Query(None)`: a bad date answers FastAPI's 422.
pub fn parse_date(q: &HashMap<String, String>, check: &mut Check) -> Option<NaiveDate> {
    let s = q.get("as_of")?;
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
    if d.is_none() {
        check.push(
            "date_from_datetime_parsing",
            "as_of",
            "Input should be a valid date or datetime, invalid character in year",
            json!(s),
        );
    }
    d
}

impl StatusParams {
    fn from_query(q: &HashMap<String, String>) -> R<StatusParams> {
        let mut check = Check::query();
        let as_of = parse_date(q, &mut check);
        check.done()?;
        let text = |k: &str| q.get(k).filter(|v| !v.is_empty()).cloned();
        Ok(StatusParams {
            as_of,
            lang: text("lang"),
            units: text("units"),
            identity_mode: text("identity_mode"),
        })
    }
}

/// One read row: the resource is the state hash, never a value.
pub(crate) fn event(principal: &Principal, profile_id: Uuid, tool: &str) -> AuditEvent {
    let via = if principal.kind == PrincipalKind::Token {
        "api"
    } else {
        "ui"
    };
    let mut e = AuditEvent::new(principal.actor_kind(), via, "read");
    e.actor_id = principal.user_id;
    e.token_id = principal.token_id;
    e.tool = Some(tool.into());
    e.profile_id = Some(profile_id);
    e
}

async fn audit(conn: &mut PgConnection, principal: &Principal, profile_id: Uuid, tool: &str, hash: &Value) -> R<()> {
    let mut e = event(principal, profile_id, tool);
    e.resource_type = Some("state".into());
    e.resource_ids = Some(vec![hash.as_str().unwrap_or_default().to_string()]);
    write_audit(conn, &e).await?;
    Ok(())
}

pub async fn status(rt: &Runtime, principal: &Principal, p: &str, q: &StatusParams) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:summary"), None)
        .await?;
    let body = ownhealth_rules::build_state(&mut tx, perm.profile.id, q.as_of.unwrap_or_else(today_utc), None).await?;
    audit(
        &mut tx,
        principal,
        perm.profile.id,
        "GET /api/profiles/{p}/status",
        &body["state_hash"],
    )
    .await?;
    tx.commit().await?;
    Ok(ownhealth_rules::render(
        &body,
        q.lang.as_deref().unwrap_or(&principal.lang),
        q.units.as_deref().unwrap_or(&principal.units),
        q.identity_mode.as_deref().unwrap_or(&principal.identity_mode),
        Some(&perm.scopes),
    )?)
}

pub async fn dashboard(rt: &Runtime, principal: &Principal, p: &str, q: &StatusParams) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:summary"), None)
        .await?;
    let lang = q.lang.as_deref().unwrap_or(&principal.lang);
    let as_of = q.as_of.unwrap_or_else(today_utc);
    let out = ownhealth_rules::dashboard(&mut tx, perm.profile.id, as_of, lang, &perm.scopes).await?;
    audit(
        &mut tx,
        principal,
        perm.profile.id,
        "GET /api/profiles/{p}/dashboard",
        &out["state_hash"],
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}

type Params = Query<HashMap<String, String>>;

async fn get_status(State(rt): State<Runtime>, Caller(c): Caller, Path(p): Path<String>, Query(q): Params) -> R {
    let q = StatusParams::from_query(&q)?;
    ok(status(&rt, &c, &p, &q).await?)
}

async fn get_dashboard(State(rt): State<Runtime>, Caller(c): Caller, Path(p): Path<String>, Query(q): Params) -> R {
    let q = StatusParams::from_query(&q)?;
    ok(dashboard(&rt, &c, &p, &q).await?)
}

/// The installed pack: version, hash and every rule with its citation. No personal data.
async fn get_rules(Caller(_): Caller) -> R {
    ok(ownhealth_rules::rules_summary()?)
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/status", get(get_status))
        .route("/profiles/{p}/dashboard", get(get_dashboard))
        .route("/rules", get(get_rules))
}
