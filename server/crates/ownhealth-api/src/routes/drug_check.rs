//! Lane E: `api/drug_check.py`. Check one drug for one profile, the same data block as the MCP
//! `check_drug` tool.
//!
//! ```text
//! GET /api/profiles/{p}/drug-check?drug=&lang=   read:genome, read:records or read:labs
//! ```
//!
//! The audit row holds the drug's id and a hash of what was typed, never the typed name.

use std::collections::{BTreeSet, HashMap};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use ownhealth_core::access::Principal;
use ownhealth_core::audit::{hash_arg, write_audit};
use ownhealth_core::{Error, Runtime};
use serde_json::{Value, json};

use super::auth::{Check, R, detail, ok};
use super::status::event;
use crate::extract::Caller;

const SCOPES: [&str; 3] = ["read:genome", "read:records", "read:labs"];

pub async fn drug_check(rt: &Runtime, principal: &Principal, p: &str, drug: &str, lang: Option<&str>) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone()).require(&mut tx, p, None, None).await?;
    if !SCOPES.iter().any(|s| perm.has(s)) {
        return Err(detail(
            StatusCode::FORBIDDEN,
            "forbidden",
            "This needs read:genome, read:records or read:labs.",
        ));
    }
    let lang = lang.filter(|l| !l.is_empty()).unwrap_or(&principal.lang);
    let out = match ownhealth_rules::check_drug(&mut tx, perm.profile.id, drug, &perm.scopes, lang, None).await {
        Err(Error::NotFound(_)) => {
            return Err(detail(
                StatusCode::NOT_FOUND,
                "drug_unknown",
                "This drug name is not in the drug table.",
            ));
        }
        other => other?,
    };
    let srcs: BTreeSet<String> = ["pgx", "current_medications_relevant", "relevant_labs"]
        .iter()
        .flat_map(|part| out[*part].as_array().into_iter().flatten())
        .filter_map(|x| x["src"].as_str().filter(|s| !s.is_empty()).map(str::to_string))
        .collect();
    let id = [&out["drug"]["cpic_drug_id"], &out["drug"]["name"]]
        .into_iter()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
        .unwrap_or_default();
    let mut e = event(principal, perm.profile.id, "GET /api/profiles/{p}/drug-check");
    e.resource_type = Some("drug_check".into());
    e.resource_ids = Some(vec![id.to_string()]);
    e.src_ids = Some(srcs.into_iter().collect());
    e.args_summary = Some(hash_arg(drug));
    e.withheld = Some(json!({"by_scope": out["withheld"]["by_scope"]}));
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    Ok(out)
}

/// `drug: str = Query(..., min_length=1, max_length=80)`: a missing or bad drug answers 422.
fn drug_param(q: &HashMap<String, String>) -> R<String> {
    let mut check = Check::query();
    let drug = q.get("drug");
    if drug.is_none() {
        check.push("missing", "drug", "Field required", Value::Null);
    }
    check.len("drug", drug.map(String::as_str), 1, 80).done()?;
    Ok(drug.cloned().unwrap_or_default())
}

async fn get_drug_check(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let drug = drug_param(&q)?;
    ok(drug_check(&rt, &c, &p, &drug, q.get("lang").map(String::as_str)).await?)
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/drug-check", get(get_drug_check))
}
