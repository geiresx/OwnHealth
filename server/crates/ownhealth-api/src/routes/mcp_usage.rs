//! Lane F: `api/mcp_usage.py`. `GET /tokens/{id}/usage`: what one MCP token used today and over the
//! last 30 days, against its limits (mcp.md 9.4). Only the token's owner sees it; another user's
//! token id answers 404. The SQL is the MCP lane's own (`ownhealth_mcp::limits`), repeated here so
//! the API crate does not depend on the MCP crate.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{NaiveDate, Utc};
use ownhealth_core::Runtime;
use ownhealth_core::access::PrincipalKind;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;

pub fn router() -> Router<Runtime> {
    Router::new().route("/tokens/{token_id}/usage", get(token_usage))
}

/// The day in the install's timezone (OWNHEALTH_TIMEZONE, default UTC); an unknown zone is UTC.
async fn today(conn: &mut PgConnection) -> ApiResult<NaiveDate> {
    let Some(tz) = std::env::var("OWNHEALTH_TIMEZONE").ok().filter(|z| !z.is_empty()) else {
        return Ok(Utc::now().date_naive());
    };
    let day: Option<NaiveDate> =
        sqlx::query_scalar("select (now() at time zone name)::date from pg_timezone_names where name = $1 limit 1")
            .bind(&tz)
            .fetch_optional(conn)
            .await?;
    Ok(day.unwrap_or_else(|| Utc::now().date_naive()))
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown", "No such token.")
}

async fn token_usage(State(rt): State<Runtime>, caller: Caller, Path(raw): Path<String>) -> Response {
    let Ok(token_id) = Uuid::parse_str(&raw) else {
        // FastAPI's UUID path parameter: 422 with the validation list
        let detail = json!([{"type": "uuid_parsing", "loc": ["path", "token_id"],
                             "msg": "Input should be a valid UUID", "input": raw}]);
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": detail}))).into_response();
    };
    match usage(&rt, caller, token_id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn usage(rt: &Runtime, caller: Caller, token_id: Uuid) -> ApiResult<Value> {
    let principal = caller.0;
    let mut conn = rt.pool.acquire().await?;
    type Limits = (Uuid, i32, i32, i64, i32, i32);
    let row: Option<Limits> = sqlx::query_as(
        "select id, rate_per_min, egress_facts_per_day, egress_bytes_per_day, genome_raw_lookups_per_day, \
         proposals_per_day from api_token where id = $1 and user_id = $2",
    )
    .bind(token_id)
    .bind(principal.user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((_, rate, facts, bytes, lookups, proposals)) = row else {
        return Err(not_found());
    };
    if principal.kind == PrincipalKind::Token {
        return Err(not_found());
    }
    let day = today(&mut conn).await?;
    type Used = (i64, i64, i64, i64, i64);
    let used: Used = sqlx::query_as(
        "select calls::bigint, facts::bigint, bytes::bigint, lookups::bigint, proposals::bigint from mcp_usage \
         where token_id = $1 and day = $2",
    )
    .bind(token_id)
    .bind(day)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or_default();
    type Hist = (NaiveDate, i64, i64, i64, i64, i64);
    let history: Vec<Hist> = sqlx::query_as(
        "select day, calls::bigint, facts::bigint, bytes::bigint, lookups::bigint, proposals::bigint from mcp_usage \
         where token_id = $1 order by day desc limit 30",
    )
    .bind(token_id)
    .fetch_all(&mut *conn)
    .await?;
    let history: Vec<Value> = history
        .into_iter()
        .map(|(d, calls, facts, bytes, lookups, proposals)| {
            json!({"day": d.format("%Y-%m-%d").to_string(), "calls": calls, "facts": facts, "bytes": bytes,
                   "lookups": lookups, "proposals": proposals})
        })
        .collect();
    Ok(json!({
        "token_id": token_id.to_string(),
        "day": day.format("%Y-%m-%d").to_string(),
        "today": {"calls": used.0, "facts": used.1, "bytes": used.2, "lookups": used.3, "proposals": used.4},
        "limits": {
            "rate_per_min": rate,
            "egress_facts_per_day": facts,
            "egress_bytes_per_day": bytes,
            "genome_raw_lookups_per_day": lookups,
            "proposals_per_day": proposals,
        },
        "history": history,
    }))
}
