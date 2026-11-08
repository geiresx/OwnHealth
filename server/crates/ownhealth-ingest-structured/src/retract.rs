//! Withdraw everything one document brought in, for a retraction or a move to another profile
//! (`ingest/retract.py`).
//!
//! A document reaches the current view three ways: observations carry `document_id`; records,
//! encounters, notes and genome findings link through their `src.document_id`; wearable samples carry
//! `document_id` (0200); child documents (an export's ECGs) carry `flags.parent_document` and are
//! retracted with it. The facts are retracted (kept, out of the current view); samples have no
//! lifecycle, so they are deleted and the rollups they fed rebuilt. The document's import flags are
//! cleared, so importing it again runs the import instead of returning the old summary.
//!
//! Samples another still-active export also holds belong to whichever export brought them first;
//! they leave with that one.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;
use ownhealth_core::Result;
use ownhealth_core::lifecycle::{self, LIFECYCLE_TABLES};
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::wearables;

/// Returns `{"observations": [ids], "records": {table: [ids]}, "wearable_samples": n, "documents": [ids]}`.
/// `documents` are the child documents the import created (an Apple Health export's ECGs), retracted
/// with it.
pub fn retract_document_facts<'c>(
    conn: &'c mut PgConnection,
    document_id: Uuid,
    reason: &'c str,
) -> BoxFuture<'c, Result<Value>> {
    Box::pin(async move {
        let mut observations: Vec<Value> = Vec::new();
        let mut records = Map::new();
        let children: Vec<Uuid> = sqlx::query_scalar(
            "update document set status = 'retracted', status_reason = $2 where flags->>'parent_document' = $1 \
             and status = 'active' returning id",
        )
        .bind(document_id.to_string())
        .bind(reason)
        .fetch_all(&mut *conn)
        .await?;
        for child in &children {
            retract_document_facts(&mut *conn, *child, reason).await?;
        }
        let mut tables: Vec<&str> = LIFECYCLE_TABLES.to_vec();
        tables.sort_unstable();
        for table in tables {
            let direct = if table == "observation" {
                " or f.document_id = $1"
            } else {
                ""
            };
            let sql = format!(
                "select f.id from {table} f left join src s on s.id = f.src_id \
                 where (s.document_id = $1{direct}) and f.status in ('current', 'provisional') order by f.id"
            );
            let ids: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(sql.as_str()))
                .bind(document_id)
                .fetch_all(&mut *conn)
                .await?;
            for id in &ids {
                lifecycle::retract(&mut *conn, table, *id, reason).await?;
            }
            let ids: Vec<Value> = ids.iter().map(|i| json!(i.to_string())).collect();
            if table == "observation" {
                observations = ids;
            } else if !ids.is_empty() {
                records.insert(table.to_string(), Value::Array(ids));
            }
        }
        let gone: Vec<(Uuid, String, i16)> = sqlx::query_as(
            "delete from wearable_sample where document_id = $1 returning profile_id, metric, source_id",
        )
        .bind(document_id)
        .fetch_all(&mut *conn)
        .await?;
        let mut touched: BTreeMap<Uuid, (BTreeSet<String>, BTreeSet<i16>)> = BTreeMap::new();
        for (p, metric, source) in &gone {
            let e = touched.entry(*p).or_default();
            e.0.insert(metric.clone());
            e.1.insert(*source);
        }
        for (profile_id, (metrics, sources)) in touched {
            let metrics: Vec<String> = metrics.into_iter().collect();
            let sources: Vec<i16> = sources.into_iter().collect();
            wearables::recompute_rollups(&mut *conn, profile_id, Some(&metrics), Some(&sources)).await?;
        }
        sqlx::query("update document set flags = flags - 'import' - 'apple_health' - 'fhir_issuer' where id = $1")
            .bind(document_id)
            .execute(&mut *conn)
            .await?;
        Ok(json!({
            "observations": observations,
            "records": records,
            "wearable_samples": gone.len(),
            "documents": children.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
        }))
    })
}
