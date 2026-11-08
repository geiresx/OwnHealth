//! Per-token limits (`mcp/limits.py`, mcp.md 9.4): a call bucket per minute, daily egress budgets in
//! facts and bytes, a daily cap on `lookup_variant` and on proposals. Counters live in Postgres
//! (migration 0500); the limits are columns of the token row, so a person can edit them per token.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::Result;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

pub const BURST: i64 = 20;
pub const MAX_PROPOSAL_ROWS: usize = 200;
pub const REGION_MAX_BP: i64 = 1_000_000;

/// The limit columns of `api_token`, in the order the usage route prints them.
pub const LIMIT_KEYS: [&str; 5] = [
    "rate_per_min",
    "egress_facts_per_day",
    "egress_bytes_per_day",
    "genome_raw_lookups_per_day",
    "proposals_per_day",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub rate_per_min: i64,
    pub egress_facts_per_day: i64,
    pub egress_bytes_per_day: i64,
    pub genome_raw_lookups_per_day: i64,
    pub proposals_per_day: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            rate_per_min: 60,
            egress_facts_per_day: 2000,
            egress_bytes_per_day: 2_000_000,
            genome_raw_lookups_per_day: 200,
            proposals_per_day: 50,
        }
    }
}

impl Limits {
    pub fn as_json(&self) -> Value {
        json!({
            "rate_per_min": self.rate_per_min,
            "egress_facts_per_day": self.egress_facts_per_day,
            "egress_bytes_per_day": self.egress_bytes_per_day,
            "genome_raw_lookups_per_day": self.genome_raw_lookups_per_day,
            "proposals_per_day": self.proposals_per_day,
        })
    }
}

/// The token row's limits (Python read them into `principal.extra["limits"]` at verify time);
/// defaults for a column that is null.
pub async fn limits_of(conn: &mut PgConnection, token_id: Option<Uuid>) -> Result<Limits> {
    let d = Limits::default();
    let Some(token_id) = token_id else {
        return Ok(d);
    };
    type Row = (Option<i32>, Option<i32>, Option<i64>, Option<i32>, Option<i32>);
    let row: Option<Row> = sqlx::query_as(
        "select rate_per_min, egress_facts_per_day, egress_bytes_per_day, genome_raw_lookups_per_day, \
         proposals_per_day from api_token where id = $1",
    )
    .bind(token_id)
    .fetch_optional(conn)
    .await?;
    let Some((rate, facts, bytes, lookups, proposals)) = row else {
        return Ok(d);
    };
    Ok(Limits {
        rate_per_min: rate.map_or(d.rate_per_min, i64::from),
        egress_facts_per_day: facts.map_or(d.egress_facts_per_day, i64::from),
        egress_bytes_per_day: bytes.unwrap_or(d.egress_bytes_per_day),
        genome_raw_lookups_per_day: lookups.map_or(d.genome_raw_lookups_per_day, i64::from),
        proposals_per_day: proposals.map_or(d.proposals_per_day, i64::from),
    })
}

/// `OWNHEALTH_TIMEZONE` (read from the environment, as Python did), when Postgres knows the zone.
fn zone() -> Option<&'static str> {
    static ZONE: OnceLock<Option<String>> = OnceLock::new();
    ZONE.get_or_init(|| std::env::var("OWNHEALTH_TIMEZONE").ok().filter(|z| !z.is_empty()))
        .as_deref()
}

/// The day in the install's timezone (OWNHEALTH_TIMEZONE, default UTC); budgets reset at 00:00.
/// Postgres holds the zone database, so the date comes from it; an unknown zone falls back to UTC.
pub async fn today(conn: &mut PgConnection) -> Result<NaiveDate> {
    // 0 unchecked, 1 known to Postgres, 2 unknown
    static CHECKED: AtomicU8 = AtomicU8::new(0);
    let Some(tz) = zone() else {
        return Ok(Utc::now().date_naive());
    };
    if CHECKED.load(Ordering::Relaxed) == 0 {
        let known: bool = sqlx::query_scalar("select exists (select 1 from pg_timezone_names where name = $1)")
            .bind(tz)
            .fetch_one(&mut *conn)
            .await?;
        CHECKED.store(if known { 1 } else { 2 }, Ordering::Relaxed);
    }
    if CHECKED.load(Ordering::Relaxed) != 1 {
        return Ok(Utc::now().date_naive());
    }
    Ok(sqlx::query_scalar("select (now() at time zone $1)::date")
        .bind(tz)
        .fetch_one(conn)
        .await?)
}

/// Python's `round(x, 1)`: correctly rounded from the binary value.
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// Take one call from the token's bucket. None when allowed; seconds to wait when not.
pub async fn take_call(
    conn: &mut PgConnection,
    token_id: Uuid,
    rate_per_min: i64,
    now: Option<DateTime<Utc>>,
) -> Result<Option<f64>> {
    let now = now.unwrap_or_else(Utc::now);
    let rate = rate_per_min.max(1) as f64 / 60.0;
    let burst = BURST.min(rate_per_min.max(1)) as f64;
    let row: Option<(f64, DateTime<Utc>)> =
        sqlx::query_as("select tokens, updated_at from mcp_rate where token_id = $1 for update")
            .bind(token_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((tokens, updated_at)) = row else {
        sqlx::query(
            "insert into mcp_rate (token_id, tokens, updated_at) values ($1, $2, $3) on conflict (token_id) do nothing",
        )
        .bind(token_id)
        .bind(burst - 1.0)
        .bind(now)
        .execute(conn)
        .await?;
        return Ok(None);
    };
    let elapsed = ((now - updated_at).num_microseconds().unwrap_or(0) as f64 / 1e6).max(0.0);
    let tokens = burst.min(tokens + elapsed * rate);
    let (left, wait) = if tokens < 1.0 {
        (tokens, Some(round1((1.0 - tokens) / rate)))
    } else {
        (tokens - 1.0, None)
    };
    sqlx::query("update mcp_rate set tokens = $2, updated_at = $3 where token_id = $1")
        .bind(token_id)
        .bind(left)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(wait)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub calls: i64,
    pub facts: i64,
    pub bytes: i64,
    pub lookups: i64,
    pub proposals: i64,
}

impl Usage {
    pub fn as_json(&self) -> Value {
        json!({"calls": self.calls, "facts": self.facts, "bytes": self.bytes, "lookups": self.lookups,
               "proposals": self.proposals})
    }
}

pub async fn usage(conn: &mut PgConnection, token_id: Uuid, day: NaiveDate) -> Result<Usage> {
    let row: Option<(i32, i32, i64, i32, i32)> = sqlx::query_as(
        "select calls, facts, bytes, lookups, proposals from mcp_usage where token_id = $1 and day = $2",
    )
    .bind(token_id)
    .bind(day)
    .fetch_optional(conn)
    .await?;
    Ok(
        row.map_or_else(Usage::default, |(calls, facts, bytes, lookups, proposals)| Usage {
            calls: calls.into(),
            facts: facts.into(),
            bytes,
            lookups: lookups.into(),
            proposals: proposals.into(),
        }),
    )
}

pub async fn add_usage(conn: &mut PgConnection, token_id: Uuid, day: NaiveDate, add: Usage) -> Result<()> {
    sqlx::query(
        "insert into mcp_usage (token_id, day, calls, facts, bytes, lookups, proposals) \
         values ($1, $2, $3, $4, $5, $6, $7) on conflict (token_id, day) do update set \
         calls = mcp_usage.calls + excluded.calls, facts = mcp_usage.facts + excluded.facts, \
         bytes = mcp_usage.bytes + excluded.bytes, lookups = mcp_usage.lookups + excluded.lookups, \
         proposals = mcp_usage.proposals + excluded.proposals",
    )
    .bind(token_id)
    .bind(day)
    .bind(i32::try_from(add.calls).unwrap_or(i32::MAX))
    .bind(i32::try_from(add.facts).unwrap_or(i32::MAX))
    .bind(add.bytes)
    .bind(i32::try_from(add.lookups).unwrap_or(i32::MAX))
    .bind(i32::try_from(add.proposals).unwrap_or(i32::MAX))
    .execute(conn)
    .await?;
    Ok(())
}
