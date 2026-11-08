//! Lane I: `api/timeline.py`. One dated list of what happened, built from the facts already in the
//! store.
//!
//! ```text
//! GET /api/profiles/{p}/timeline   ?types=lab,visit&from=YYYY-MM-DD&to=YYYY-MM-DD&lang=en
//! ```
//!
//! Lab draws (one event per report, not per value), medication starts and stops, diagnoses, vaccines,
//! visits, imaging studies and genome imports. Only current facts; sensitive rows only with
//! `read:sensitive`; each event type only with the scope that guards its facts. Every event cites a
//! `src`.

use std::collections::BTreeSet;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::NaiveDate;
use ownhealth_core::Runtime;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::pyjson::py_date;
use serde_json::{Value, json};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgConnection, Row};
use uuid::Uuid;

use crate::Caller;
use crate::error::ApiError;
use crate::routes::auth::{Fail, R};
use crate::validate::{Errors, Params, param};

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/timeline", get(timeline))
}

const TYPES: [&str; 7] = [
    "lab",
    "visit",
    "imaging",
    "diagnosis",
    "medication",
    "vaccine",
    "genome",
];

fn scope(t: &str) -> &'static str {
    match t {
        "lab" => "read:labs",
        "genome" => "read:genome",
        _ => "read:records",
    }
}

/// The words of one language.
struct Words {
    draw: &'static str,
    draw1: &'static str,
    flagged: &'static str,
    started: &'static str,
    stopped: &'static str,
    genome: &'static str,
    visit: &'static str,
}

const EN: Words = Words {
    draw: "Lab report · {n} results",
    draw1: "Lab report · 1 result",
    flagged: "{n} flagged by the lab",
    started: "{name} started",
    stopped: "{name} stopped",
    genome: "Genome facts imported",
    visit: "Visit",
};

const ES: Words = Words {
    draw: "Analítica · {n} resultados",
    draw1: "Analítica · 1 resultado",
    flagged: "{n} marcados por el laboratorio",
    started: "Inicio de {name}",
    stopped: "Fin de {name}",
    genome: "Datos genómicos importados",
    visit: "Consulta",
};

/// What is true now on a clinical table aliased `a`: current, not superseded, sensitive only if allowed.
fn live(a: &str, include_sensitive: bool) -> String {
    let sql = format!("{a}.status = 'current' and {a}.superseded_at is null");
    if include_sensitive {
        sql
    } else {
        format!("{sql} and cardinality({a}.sensitivity) = 0")
    }
}

/// `" · ".join(x for x in parts if x)`.
fn joined(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .filter_map(|p| p.as_deref().filter(|s| !s.is_empty()))
        .collect::<Vec<_>>()
        .join(" · ")
}

struct Event {
    date: String,
    kind: &'static str,
    title: String,
    json: Value,
}

fn event(
    day: Option<NaiveDate>,
    kind: &'static str,
    title: String,
    detail: Option<String>,
    src: Option<String>,
    label: Option<String>,
    provider: Option<String>,
) -> Option<Event> {
    let date = py_date(&day?);
    let json = json!({
        "date": date,
        "type": kind,
        "title": title,
        "detail": detail.filter(|d| !d.is_empty()),
        "country": null, // not recorded per fact in v0.1
        "flags": [],
        "src": src,
        "src_label": label,
        "provider": provider,
    });
    Some(Event {
        date,
        kind,
        title,
        json,
    })
}

fn text(r: &PgRow, col: &str) -> R<Option<String>> {
    Ok(r.try_get::<Option<String>, _>(col)?)
}

fn day(r: &PgRow, col: &str) -> R<Option<NaiveDate>> {
    Ok(r.try_get::<Option<NaiveDate>, _>(col)?)
}

async fn rows(conn: &mut PgConnection, sql: &str, pid: Uuid) -> R<Vec<PgRow>> {
    Ok(sqlx::query(AssertSqlSafe(sql)).bind(pid).fetch_all(conn).await?)
}

async fn labs(conn: &mut PgConnection, pid: Uuid, sens: bool, t: &Words) -> R<Vec<Option<Event>>> {
    // One event per draw: report_key when the importer set it, else the document, else the day.
    let sql = format!(
        "select min(o.effective_at)::date as day, count(*) as n, \
         count(*) filter (where o.interpretation in ('L', 'H', 'LL', 'HH', 'A')) as flagged, \
         min(d.provider_name) as provider, min(o.src_id) as src, min(s.label) as label \
         from observation o left join document d on d.id = o.document_id join src s on s.id = o.src_id \
         where o.profile_id = $1 and o.category = 'laboratory' and {} \
         group by coalesce(o.report_key, o.document_id::text, o.effective_at::date::text)",
        live("o", sens)
    );
    let mut out = Vec::new();
    for r in rows(conn, &sql, pid).await? {
        let n: i64 = r.try_get("n")?;
        let flagged: i64 = r.try_get("flagged")?;
        let provider = text(&r, "provider")?;
        let flagged_text = (flagged != 0).then(|| t.flagged.replace("{n}", &flagged.to_string()));
        let detail = joined(&[provider.clone(), flagged_text]);
        let title = if n == 1 {
            t.draw1.to_string()
        } else {
            t.draw.replace("{n}", &n.to_string())
        };
        out.push(event(
            day(&r, "day")?,
            "lab",
            title,
            Some(detail),
            text(&r, "src")?,
            text(&r, "label")?,
            provider,
        ));
    }
    Ok(out)
}

async fn records(
    conn: &mut PgConnection,
    pid: Uuid,
    sens: bool,
    t: &Words,
    want: &BTreeSet<&str>,
) -> R<Vec<Option<Event>>> {
    let mut out = Vec::new();
    if want.contains("medication") {
        let sql = format!(
            "select m.name, m.strength, m.started, m.stopped, m.reason_text, m.dose_text, m.src_id, s.label \
             from medication_statement m join src s on s.id = m.src_id where m.profile_id = $1 and {}",
            live("m", sens)
        );
        for r in rows(conn, &sql, pid).await? {
            let name = [text(&r, "name")?, text(&r, "strength")?]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let (src, label) = (text(&r, "src_id")?, text(&r, "label")?);
            out.push(event(
                day(&r, "started")?,
                "medication",
                t.started.replace("{name}", &name),
                text(&r, "dose_text")?,
                src.clone(),
                label.clone(),
                None,
            ));
            out.push(event(
                day(&r, "stopped")?,
                "medication",
                t.stopped.replace("{name}", &name),
                text(&r, "reason_text")?,
                src,
                label,
                None,
            ));
        }
    }
    if want.contains("diagnosis") {
        let sql = format!(
            "select c.text, c.code_system, c.code, coalesce(c.onset, c.effective_at::date) as day, \
             c.verification_status, c.src_id, s.label from condition c join src s on s.id = c.src_id \
             where c.profile_id = $1 and {}",
            live("c", sens)
        );
        for r in rows(conn, &sql, pid).await? {
            let code = match (text(&r, "code_system")?, text(&r, "code")?) {
                (Some(sys), Some(code)) if !sys.is_empty() && !code.is_empty() => {
                    Some(format!("{} {code}", sys.to_uppercase()))
                }
                _ => None,
            };
            let detail = joined(&[code, text(&r, "verification_status")?]);
            out.push(event(
                day(&r, "day")?,
                "diagnosis",
                text(&r, "text")?.unwrap_or_default(),
                Some(detail),
                text(&r, "src_id")?,
                text(&r, "label")?,
                None,
            ));
        }
    }
    if want.contains("vaccine") {
        let sql = format!(
            "select i.vaccine, coalesce(i.occurred, i.effective_at::date) as day, i.dose_number, i.lot, \
             i.src_id, s.label from immunization i join src s on s.id = i.src_id where i.profile_id = $1 and {}",
            live("i", sens)
        );
        for r in rows(conn, &sql, pid).await? {
            let dose: Option<i32> = r.try_get("dose_number")?;
            let dose = dose.filter(|d| *d != 0).map(|d| format!("#{d}"));
            let detail = joined(&[dose, text(&r, "lot")?]);
            out.push(event(
                day(&r, "day")?,
                "vaccine",
                text(&r, "vaccine")?.unwrap_or_default(),
                Some(detail),
                text(&r, "src_id")?,
                text(&r, "label")?,
                None,
            ));
        }
    }
    if want.contains("visit") {
        let sql = format!(
            "select e.class, e.provider_name, e.reason_text, coalesce(e.period_start, e.effective_at)::date as day, \
             e.src_id, s.label from encounter e join src s on s.id = e.src_id where e.profile_id = $1 and {}",
            live("e", sens)
        );
        for r in rows(conn, &sql, pid).await? {
            let title = joined(&[Some(t.visit.to_string()), text(&r, "reason_text")?]);
            let provider = text(&r, "provider_name")?;
            out.push(event(
                day(&r, "day")?,
                "visit",
                title,
                provider.clone(),
                text(&r, "src_id")?,
                text(&r, "label")?,
                provider,
            ));
        }
    }
    if want.contains("imaging") {
        let sql = "select i.modality, i.description, i.study_date, i.src_id, s.label \
                   from imaging_study i join src s on s.id = i.src_id where i.profile_id = $1 and i.status = 'current'";
        for r in rows(conn, sql, pid).await? {
            let modality = text(&r, "modality")?.filter(|m| !m.is_empty());
            let title = text(&r, "description")?
                .filter(|d| !d.is_empty())
                .or_else(|| modality.clone())
                .unwrap_or_else(|| "Imaging study".into());
            out.push(event(
                day(&r, "study_date")?,
                "imaging",
                title,
                modality,
                text(&r, "src_id")?,
                text(&r, "label")?,
                None,
            ));
        }
    }
    Ok(out)
}

async fn genome(conn: &mut PgConnection, pid: Uuid, t: &Words) -> R<Vec<Option<Event>>> {
    let sql = "select g.imported_at::date as day, g.assay, g.build, g.owngenome_version, g.src_id, s.label \
               from genome_dataset g join src s on s.id = g.src_id where g.profile_id = $1 and g.status = 'current'";
    let mut out = Vec::new();
    for r in rows(conn, sql, pid).await? {
        let version = text(&r, "owngenome_version")?.unwrap_or_else(|| "None".into());
        let assay = text(&r, "assay")?.unwrap_or_default().to_uppercase();
        let build = text(&r, "build")?.unwrap_or_else(|| "None".into());
        out.push(event(
            day(&r, "day")?,
            "genome",
            t.genome.to_string(),
            Some(format!("OwnGenome {version} · {assay} · {build}")),
            text(&r, "src_id")?,
            text(&r, "label")?,
            Some("OwnGenome".into()),
        ));
    }
    Ok(out)
}

/// Python's `repr(sorted(set_of_str))`: `['a', 'b']`.
fn py_list(items: &BTreeSet<&str>) -> String {
    let parts: Vec<String> = items.iter().map(|s| ownhealth_normalize::text::py_repr(s)).collect();
    format!("[{}]", parts.join(", "))
}

async fn timeline(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    Query(q): Query<Params>,
) -> R<Json<Value>> {
    let principal = &caller.0;
    let mut e = Errors::new();
    let from = e.date(&q, "from");
    let to = e.date(&q, "to");
    e.done()?;
    let lang = param(&q, "lang").unwrap_or("en");
    let types = param(&q, "types").filter(|t| !t.is_empty());

    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, None).await?;
    let asked: BTreeSet<&str> = match types {
        None => TYPES.into_iter().collect(),
        Some(t) => t.split(',').map(str::trim).filter(|x| !x.is_empty()).collect(),
    };
    let unknown: BTreeSet<&str> = asked.iter().copied().filter(|t| !TYPES.contains(t)).collect();
    if !unknown.is_empty() {
        return Err(Fail::Api(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_input",
            format!("Unknown types: {}", py_list(&unknown)),
        )));
    }
    let want: BTreeSet<&str> = asked.iter().copied().filter(|k| perm.has(scope(k))).collect();
    let withheld: Vec<&str> = asked.difference(&want).copied().collect();
    let t = if lang == "es" { &ES } else { &EN };
    let sens = perm.has("read:sensitive");
    let pid = perm.profile.id;

    let mut events: Vec<Option<Event>> = Vec::new();
    if want.contains("lab") {
        events.extend(labs(&mut tx, pid, sens, t).await?);
    }
    events.extend(records(&mut tx, pid, sens, t, &want).await?);
    if want.contains("genome") {
        events.extend(genome(&mut tx, pid, t).await?);
    }
    let from = from.map(|d| py_date(&d));
    let to = to.map(|d| py_date(&d));
    let mut items: Vec<Event> = events
        .into_iter()
        .flatten()
        .filter(|e| from.as_ref().is_none_or(|f| &e.date >= f) && to.as_ref().is_none_or(|t| &e.date <= t))
        .collect();
    // Python's sort(reverse=True) is stable: equal keys keep their order
    items.sort_by(|a, b| (&b.date, b.kind, &b.title).cmp(&(&a.date, a.kind, &a.title)));

    let mut a = AuditEvent::new(principal.actor_kind(), "ui", "read");
    a.actor_id = principal.user_id;
    a.token_id = principal.token_id;
    a.tool = Some("GET /api/profiles/{p}/timeline".into());
    a.profile_id = Some(pid);
    a.resource_type = Some("timeline".into());
    a.rows_out = Some(i32::try_from(items.len()).unwrap_or(i32::MAX));
    a.withheld = (!withheld.is_empty()).then(|| json!({"by_scope": withheld}));
    write_audit(&mut tx, &a).await?;
    tx.commit().await?;
    let events: Vec<Value> = items.into_iter().map(|e| e.json).collect();
    Ok(Json(json!({"events": events, "withheld": {"by_scope": withheld}})))
}
