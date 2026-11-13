//! Lane D: `api/genome.py`. The genome page, one variant lookup, and the two consent switches.
//!
//! ```text
//! GET  /api/profiles/{p}/genome?min_evidence=E3      read:genome
//! GET  /api/profiles/{p}/genome/variant?rsid=|hgvs=|region=&cursor=   read:genome_raw
//! POST /api/profiles/{p}/genome/consent {ui?, agents?}   owner (or guardian of a minor), step-up
//! ```
//!
//! A browser session is gated by the profile's UI switch, a token by the agents switch (mcp.md 9.7).
//! Every read writes one audit row with identifiers only, never a genotype.
//!
//! The REST extractor refuses tokens, so the handlers are thin: each calls a `pub` function taking
//! the principal, which is also how the tests reach the token paths Python reached by overriding
//! `get_principal`.

pub mod couple;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::NaiveDate;
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, PrincipalKind, majority, set_genome_switches};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::{Error, Result as CoreResult};
use ownhealth_genome::findings::visible_rows;
use ownhealth_genome::{EVIDENCE, FindingsQuery, Gate, NOTICE, VariantQuery, gate_for, withheld_reason};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use std::collections::HashMap;

use super::auth::{Body, Check, Fail, R, detail, ok};
use crate::extract::Caller;

const TILE_CATEGORIES: [&str; 6] = ["pgx", "carrier", "hla", "risk_marker", "trait", "prs"];
const STEP_UP_SECONDS: i64 = 300;

fn surface(p: &Principal) -> &'static str {
    if p.kind == PrincipalKind::Token { "agents" } else { "ui" }
}

fn today() -> NaiveDate {
    ownhealth_core::access::today()
}

async fn gate(_rt: &Runtime, conn: &mut PgConnection, perm: &Permission, p: &Principal) -> CoreResult<Gate> {
    let aom = majority(conn, perm.profile.household_id).await?;
    gate_for(perm, surface(p), today(), aom)
}

fn event(p: &Principal, perm: &Permission, action: &str, tool: &str) -> AuditEvent {
    let mut e = AuditEvent::new(
        p.actor_kind(),
        if p.kind == PrincipalKind::Token { "api" } else { "ui" },
        action,
    );
    e.actor_id = p.user_id;
    e.token_id = p.token_id;
    e.client = p.token_label.clone();
    e.tool = Some(tool.into());
    e.profile_id = Some(perm.profile.id);
    e
}

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(v)` for the JSON values a finding carries.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `a or b or ...`: the first truthy value, else the last.
fn first_truthy<'a>(vals: &[&'a Value]) -> &'a Value {
    vals.iter().copied().find(|v| truthy(v)).unwrap_or(vals[vals.len() - 1])
}

fn tiles(findings: &[Value], locked: &HashMap<String, i64>) -> Vec<Value> {
    let mut out = Vec::new();
    for cat in TILE_CATEGORIES {
        let rows: Vec<&Value> = findings.iter().filter(|f| f["category"] == cat).collect();
        let hidden = locked.get(cat).copied().unwrap_or(0);
        if rows.is_empty() && hidden == 0 {
            continue;
        }
        let actionable = rows
            .iter()
            .filter(|f| {
                let ph = &f["phenotype"];
                truthy(ph) && !py_str(ph).to_lowercase().starts_with("normal")
            })
            .count();
        // a gene, else the score id or rsID: never the title, whose words are English
        let mut seen: Vec<String> = Vec::new();
        for f in &rows {
            let value = &f["value"];
            let pgs = if truthy(value) { &value["pgs_id"] } else { &Value::Null };
            let key = py_str(first_truthy(&[&f["gene"], pgs, &f["rsid"], &f["title"]]));
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        let genes: String = seen.join(", ").chars().take(120).collect();
        let mut detail = genes.clone();
        if cat == "pgx" && actionable > 0 {
            detail = format!("{actionable} actionable · {genes}");
        }
        if hidden > 0 {
            detail = if detail.is_empty() {
                format!("{hidden} hidden")
            } else {
                format!("{detail} · {hidden} hidden")
            };
        }
        // The numbers go out too, so the UI words them in its own language.
        out.push(json!({
            "key": cat,
            "value": rows.len().to_string(),
            "detail": detail,
            "actionable": if cat == "pgx" { actionable } else { 0 },
            "genes": genes,
            "hidden": hidden,
        }));
    }
    out
}

/// `GET /profiles/{p}/genome` for one principal.
pub async fn overview(rt: &Runtime, principal: &Principal, p: &str, min_evidence: Option<&str>) -> R<Value> {
    let min_evidence = min_evidence.unwrap_or("E3");
    if !EVIDENCE.contains(&min_evidence) {
        return Err(detail(
            StatusCode::BAD_REQUEST,
            "bad_input",
            "min_evidence is one of ['E1', 'E2', 'E3', 'E4']",
        ));
    }
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:genome"), None)
        .await?;
    let gate = gate(rt, &mut tx, &perm, principal).await?;
    let q = FindingsQuery {
        min_evidence: Some(min_evidence),
        ..FindingsQuery::default()
    };
    let found = ownhealth_genome::list_findings_typed(&mut tx, perm.profile.id, &gate, &q).await?;
    let pgx_grid = pgx::grid(&mut tx, &perm, &found).await?;
    let mut locked_rows: Vec<Value> = Vec::new();
    if surface(principal) == "ui" && !gate.minor {
        for r in visible_rows(&mut tx, perm.profile.id, None).await? {
            if r.status == "current" && withheld_reason(&r, &gate) == Some("sensitive") {
                locked_rows.push(json!({"category": r.category, "gene": r.gene}));
            }
        }
    }
    let mut locked_by_cat: HashMap<String, i64> = HashMap::new();
    for r in &locked_rows {
        *locked_by_cat.entry(py_str(&r["category"])).or_insert(0) += 1;
    }
    let pr = &perm.profile;
    let can_consent =
        principal.kind != PrincipalKind::Token && (perm.role == "owner" || (perm.role == "guardian" && gate.minor));
    let withheld = json!({"sensitive": found.withheld_sensitive, "minor_rule": found.withheld_minor});
    let mut e = event(principal, &perm, "read", "GET /api/profiles/{p}/genome");
    e.resource_type = Some("genome_finding".into());
    e.resource_ids = Some(found.findings.iter().map(|f| py_str(&f["id"])).collect());
    e.src_ids = Some(found.findings.iter().map(|f| py_str(&f["src"])).collect());
    e.rows_out = i32::try_from(found.findings.len()).ok();
    e.withheld = Some(withheld.clone());
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    let dataset = match &found.assay {
        None => Value::Null,
        Some(a) => {
            let f = |k: &str| a.get(k).cloned().unwrap_or(Value::Null);
            json!({
                "assay": f("type"),
                "platform": f("platform"),
                "mean_depth": f("mean_depth"),
                "build": f("build"),
                "source_tool": f("source_tool"),
                "generated_utc": f("generated_utc"),
                "imported_at": f("imported_at"),
                "db_versions": found.db_versions,
                "raw_files": f("raw_files"),
                "src": f("src"),
            })
        }
    };
    Ok(json!({
        "dataset": dataset,
        "tiles": tiles(&found.findings, &locked_by_cat),
        "pgx_grid": pgx_grid,
        "findings": found.findings,
        "coverage": found.coverage,
        "not_assessed": found.not_assessed,
        "withheld": withheld,
        "locked": locked_rows,
        "consent": {"ui": pr.genome_sensitive_ui, "agents": pr.genome_sensitive_agents, "can_change": can_consent},
        "is_minor": gate.minor,
        "limitations": found.limitations,
        "limitation_codes": found.limitation_codes,
        "notice": NOTICE,
        "notice_code": "notice",
    }))
}

/// The query of `/genome/variant`; empty strings stay as given (Python's `rsid or hgvs or ...`).
#[derive(Debug, Clone, Default)]
pub struct VariantParams {
    pub rsid: Option<String>,
    pub hgvs: Option<String>,
    pub region: Option<String>,
    pub cursor: Option<String>,
}

/// `GET /profiles/{p}/genome/variant` for one principal: the status and the JSON body. A query the
/// lookup cannot answer is a 404 (`no_genome`) or 400 answer, after its audit row commits.
pub async fn variant(rt: &Runtime, principal: &Principal, p: &str, params: &VariantParams) -> R<(StatusCode, Value)> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:genome_raw"), None)
        .await?;
    let gate = gate(rt, &mut tx, &perm, principal).await?;
    let query: String = [&params.rsid, &params.hgvs, &params.region]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .map(|s| s.chars().take(120).collect())
        .unwrap_or_default();
    let q = VariantQuery {
        rsid: params.rsid.as_deref(),
        hgvs: params.hgvs.as_deref(),
        region: params.region.as_deref(),
        cursor: params.cursor.as_deref(),
    };
    let tool = "GET /api/profiles/{p}/genome/variant";
    let mut e = event(principal, &perm, "lookup", tool);
    e.resource_type = Some("genome_position".into());
    e.args_summary = Some(query);
    let answer = match ownhealth_genome::lookup_variant(&mut tx, &rt.blobs, perm.profile.id, &gate, &q).await? {
        Err(exc) => {
            e.result_kind = Some(exc.code.into());
            write_audit(&mut tx, &e).await?;
            tx.commit().await?;
            let status = if exc.code == "no_genome" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            return Ok((status, json!({"detail": exc.as_value()})));
        }
        Ok(out) => out,
    };
    let get = |k: &str| answer.get(k).filter(|v| truthy(v));
    e.resource_ids = Some(
        get("cited_by_findings")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(py_str).collect())
            .unwrap_or_default(),
    );
    e.result_kind = Some(get("withheld").map_or_else(|| "ok".into(), py_str));
    let rows = get("calls")
        .or_else(|| get("records"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    e.rows_out = i32::try_from(rows).ok();
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    let mut out = match answer {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
    };
    // one code per limitation sentence, so the UI says it in the reader's language
    let codes: Vec<&str> = out
        .get("limitations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|l| ownhealth_genome::lookup::limitation_code(l.as_str().unwrap_or_default()))
        .collect();
    out.insert("limitation_codes".into(), json!(codes));
    out.insert("notice".into(), json!(NOTICE));
    Ok((StatusCode::OK, Value::Object(out)))
}

/// Pydantic's lax `bool | None`: true/false, 0/1, and its words ("yes", "off", ...).
fn lax_bool(check: &mut Check, body: &Map<String, Value>, field: &str) -> Option<bool> {
    let v = body.get(field)?;
    let parsed = match v {
        Value::Null => return None,
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => match n.as_f64() {
            Some(0.0) => Ok(false),
            Some(1.0) => Ok(true),
            _ => Err("bool_parsing"),
        },
        Value::String(s) => match s.to_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            _ => Err("bool_parsing"),
        },
        _ => Err("bool_type"),
    };
    match parsed {
        Ok(b) => Some(b),
        Err(kind) => {
            let msg = if kind == "bool_type" {
                "Input should be a valid boolean"
            } else {
                "Input should be a valid boolean, unable to interpret input"
            };
            check.push(kind, field, msg, v.clone());
            None
        }
    }
}

fn py_opt_bool(b: Option<bool>) -> &'static str {
    match b {
        None => "None",
        Some(true) => "True",
        Some(false) => "False",
    }
}

/// `POST /profiles/{p}/genome/consent` for one principal. Only the owner, or a guardian of a minor
/// (whose sensitive findings stay withheld by the minor rule anyway); never a token; needs a fresh
/// step-up.
pub async fn consent(rt: &Runtime, principal: &Principal, p: &str, ui: Option<bool>, agents: Option<bool>) -> R<Value> {
    if principal.kind == PrincipalKind::Token {
        return Err(Error::access_denied("forbidden", "Consent changes only in the app, never with a token.").into());
    }
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone()).require(&mut tx, p, None, None).await?;
    let aom = majority(&mut tx, perm.profile.household_id).await?;
    let minor = perm.profile.is_minor(today(), aom);
    if !(perm.role == "owner" || (perm.role == "guardian" && minor)) {
        return Err(Error::access_denied("forbidden", "Only the profile's owner changes its genome consent.").into());
    }
    sessions::require_step_up(&mut tx, principal, STEP_UP_SECONDS).await?;
    set_genome_switches(&mut tx, perm.profile.id, ui, agents).await?;
    let mut e = event(
        principal,
        &perm,
        "consent_change",
        "POST /api/profiles/{p}/genome/consent",
    );
    e.resource_type = Some("profile".into());
    e.resource_ids = Some(vec![perm.profile.id.to_string()]);
    e.args_summary = Some(format!("ui={} agents={}", py_opt_bool(ui), py_opt_bool(agents)));
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    Ok(json!({
        "ui": ui.unwrap_or(perm.profile.genome_sensitive_ui),
        "agents": agents.unwrap_or(perm.profile.genome_sensitive_agents),
    }))
}

type Params = Query<HashMap<String, String>>;

async fn genome_overview(State(rt): State<Runtime>, Caller(c): Caller, Path(p): Path<String>, Query(q): Params) -> R {
    ok(overview(&rt, &c, &p, q.get("min_evidence").map(String::as_str)).await?)
}

async fn genome_variant(State(rt): State<Runtime>, Caller(c): Caller, Path(p): Path<String>, Query(q): Params) -> R {
    let params = VariantParams {
        rsid: q.get("rsid").cloned(),
        hgvs: q.get("hgvs").cloned(),
        region: q.get("region").cloned(),
        cursor: q.get("cursor").cloned(),
    };
    let (status, body) = variant(&rt, &c, &p, &params).await?;
    Ok((status, axum::Json(body)).into_response())
}

async fn genome_consent(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path(p): Path<String>,
    Body(body): Body<Map<String, Value>>,
) -> Result<Response, Fail> {
    let mut check = Check::body();
    let ui = lax_bool(&mut check, &body, "ui");
    let agents = lax_bool(&mut check, &body, "agents");
    check.done()?;
    ok(consent(&rt, &c, &p, ui, agents).await?)
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/genome", get(genome_overview))
        .route("/profiles/{p}/genome/variant", get(genome_variant))
        .route("/profiles/{p}/genome/consent", post(genome_consent))
}

/// The ClinVar diff's history, a submodule router (merged in routes.rs).
pub mod events;
/// The PGx gene grid of the overview.
pub mod pgx;
