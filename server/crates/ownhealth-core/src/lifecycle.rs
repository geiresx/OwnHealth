//! One lifecycle for every fact: provisional -> current -> superseded | retracted (`core/lifecycle.py`).
//!
//! Rows are immutable (a trigger enforces it). A correction inserts a new row with
//! `supersedes = old.id`; the old row only changes `status`, `status_reason` and `superseded_at`,
//! once. `superseded_at` is set for both superseded and retracted rows: it is when the row left the
//! current view.

use serde_json::{Map, Value};
use sqlx::AssertSqlSafe;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::pyjson::row_to_json;

pub const LIFECYCLE_TABLES: [&str; 9] = [
    "observation",
    "condition",
    "medication_statement",
    "allergy",
    "immunization",
    "procedure",
    "encounter",
    "note",
    "genome_finding",
];

/// The table name, once checked against `LIFECYCLE_TABLES` (it goes into SQL text).
fn checked(table: &str) -> Result<&'static str> {
    LIFECYCLE_TABLES
        .iter()
        .find(|t| **t == table)
        .copied()
        .ok_or_else(|| Error::Lifecycle(format!("{table} has no lifecycle")))
}

/// Lock a row that may still change (provisional or current) and return it.
pub async fn lock_current(conn: &mut PgConnection, table: &str, row_id: Uuid) -> Result<Map<String, Value>> {
    let t = checked(table)?;
    let row = sqlx::query(AssertSqlSafe(format!("select * from {t} where id = $1 for update")))
        .bind(row_id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| Error::Lifecycle(format!("{t} {row_id} not found")))?;
    let row = row_to_json(&row)?;
    let status = row
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if status != "provisional" && status != "current" {
        return Err(Error::Lifecycle(format!(
            "{t} {row_id} is {status}; only provisional or current rows change"
        )));
    }
    Ok(row)
}

pub async fn mark_superseded(conn: &mut PgConnection, table: &str, row_id: Uuid, reason: Option<&str>) -> Result<()> {
    let t = checked(table)?;
    lock_current(&mut *conn, t, row_id).await?;
    sqlx::query(AssertSqlSafe(format!(
        "update {t} set status = 'superseded', status_reason = $2, superseded_at = clock_timestamp() where id = $1"
    )))
    .bind(row_id)
    .bind(reason)
    .execute(conn)
    .await?;
    Ok(())
}

/// Withdraw a fact (entered in error, deleted by a person). Reason required; nothing is deleted.
pub async fn retract(conn: &mut PgConnection, table: &str, row_id: Uuid, reason: &str) -> Result<()> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(Error::Lifecycle("a retraction needs a reason".into()));
    }
    let t = checked(table)?;
    lock_current(&mut *conn, t, row_id).await?;
    sqlx::query(AssertSqlSafe(format!(
        "update {t} set status = 'retracted', status_reason = $2, superseded_at = clock_timestamp() where id = $1"
    )))
    .bind(row_id)
    .bind(reason)
    .execute(conn)
    .await?;
    Ok(())
}

/// A final report confirmed a preliminary value: provisional -> current.
pub async fn confirm_provisional(conn: &mut PgConnection, table: &str, row_id: Uuid) -> Result<()> {
    let t = checked(table)?;
    let row = lock_current(&mut *conn, t, row_id).await?;
    if row.get("status").and_then(Value::as_str) != Some("provisional") {
        return Err(Error::Lifecycle(format!("{t} {row_id} is not provisional")));
    }
    sqlx::query(AssertSqlSafe(format!(
        "update {t} set status = 'current' where id = $1"
    )))
    .bind(row_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Every version of a fact, oldest first, following `supersedes` both ways.
pub async fn history(conn: &mut PgConnection, table: &str, row_id: Uuid) -> Result<Vec<Map<String, Value>>> {
    let t = checked(table)?;
    let sql = format!(
        "with recursive back as (select t.*, 0 as chain_pos from {t} t where id = $1 \
         union all select t.*, b.chain_pos - 1 from {t} t join back b on t.id = b.supersedes), \
         fwd as (select t.*, 0 as chain_pos from {t} t where id = $1 \
         union all select t.*, f.chain_pos + 1 from {t} t join fwd f on t.supersedes = f.id) \
         select * from back union select * from fwd order by chain_pos"
    );
    let rows = sqlx::query(AssertSqlSafe(sql.as_str()))
        .bind(row_id)
        .fetch_all(conn)
        .await?;
    // Chain order, not recorded_at: a correction made in the same transaction has the same now().
    rows.iter()
        .map(|r| {
            let mut m = row_to_json(r)?;
            m.shift_remove("chain_pos");
            Ok(m)
        })
        .collect()
}

/// SQL predicate: rows visible as of `param` (a timestamptz bind such as `$2`). The current view
/// when the bind is null.
pub fn visible_sql(alias: &str, param: &str) -> String {
    let a = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    format!(
        "(case when cast({param} as timestamptz) is null then {a}status = 'current' \
         else {a}recorded_at <= cast({param} as timestamptz) and {a}status <> 'provisional' \
         and ({a}superseded_at is null or {a}superseded_at > cast({param} as timestamptz)) end)"
    )
}
