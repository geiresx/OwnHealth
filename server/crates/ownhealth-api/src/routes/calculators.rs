//! v0.3 Insight: the calculators (`rules::calc`), computed on read and never stored.
//!
//! ```text
//! GET /api/profiles/{p}/calculators?as_of=&lang=    read:labs
//! ```
//!
//! The answer is the ownhealth.calculators/v1 document of the profile on `as_of`, with the words in
//! `lang`: eGFR by CKD-EPI 2021 (creatinine, and creatinine with cystatin C), FIB-4 and HOMA-IR, each
//! with its formula version, citation and inputs with their src ids, or the reason it was not
//! computed. SCORE2 and PREVENT also read conditions, medicines and vitals, so they need read:records
//! and read:vitals too; without them each is `not_assessed`, `scope_missing`, with no input line. The
//! `calculators_hash` is the same for every caller with the same scopes, in every language.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use chrono::NaiveDate;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Principal, today_utc};
use ownhealth_core::audit::write_audit;
use ownhealth_rules::calc;
use serde_json::Value;

use super::auth::{Check, R, ok};
use super::status::{event, parse_date};
use crate::extract::Caller;

pub async fn calculators(
    rt: &Runtime,
    principal: &Principal,
    p: &str,
    as_of: Option<NaiveDate>,
    lang: Option<&str>,
) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:labs"), None)
        .await?;
    let doc = calc::document_for(&mut tx, perm.profile.id, as_of.unwrap_or_else(today_utc), &perm.scopes).await?;
    let mut e = event(principal, perm.profile.id, "GET /api/profiles/{p}/calculators");
    e.resource_type = Some("calculators".into());
    e.resource_ids = Some(vec![doc["calculators_hash"].as_str().unwrap_or_default().to_string()]);
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    Ok(calc::render(&doc, lang.unwrap_or(&principal.lang))?)
}

async fn get_calculators(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let mut check = Check::query();
    let as_of = parse_date(&q, &mut check);
    check.done()?;
    let lang = q.get("lang").filter(|v| !v.is_empty()).map(String::as_str);
    ok(calculators(&rt, &c, &p, as_of, lang).await?)
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/calculators", get(get_calculators))
}
