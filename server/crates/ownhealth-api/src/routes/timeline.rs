//! Lane I: `api/timeline.py`. One dated list of what happened, built from the facts already in the
//! store.
//!
//! ```text
//! GET /api/profiles/{p}/timeline   ?types=lab,visit&from=YYYY-MM-DD&to=YYYY-MM-DD&lang=en
//! ```
//!
//! Lab draws (one event per report, not per value), medication starts and stops, diagnoses, vaccines,
//! visits, procedures, non-lab documents, imaging studies and genome imports. Only current facts;
//! sensitive rows only with `read:sensitive`; each event type only with the scope that guards its
//! facts. Every fact event cites a `src`; a document event names its document, and its filename goes
//! out as untrusted text.
//!
//! `spans` is the bars lane: one row per condition from onset to abatement, and one row per medicine
//! (grouped by ingredient, else name) with one segment per course. A segment has the annotation shape
//! of the biomarker chart, `{date, end, label}` (end null while it runs), plus its `src`.

use std::collections::BTreeSet;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::NaiveDate;
use ownhealth_core::Runtime;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::documents::sensitive_document_sql;
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

const TYPES: [&str; 9] = [
    "lab",
    "visit",
    "imaging",
    "diagnosis",
    "medication",
    "vaccine",
    "genome",
    "procedure",
    "document",
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
    flagged1: &'static str,
    started: &'static str,
    stopped: &'static str,
    genome: &'static str,
    visit: &'static str,
    /// A document type (`document.doc_type`) as a title; any other type is `document`.
    doc_types: &'static [(&'static str, &'static str)],
    document: &'static str,
    /// Stored status words (condition verification, medication status) in this language.
    enums: &'static [(&'static str, &'static str)],
}

const EN: Words = Words {
    draw: "Lab report · {n} results",
    draw1: "Lab report · 1 result",
    flagged: "{n} flagged by the lab",
    flagged1: "1 flagged by the lab",
    started: "{name} started",
    stopped: "{name} stopped",
    genome: "Genome facts imported",
    visit: "Visit",
    doc_types: &[
        ("discharge", "Discharge report"),
        ("imaging_report", "Imaging report"),
        ("prescription", "Prescription"),
        ("ecg", "ECG"),
    ],
    document: "Document",
    enums: &[("on-hold", "paused"), ("entered-in-error", "entered in error")],
};

const ES: Words = Words {
    draw: "Analítica · {n} resultados",
    draw1: "Analítica · 1 resultado",
    flagged: "{n} marcados por el laboratorio",
    flagged1: "1 marcado por el laboratorio",
    started: "Inicio de {name}",
    stopped: "Fin de {name}",
    genome: "Datos genómicos importados",
    visit: "Visita médica",
    doc_types: &[
        ("discharge", "Informe de alta"),
        ("imaging_report", "Informe de imagen"),
        ("prescription", "Receta"),
        ("ecg", "Electrocardiograma"),
    ],
    document: "Documento",
    enums: &[
        ("confirmed", "confirmado"),
        ("provisional", "provisional"),
        ("unconfirmed", "sin confirmar"),
        ("differential", "diferencial"),
        ("refuted", "descartado"),
        ("entered-in-error", "registrado por error"),
        ("active", "activo"),
        ("paused", "en pausa"),
        ("on-hold", "en pausa"),
        ("stopped", "suspendido"),
        ("completed", "completado"),
        ("intended", "previsto"),
    ],
};

impl Words {
    /// A stored status word in this language; any other text stays as it is.
    fn word(&self, v: Option<String>) -> Option<String> {
        v.map(|v| match self.enums.iter().find(|(k, _)| *k == v) {
            Some((_, w)) => (*w).to_string(),
            None => v,
        })
    }
}

/// Every fixed phrase the timeline writes, in both languages, for the wording gate (`wording_all.rs`).
pub fn phrases() -> Value {
    let one = |w: &Words| {
        let mut v = vec![
            w.draw, w.draw1, w.flagged, w.flagged1, w.started, w.stopped, w.genome, w.visit, w.document,
        ];
        v.extend(w.doc_types.iter().map(|(_, t)| *t));
        v.extend(w.enums.iter().map(|(_, t)| *t));
        v
    };
    json!({"en": one(&EN), "es": one(&ES)})
}

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

/// `ICD10 E78.0`: the code system upper-cased and the code, when both are recorded.
fn coded(r: &PgRow) -> R<Option<String>> {
    Ok(match (text(r, "code_system")?, text(r, "code")?) {
        (Some(sys), Some(code)) if !sys.is_empty() && !code.is_empty() => {
            Some(format!("{} {code}", sys.to_uppercase()))
        }
        _ => None,
    })
}

struct Event {
    date: String,
    kind: &'static str,
    title: String,
    /// Breaks ties between equal (date, type, title), so the same request gives the same bytes.
    tie: String,
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
    let tie = src.clone().unwrap_or_default();
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
        tie,
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
        let flagged_text = match flagged {
            0 => None,
            1 => Some(t.flagged1.to_string()),
            n => Some(t.flagged.replace("{n}", &n.to_string())),
        };
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
                t.word(text(&r, "reason_text")?),
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
            let detail = joined(&[coded(&r)?, t.word(text(&r, "verification_status")?)]);
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
    if want.contains("procedure") {
        let sql = format!(
            "select p.text, p.code_system, p.code, coalesce(p.performed, p.effective_at::date) as day, p.src_id, s.label \
             from procedure p join src s on s.id = p.src_id where p.profile_id = $1 and {}",
            live("p", sens)
        );
        for r in rows(conn, &sql, pid).await? {
            out.push(event(
                day(&r, "day")?,
                "procedure",
                text(&r, "text")?.unwrap_or_default(),
                coded(&r)?,
                text(&r, "src_id")?,
                text(&r, "label")?,
                None,
            ));
        }
    }
    if want.contains("document") {
        out.extend(documents(conn, pid, sens, t).await?);
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

/// Documents that are not lab reports (draws are lab events) or genome files (genome events), on the
/// day they were collected, else reported. A document with neither date has no place on the timeline
/// (the day it was uploaded is not a day anything happened). A document with a sensitive fact is
/// sensitive. A document has no `src`: it is its own source.
async fn documents(conn: &mut PgConnection, pid: Uuid, sens: bool, t: &Words) -> R<Vec<Option<Event>>> {
    let hide = if sens {
        String::new()
    } else {
        format!(" and not {}", sensitive_document_sql("d.id"))
    };
    let sql = format!(
        "select d.id, d.doc_type, d.filename, d.provider_name, coalesce(d.collected_at, d.reported_at)::date as day \
         from document d where d.profile_id = $1 and d.status = 'active' \
         and d.doc_type is distinct from 'lab_report' and d.doc_type is distinct from 'genomics'{hide} \
         order by d.id"
    );
    let mut out = Vec::new();
    for r in rows(conn, &sql, pid).await? {
        let doc_type = text(&r, "doc_type")?;
        let title = t
            .doc_types
            .iter()
            .find(|(k, _)| Some(*k) == doc_type.as_deref())
            .map_or(t.document, |(_, w)| *w);
        let provider = text(&r, "provider_name")?.filter(|p| !p.is_empty());
        let Some(mut e) = event(
            day(&r, "day")?,
            "document",
            title.to_string(),
            provider.clone(),
            None,
            None,
            provider,
        ) else {
            continue;
        };
        let id: Uuid = r.try_get("id")?;
        let filename = text(&r, "filename")?.filter(|f| !f.is_empty()).map_or(
            Value::Null,
            |f| json!({"trust": "untrusted_text", "origin": "filename", "text": f}),
        );
        if let Some(m) = e.json.as_object_mut() {
            m.insert("document_id".into(), json!(id));
            m.insert("filename".into(), filename);
        }
        e.tie = id.to_string();
        out.push(Some(e));
    }
    Ok(out)
}

/// One segment of a bar: `{date, end, label, src, src_label}`.
struct Segment {
    date: NaiveDate,
    end: Option<NaiveDate>,
    label: String,
    src: Option<String>,
    src_label: Option<String>,
}

impl Segment {
    fn json(&self) -> Value {
        json!({
            "date": py_date(&self.date),
            "end": self.end.as_ref().map(py_date),
            "label": self.label,
            "src": self.src,
            "src_label": self.src_label,
        })
    }

    /// Does the segment touch the window? An open segment runs to today.
    fn within(&self, from: Option<NaiveDate>, to: Option<NaiveDate>) -> bool {
        from.is_none_or(|f| self.end.is_none_or(|e| e >= f)) && to.is_none_or(|t| self.date <= t)
    }
}

/// A row of the bars lane: its type (`diagnosis` | `medication`), label and segments by date.
struct Span {
    kind: &'static str,
    label: String,
    segments: Vec<Segment>,
}

/// The bars lane. A condition that is resolved but has no abatement date, or a course that stopped
/// or is on hold with no stop date, is a one-day segment: it never reads as still running.
async fn spans(
    conn: &mut PgConnection,
    pid: Uuid,
    sens: bool,
    want: &BTreeSet<&str>,
    window: (Option<NaiveDate>, Option<NaiveDate>),
) -> R<Vec<Value>> {
    let mut out: Vec<Span> = Vec::new();
    if want.contains("diagnosis") {
        let sql = format!(
            "select c.text, coalesce(c.onset, c.effective_at::date) as day, c.abatement, c.clinical_status, c.src_id, \
             s.label from condition c join src s on s.id = c.src_id where c.profile_id = $1 and {} \
             and c.verification_status not in ('refuted', 'entered-in-error')",
            live("c", sens)
        );
        for r in rows(conn, &sql, pid).await? {
            let Some(start) = day(&r, "day")? else { continue };
            let over = matches!(
                text(&r, "clinical_status")?.as_deref(),
                Some("resolved" | "inactive" | "remission")
            );
            let label = text(&r, "text")?.unwrap_or_default();
            out.push(Span {
                kind: "diagnosis",
                label: label.clone(),
                segments: vec![Segment {
                    date: start,
                    end: day(&r, "abatement")?.or(over.then_some(start)),
                    label,
                    src: text(&r, "src_id")?,
                    src_label: text(&r, "label")?,
                }],
            });
        }
    }
    if want.contains("medication") {
        let sql = format!(
            "select lower(trim(coalesce(nullif(trim(m.ingredient), ''), m.name))) as key, m.name, m.strength, \
             m.started, m.stopped, m.med_status, m.src_id, s.label from medication_statement m \
             join src s on s.id = m.src_id where m.profile_id = $1 and m.started is not null and {} \
             order by m.started, m.name, m.src_id",
            live("m", sens)
        );
        let today = super::core::local_today();
        let mut by_key: std::collections::BTreeMap<String, Span> = std::collections::BTreeMap::new();
        for r in rows(conn, &sql, pid).await? {
            let Some(start) = day(&r, "started")? else { continue };
            let name = text(&r, "name")?.unwrap_or_default();
            let label = [Some(name.clone()), text(&r, "strength")?]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            // a course on hold is not running now: with no stop date it reads like a stopped one
            let stopped = matches!(text(&r, "med_status")?.as_deref(), Some("stopped" | "on_hold"));
            let seg = Segment {
                date: start,
                end: day(&r, "stopped")?.or(stopped.then_some(start)),
                label,
                src: text(&r, "src_id")?,
                src_label: text(&r, "label")?,
            };
            // the window applies before naming, so a course it removes never names the row
            if !seg.within(window.0, window.1) {
                continue;
            }
            let key: String = r.try_get::<Option<String>, _>("key")?.unwrap_or_default();
            let span = by_key.entry(key).or_insert_with(|| Span {
                kind: "medication",
                label: name.clone(),
                segments: Vec::new(),
            });
            // the row is named after its latest course that has started by today (rows come by start, then
            // name: a tie is stable); the lane draws no bar for a later course, so it never names the row,
            // unless no course has started yet
            if start <= today || span.segments.iter().all(|s| s.date > today) {
                span.label = name;
            }
            span.segments.push(seg);
        }
        out.extend(by_key.into_values());
    }
    for s in &mut out {
        s.segments.retain(|g| g.within(window.0, window.1));
        s.segments
            .sort_by(|a, b| (a.date, a.end.is_none(), a.end, &a.src).cmp(&(b.date, b.end.is_none(), b.end, &b.src)));
    }
    out.retain(|s| !s.segments.is_empty());
    let rank = |k: &str| usize::from(k != "diagnosis");
    out.sort_by(|a, b| {
        (rank(a.kind), a.segments[0].date, &a.label, &a.segments[0].src).cmp(&(
            rank(b.kind),
            b.segments[0].date,
            &b.label,
            &b.segments[0].src,
        ))
    });
    Ok(out
        .iter()
        .map(|s| {
            json!({
                "type": s.kind,
                "label": s.label,
                "segments": s.segments.iter().map(Segment::json).collect::<Vec<_>>(),
            })
        })
        .collect())
}

async fn genome(conn: &mut PgConnection, pid: Uuid, t: &Words) -> R<Vec<Option<Event>>> {
    let sql = "select g.imported_at::date as day, g.assay, g.build, g.owngenome_version, g.src_id, s.label \
               from genome_dataset g join src s on s.id = g.src_id where g.profile_id = $1 and g.status = 'current' \
               and not exists (select 1 from document d where d.id = g.document_id and d.status = 'retracted')";
    let mut out = Vec::new();
    for r in rows(conn, sql, pid).await? {
        // the producer and its version ('OwnGenome 1.4.2'); a bare version gets the name once
        let version = match text(&r, "owngenome_version")? {
            Some(v) if v.starts_with("OwnGenome") => v,
            Some(v) => format!("OwnGenome {v}"),
            None => "OwnGenome".into(),
        };
        let assay = text(&r, "assay")?.unwrap_or_default().to_uppercase();
        let build = text(&r, "build")?.unwrap_or_else(|| "None".into());
        out.push(event(
            day(&r, "day")?,
            "genome",
            t.genome.to_string(),
            Some(format!("{version} · {assay} · {build}")),
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
    let (from_day, to_day) = (from, to);
    let from = from.map(|d| py_date(&d));
    let to = to.map(|d| py_date(&d));
    let mut items: Vec<Event> = events
        .into_iter()
        .flatten()
        .filter(|e| from.as_ref().is_none_or(|f| &e.date >= f) && to.as_ref().is_none_or(|t| &e.date <= t))
        .collect();
    // newest first, as Python's sort(reverse=True); the src breaks the remaining ties
    items.sort_by(|a, b| {
        (&b.date, b.kind, &b.title)
            .cmp(&(&a.date, a.kind, &a.title))
            .then(a.tie.cmp(&b.tie))
    });
    let spans = spans(&mut tx, pid, sens, &want, (from_day, to_day)).await?;

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
    Ok(Json(
        json!({"events": events, "spans": spans, "withheld": {"by_scope": withheld}}),
    ))
}
