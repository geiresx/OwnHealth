//! Append-only audit log writer (`core/audit.py`). Identifiers only: never values, document text or
//! free-text arguments.
//!
//! Write the event in the same transaction as the read it records. Free-text arguments go through
//! `hash_arg` so the log never says what was searched.

use std::net::IpAddr;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::Result;

/// One audit row. `actor_kind`: user | token | share | system; `via`: ui | api | mcp | job | share |
/// export; `action`: read | search | propose | export | grant_change | login | purge | review | import ...
#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub actor_kind: String,
    pub actor_id: Option<Uuid>,
    pub via: String,
    pub action: String,
    pub token_id: Option<Uuid>,
    pub client: Option<String>,
    pub tool: Option<String>,
    pub profile_id: Option<Uuid>,
    pub resource_type: Option<String>,
    pub resource_ids: Option<Vec<String>>,
    pub src_ids: Option<Vec<String>>,
    pub args_summary: Option<String>,
    pub result_kind: Option<String>,
    pub rows_out: Option<i32>,
    pub bytes_out: Option<i32>,
    pub withheld: Option<Value>,
    pub protocol_version: Option<String>,
    /// Kept only when it parses as an address: the column is inet, and a bad cast would abort the read.
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl AuditEvent {
    /// An event with `result_kind = "ok"` and nothing else set, as the pydantic model defaults.
    pub fn new(actor_kind: &str, via: &str, action: &str) -> AuditEvent {
        AuditEvent {
            actor_kind: actor_kind.into(),
            actor_id: None,
            via: via.into(),
            action: action.into(),
            token_id: None,
            client: None,
            tool: None,
            profile_id: None,
            resource_type: None,
            resource_ids: None,
            src_ids: None,
            args_summary: None,
            result_kind: Some("ok".into()),
            rows_out: None,
            bytes_out: None,
            withheld: None,
            protocol_version: None,
            ip: None,
            user_agent: None,
        }
    }
}

/// Python's `ipaddress.ip_address(str(v).strip())`, or None.
pub fn parse_ip(raw: Option<&str>) -> Option<IpAddr> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

/// Stable short hash for a free-text argument, so the log keeps no search terms.
pub fn hash_arg(value: &str) -> String {
    format!("h:{}", &hex::encode(Sha256::digest(value.as_bytes()))[..12])
}

pub async fn write_audit(conn: &mut PgConnection, e: &AuditEvent) -> Result<i64> {
    let ip = parse_ip(e.ip.as_deref()).map(|a| a.to_string());
    Ok(sqlx::query_scalar(
        "insert into audit_event (actor_kind, actor_id, token_id, client, via, action, tool, profile_id, \
         resource_type, resource_ids, src_ids, args_summary, result_kind, rows_out, bytes_out, withheld, \
         protocol_version, ip, user_agent) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
         $15, $16, $17, cast($18 as inet), $19) returning id",
    )
    .bind(&e.actor_kind)
    .bind(e.actor_id)
    .bind(e.token_id)
    .bind(&e.client)
    .bind(&e.via)
    .bind(&e.action)
    .bind(&e.tool)
    .bind(e.profile_id)
    .bind(&e.resource_type)
    .bind(&e.resource_ids)
    .bind(&e.src_ids)
    .bind(&e.args_summary)
    .bind(&e.result_kind)
    .bind(e.rows_out)
    .bind(e.bytes_out)
    .bind(&e.withheld)
    .bind(&e.protocol_version)
    .bind(ip)
    .bind(&e.user_agent)
    .fetch_one(conn)
    .await?)
}

/// Newest first. Filter by the profiles the caller may see, or by actor.
pub async fn list_audit(
    conn: &mut PgConnection,
    profile_ids: Option<&[Uuid]>,
    actor_id: Option<Uuid>,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<Map<String, Value>>> {
    let rows = sqlx::query(
        "select * from audit_event where ($1::uuid[] is null or profile_id = any($1)) \
         and ($2::uuid is null or actor_id = $2) and ($3::bigint is null or id < $3) order by id desc limit $4",
    )
    .bind(profile_ids)
    .bind(actor_id)
    .bind(before_id)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.iter().map(crate::pyjson::row_to_json).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_arg_hides_search_terms() {
        let h = hash_arg("HIV test");
        assert!(!h.contains("HIV") && h == hash_arg("HIV test") && h.len() == 14);
        // the Python value: "h:" + sha256("HIV test").hexdigest()[:12]
        assert_eq!(h, format!("h:{}", &hex::encode(Sha256::digest(b"HIV test"))[..12]));
    }

    #[test]
    fn only_real_addresses_are_kept() {
        assert_eq!(parse_ip(Some("testclient")), None);
        assert_eq!(parse_ip(Some("1.2.3.4, 5.6.7.8")), None);
        assert_eq!(
            parse_ip(Some(" 10.0.0.1 ")).map(|a| a.to_string()),
            Some("10.0.0.1".into())
        );
        assert_eq!(parse_ip(Some("::1")).map(|a| a.to_string()), Some("::1".into()));
    }
}
