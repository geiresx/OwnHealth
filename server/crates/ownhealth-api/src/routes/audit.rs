//! Lane G: `api/audit.py`. The audit log viewer: who (person or token) read or changed what, never
//! the payloads.
//!
//! A person sees every event on a profile they manage (owner, or guardian of an unclaimed profile),
//! and their own actions everywhere, including what their tokens did. A viewer of someone else's
//! profile does not see who else read it.

use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use chrono::{DateTime, Utc};
use ownhealth_auth::household::can_manage;
use ownhealth_core::Runtime;
use ownhealth_core::access::accessible_profiles;
use ownhealth_core::pyjson::pydantic_datetime_utc;
use serde_json::{Value, json};
use uuid::Uuid;

use super::auth::{R, ok};
use crate::extract::{Caller, SessionCaller};
use crate::validate;

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    ts: DateTime<Utc>,
    actor_kind: String,
    via: String,
    action: String,
    tool: Option<String>,
    resource_type: Option<String>,
    resource_ids: Option<Vec<String>>,
    src_ids: Option<Vec<String>>,
    args_summary: Option<String>,
    rows_out: Option<i32>,
    bytes_out: Option<i32>,
    result_kind: Option<String>,
    withheld: Option<Value>,
    user_name: Option<String>,
    token_label: Option<String>,
    profile_slug: Option<String>,
}

async fn audit_log(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Query(q): Query<validate::Params>) -> R {
    let mut e = validate::Errors::new();
    let limit = e.int("query", &q, "limit", 100, Some(1), Some(500));
    let before = validate::param(&q, "before").map(|_| e.int("query", &q, "before", 0, Some(1), None));
    e.done()?;
    let mut tx = rt.pool.begin().await?;
    let managed: Vec<Uuid> = accessible_profiles(&mut tx, &p)
        .await?
        .iter()
        .filter(|perm| can_manage(perm))
        .map(|perm| perm.profile.id)
        .collect();
    let profile = validate::param(&q, "profile").filter(|s| !s.is_empty());
    let pid = match profile {
        Some(r) => Some(Caller(p.clone()).require(&mut tx, r, None, None).await?.profile.id),
        None => None,
    };
    let rows: Vec<Row> = sqlx::query_as(
        "select e.id, e.ts, e.actor_kind, e.via, e.action, e.tool, e.resource_type, e.resource_ids, e.src_ids, \
         e.args_summary, e.rows_out, e.bytes_out, e.result_kind, e.withheld, u.display_name as user_name, \
         t.label as token_label, p.slug as profile_slug \
         from audit_event e left join app_user u on u.id = e.actor_id \
         left join api_token t on t.id = e.token_id left join profile p on p.id = e.profile_id \
         where (e.profile_id = any($1) or e.actor_id = $2) and ($3::uuid is null or e.profile_id = $3) \
         and ($4::bigint is null or e.id < $4) order by e.id desc limit $5",
    )
    .bind(&managed)
    .bind(p.user_id)
    .bind(pid)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let actor = match r.actor_kind.as_str() {
                "token" => format!("{} (token)", r.token_label.as_deref().unwrap_or("deleted token")),
                "system" => "OwnHealth".into(),
                _ => r.user_name.clone().unwrap_or_else(|| "unknown".into()),
            };
            json!({
                "id": r.id,
                "ts": pydantic_datetime_utc(&r.ts),
                "actor": actor,
                "actor_kind": r.actor_kind,
                "via": r.via,
                "action": r.action,
                "tool": r.tool,
                "profile": r.profile_slug,
                "resource_type": r.resource_type,
                "resource_ids": r.resource_ids.unwrap_or_default(),
                "src_ids": r.src_ids.unwrap_or_default(),
                "args_summary": r.args_summary,
                "rows_out": r.rows_out,
                "bytes_out": r.bytes_out,
                "result_kind": r.result_kind,
                "withheld": r.withheld,
            })
        })
        .collect();
    let next = if items.len() as i64 == limit {
        items.last().map_or(Value::Null, |i| i["id"].clone())
    } else {
        Value::Null
    };
    ok(json!({"items": items, "next_before": next}))
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/audit", get(audit_log))
}
