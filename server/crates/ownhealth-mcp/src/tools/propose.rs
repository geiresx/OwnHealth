//! propose (`mcp/tools/propose.py`, mcp.md 5.8, 9.3): the only write tool. The agent proposes,
//! OwnHealth checks, a person confirms in the review queue. Nothing here writes a fact: every call
//! creates (or, for the same payload, returns) one `agent_proposal` review item. Rows that fail a
//! check are queued and marked, never dropped, and an agent proposal never auto-accepts.
//!
//! The accept handler at the bottom runs only when a person accepts the item in the review queue.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use futures::future::BoxFuture;
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, ensure_src, resolve_src};
use ownhealth_core::records::{
    MedicationIn, RecordIn, RecordOptions, active_medications, add_record, get_record, supersede_record,
};
use ownhealth_core::review::{ReviewItem, ReviewItemIn, create_item, register_accept_handler};
use ownhealth_core::text::untrusted;
use ownhealth_core::{Error, Result};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use super::{Answer, Call, ToolResult, ToolSpec, parse_iso_date, start_of_day};
use crate::envelope::{ToolError, dumps, py_float};
use crate::gate::{self, READ_SCOPES};
use crate::limits::MAX_PROPOSAL_ROWS;
use crate::services::ProposedRow;

const LIMITATION: &str = "Nothing is saved until a person confirms it in the review queue.";

#[allow(clippy::unwrap_used)]
static SRC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^src_[a-z0-9_]+$").unwrap());
#[allow(clippy::unwrap_used)]
static DOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^doc_(?:\d{4}-\d{2}-\d{2}|undated)_([0-9a-f]{8})$").unwrap());
#[allow(clippy::unwrap_used)]
static FINDING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^f_[0-9a-f]{4,16}$").unwrap());

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "profile": {"type": "string"},
            "kind": {"enum": ["observations", "note", "finding_status", "medication"]},
            "payload": {"type": "object"},
            "based_on": {
                "type": "array",
                "items": {"type": "string", "maxLength": 80},
                "maxItems": 200,
                "description": "src or doc ids the agent read",
            },
        },
        "required": ["kind", "payload"],
        "additionalProperties": false,
    })
}

pub static SPEC: ToolSpec = ToolSpec {
    name: "propose",
    title: "Propose a change for review",
    description: "The only way to write. Propose observations read from a document (with doc_id, page and value), a \
note, a genome finding status change, or a medication start, stop or change. OwnHealth checks each \
row (code, unit, plausibility, the value grounded on the page) and puts the proposal in the review \
queue; nothing is saved until a person confirms it. Rows that fail a check are still queued and marked. \
List in based_on the src or doc ids you read. Never propose a value that is not on the page.",
    input_schema: schema,
    any_scope: &["write:propose"],
    all_scopes: &[],
    profile_bound: true,
    read_only: false,
};

// ------------------------------------------------------- payload validation ----
// The pydantic models of propose.py: the same fields, defaults, limits and `model_dump(mode="json")`
// output; extra keys are ignored as pydantic does. Error wording follows pydantic's.

/// (dotted location, message) of the first invalid field.
type Invalid = (String, String);

struct Obj<'a> {
    m: &'a Map<String, Value>,
    at: String,
}

fn loc(at: &str, key: &str) -> String {
    if at.is_empty() {
        key.to_string()
    } else {
        format!("{at}.{key}")
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn literal_list(values: &[&str]) -> String {
    let quoted: Vec<String> = values.iter().map(|v| format!("'{v}'")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        Some((last, _)) => last.clone(),
        None => String::new(),
    }
}

impl<'a> Obj<'a> {
    fn of(v: &'a Value, at: &str, model: &str) -> std::result::Result<Obj<'a>, Invalid> {
        match v {
            Value::Object(m) => Ok(Obj { m, at: at.to_string() }),
            _ => Err((
                at.to_string(),
                format!("Input should be a valid dictionary or instance of {model}"),
            )),
        }
    }

    fn err(&self, key: &str, msg: impl Into<String>) -> Invalid {
        (loc(&self.at, key), msg.into())
    }

    fn present(&self, key: &str) -> Option<&'a Value> {
        self.m.get(key)
    }

    fn str_opt(
        &self,
        key: &str,
        min: usize,
        max: Option<usize>,
        pattern: Option<&Regex>,
    ) -> std::result::Result<Option<String>, Invalid> {
        match self.present(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => {
                let n = s.chars().count();
                if n < min {
                    return Err(self.err(key, format!("String should have at least {}", plural(min, "character"))));
                }
                if let Some(max) = max
                    && n > max
                {
                    return Err(self.err(key, format!("String should have at most {}", plural(max, "character"))));
                }
                if let Some(p) = pattern
                    && !p.is_match(s)
                {
                    return Err(self.err(key, format!("String should match pattern '{}'", p.as_str())));
                }
                Ok(Some(s.clone()))
            }
            Some(_) => Err(self.err(key, "Input should be a valid string")),
        }
    }

    fn str_req(
        &self,
        key: &str,
        min: usize,
        max: Option<usize>,
        pattern: Option<&Regex>,
    ) -> std::result::Result<String, Invalid> {
        if self.present(key).is_none() {
            return Err(self.err(key, "Field required"));
        }
        if matches!(self.present(key), Some(Value::Null)) {
            return Err(self.err(key, "Input should be a valid string"));
        }
        self.str_opt(key, min, max, pattern).map(Option::unwrap_or_default)
    }

    fn literal_opt(&self, key: &str, values: &[&str]) -> std::result::Result<Option<String>, Invalid> {
        match self.present(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if values.contains(&s.as_str()) => Ok(Some(s.clone())),
            Some(_) => Err(self.err(key, format!("Input should be {}", literal_list(values)))),
        }
    }

    fn literal_req(&self, key: &str, values: &[&str]) -> std::result::Result<String, Invalid> {
        match self.present(key) {
            None => Err(self.err(key, "Field required")),
            Some(Value::Null) => Err(self.err(key, format!("Input should be {}", literal_list(values)))),
            _ => self.literal_opt(key, values).map(Option::unwrap_or_default),
        }
    }

    fn date_req(&self, key: &str) -> std::result::Result<NaiveDate, Invalid> {
        match self.present(key) {
            None => Err(self.err(key, "Field required")),
            Some(Value::String(s)) => {
                let d = parse_iso_date(s).or_else(|| {
                    // pydantic takes a datetime at midnight as a date
                    s.strip_suffix("T00:00:00")
                        .or_else(|| s.strip_suffix("T00:00:00Z"))
                        .and_then(parse_iso_date)
                });
                d.ok_or_else(|| self.err(key, "Input should be a valid date or datetime, input is too short"))
            }
            Some(_) => Err(self.err(key, "Input should be a valid date")),
        }
    }
}

#[derive(Debug, Clone)]
enum Scalar {
    Str(String),
    Int(i64),
    Float(f64),
}

impl Scalar {
    fn json(&self) -> Value {
        match self {
            Scalar::Str(s) => json!(s),
            Scalar::Int(i) => json!(i),
            Scalar::Float(f) => json!(f),
        }
    }

    /// Python `str(value)`.
    fn text(&self) -> String {
        match self {
            Scalar::Str(s) => s.clone(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => py_float(*f),
        }
    }
}

#[derive(Debug, Clone)]
struct ObsRow {
    name: String,
    value: Scalar,
    unit: Option<String>,
    range: Option<String>,
    date: NaiveDate,
    page: i64,
    bbox: Option<[f64; 4]>,
}

impl ObsRow {
    fn parse(v: &Value, at: &str) -> std::result::Result<ObsRow, Invalid> {
        let o = Obj::of(v, at, "ObsRow")?;
        let name = o.str_req("name", 1, Some(200), None)?;
        let value = match o.present("value") {
            None => return Err(o.err("value", "Field required")),
            Some(Value::String(s)) => Scalar::Str(s.clone()),
            Some(Value::Bool(b)) => Scalar::Int(i64::from(*b)),
            Some(Value::Number(n)) => match n.as_i64() {
                Some(i) => Scalar::Int(i),
                None => Scalar::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Some(_) => return Err(o.err("value.str", "Input should be a valid string")),
        };
        let unit = o.str_opt("unit", 0, Some(40), None)?;
        let range = o.str_opt("range", 0, Some(80), None)?;
        let date = o.date_req("date")?;
        let page = match o.present("page") {
            None => return Err(o.err("page", "Field required")),
            Some(Value::Number(n)) => match n.as_i64() {
                Some(i) => i,
                None => match n.as_f64() {
                    Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => f as i64,
                    _ => {
                        return Err(o.err(
                            "page",
                            "Input should be a valid integer, got a number with a fractional part",
                        ));
                    }
                },
            },
            Some(Value::String(s)) => s.trim().parse::<i64>().map_err(|_| {
                o.err(
                    "page",
                    "Input should be a valid integer, unable to parse string as an integer",
                )
            })?,
            Some(_) => return Err(o.err("page", "Input should be a valid integer")),
        };
        if page < 1 {
            return Err(o.err("page", "Input should be greater than or equal to 1"));
        }
        if page > 10_000 {
            return Err(o.err("page", "Input should be less than or equal to 10000"));
        }
        let bbox = match o.present("bbox") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => {
                if a.len() != 4 {
                    let msg = if a.len() < 4 {
                        format!("Field required ({} of 4 items)", a.len())
                    } else {
                        format!("Tuple should have at most 4 items after validation, not {}", a.len())
                    };
                    return Err(o.err("bbox", msg));
                }
                let mut out = [0.0; 4];
                for (i, x) in a.iter().enumerate() {
                    out[i] = match x {
                        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
                        Value::String(s) => s
                            .trim()
                            .parse()
                            .map_err(|_| o.err(&format!("bbox.{i}"), "Input should be a valid number"))?,
                        _ => return Err(o.err(&format!("bbox.{i}"), "Input should be a valid number")),
                    };
                }
                Some(out)
            }
            Some(_) => return Err(o.err("bbox", "Input should be a valid tuple")),
        };
        Ok(ObsRow {
            name,
            value,
            unit,
            range,
            date,
            page,
            bbox,
        })
    }

    fn dump(&self) -> Value {
        json!({
            "name": self.name,
            "value": self.value.json(),
            "unit": self.unit,
            "range": self.range,
            "date": self.date.format("%Y-%m-%d").to_string(),
            "page": self.page,
            "bbox": self.bbox.map(|b| b.to_vec()),
        })
    }
}

#[derive(Debug, Clone)]
struct ObservationsPayload {
    doc_id: String,
    rows: Vec<ObsRow>,
}

#[derive(Debug, Clone)]
struct NotePayload {
    text: String,
    attach_to: Option<String>,
    lang: Option<String>,
}

#[derive(Debug, Clone)]
struct FindingStatusPayload {
    finding_id: String,
    verification: Option<String>,
    lifecycle: Option<String>,
    reason: String,
    evidence_src: Option<String>,
}

#[derive(Debug, Clone)]
struct MedicationPayload {
    action: String,
    name: String,
    dose: Option<String>,
    schedule: Option<String>,
    date: NaiveDate,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
enum Payload {
    Observations(ObservationsPayload),
    Note(NotePayload),
    FindingStatus(FindingStatusPayload),
    Medication(MedicationPayload),
}

impl Payload {
    fn parse(kind: &str, v: &Value) -> std::result::Result<Payload, Invalid> {
        Ok(match kind {
            "observations" => {
                let o = Obj::of(v, "", "ObservationsPayload")?;
                let doc_id = o.str_req("doc_id", 0, Some(80), None)?;
                let rows = match o.present("rows") {
                    None => return Err(o.err("rows", "Field required")),
                    Some(Value::Array(a)) => {
                        if a.is_empty() {
                            return Err(o.err("rows", "List should have at least 1 item after validation, not 0"));
                        }
                        a.iter()
                            .enumerate()
                            .map(|(i, r)| ObsRow::parse(r, &format!("rows.{i}")))
                            .collect::<std::result::Result<Vec<_>, _>>()?
                    }
                    Some(_) => return Err(o.err("rows", "Input should be a valid list")),
                };
                Payload::Observations(ObservationsPayload { doc_id, rows })
            }
            "note" => {
                let o = Obj::of(v, "", "NotePayload")?;
                let text = o.str_req("text", 1, Some(4000), None)?;
                let attach_to = o.str_opt("attach_to", 0, None, None)?;
                if let Some(a) = &attach_to
                    && a != "status"
                    && !SRC_RE.is_match(a)
                {
                    return Err(o.err("attach_to", "Value error, attach_to is a src id or 'status'"));
                }
                let lang = o.literal_opt("lang", &["es", "en"])?;
                Payload::Note(NotePayload { text, attach_to, lang })
            }
            "finding_status" => {
                let o = Obj::of(v, "", "FindingStatusPayload")?;
                Payload::FindingStatus(FindingStatusPayload {
                    finding_id: o.str_req("finding_id", 0, None, Some(&FINDING_RE))?,
                    verification: o.literal_opt("verification", &["confirmed", "provisional"])?,
                    lifecycle: o.literal_opt("lifecycle", &["retracted"])?,
                    reason: o.str_req("reason", 1, Some(500), None)?,
                    evidence_src: o.str_opt("evidence_src", 0, None, Some(&SRC_RE))?,
                })
            }
            _ => {
                let o = Obj::of(v, "", "MedicationPayload")?;
                Payload::Medication(MedicationPayload {
                    action: o.literal_req("action", &["start", "stop", "change"])?,
                    name: o.str_req("name", 1, Some(200), None)?,
                    dose: o.str_opt("dose", 0, Some(100), None)?,
                    schedule: o.str_opt("schedule", 0, Some(100), None)?,
                    date: o.date_req("date")?,
                    reason: o.str_opt("reason", 0, Some(500), None)?,
                })
            }
        })
    }

    /// `model_dump(mode="json")`.
    fn dump(&self) -> Value {
        match self {
            Payload::Observations(p) => json!({
                "doc_id": p.doc_id,
                "rows": p.rows.iter().map(ObsRow::dump).collect::<Vec<_>>(),
            }),
            Payload::Note(p) => json!({"text": p.text, "attach_to": p.attach_to, "lang": p.lang}),
            Payload::FindingStatus(p) => json!({
                "finding_id": p.finding_id,
                "verification": p.verification,
                "lifecycle": p.lifecycle,
                "reason": p.reason,
                "evidence_src": p.evidence_src,
            }),
            Payload::Medication(p) => json!({
                "action": p.action,
                "name": p.name,
                "dose": p.dose,
                "schedule": p.schedule,
                "date": p.date.format("%Y-%m-%d").to_string(),
                "reason": p.reason,
            }),
        }
    }
}

// ------------------------------------------------------------------ checks ----

fn check(name: &str, ok: bool, reason: &str, detail: Option<&str>) -> Value {
    json!({"name": name, "ok": ok, "reason": if ok { None } else { Some(reason) }, "detail": detail})
}

fn failed_reasons(checks: &[Value]) -> Vec<String> {
    checks
        .iter()
        .filter(|c| c.get("ok") != Some(&Value::Bool(true)))
        .filter_map(|c| {
            c.get("reason")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .map(str::to_string)
        })
        .collect()
}

#[derive(Debug, Clone)]
struct Doc {
    id: Uuid,
    pages: Option<i32>,
    collected_at: Option<DateTime<Utc>>,
    reported_at: Option<DateTime<Utc>>,
}

async fn find_document(call: &mut Call<'_>, doc_id: &str) -> ToolResult<Option<Doc>> {
    let m = DOC_RE.captures(doc_id);
    let uid = match &m {
        Some(_) => None,
        None => match Uuid::parse_str(doc_id) {
            Ok(u) => Some(u),
            Err(_) => return Ok(None),
        },
    };
    let pid = call.profile_id()?;
    type Row = (Uuid, Option<i32>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);
    let row: Option<Row> = match (uid, m) {
        (Some(u), _) => {
            sqlx::query_as(
                "select id, pages, collected_at, reported_at from document where profile_id = $1 and id = $2 \
                 order by created_at limit 1",
            )
            .bind(pid)
            .bind(u)
            .fetch_optional(&mut *call.conn)
            .await?
        }
        (None, Some(m)) => {
            sqlx::query_as(
                "select id, pages, collected_at, reported_at from document where profile_id = $1 \
                 and replace(id::text, '-', '') like $2 order by created_at limit 1",
            )
            .bind(pid)
            .bind(format!("{}%", &m[1]))
            .fetch_optional(&mut *call.conn)
            .await?
        }
        (None, None) => None,
    };
    Ok(row.map(|(id, pages, collected_at, reported_at)| Doc {
        id,
        pages,
        collected_at,
        reported_at,
    }))
}

fn wrap(s: Option<&str>) -> Value {
    untrusted(s, "agent_note").unwrap_or(Value::Null)
}

struct Outcome {
    shown: Map<String, Value>,
    stored: Map<String, Value>,
    reasons: Vec<String>,
    limitations: Vec<String>,
    document_id: Option<Uuid>,
    any_ungrounded: bool,
}

impl Outcome {
    fn checks(checks: Vec<Value>, stored_extra: Map<String, Value>) -> Outcome {
        let reasons = failed_reasons(&checks);
        let mut shown = Map::new();
        shown.insert("checks".into(), json!(checks));
        let mut stored = Map::new();
        stored.insert("checks".into(), json!(checks));
        stored.extend(stored_extra);
        Outcome {
            shown,
            stored,
            reasons,
            limitations: Vec::new(),
            document_id: None,
            any_ungrounded: false,
        }
    }
}

async fn observations(call: &mut Call<'_>, p: &ObservationsPayload) -> ToolResult<Outcome> {
    if p.rows.len() > MAX_PROPOSAL_ROWS {
        return Err(ToolError::fix(
            "propose_cap",
            format!("At most {MAX_PROPOSAL_ROWS} rows per call."),
            "Split the proposal.",
        )
        .into());
    }
    let doc = find_document(call, &p.doc_id).await?;
    let pid = call.profile_id()?;
    let blobs = call.rt.blobs.clone();
    let mut rows_out = Vec::new();
    let mut stored = Vec::new();
    let mut reasons = Vec::new();
    let mut any_ungrounded = false;
    for row in &p.rows {
        let mut checks: Vec<Value> = Vec::new();
        let input = ProposedRow {
            profile_id: pid,
            printed_name: row.name.clone(),
            printed_value: row.value.text(),
            printed_unit: row.unit.clone(),
            printed_range: row.range.clone(),
            effective_at: start_of_day(row.date),
            lang: call.lang.clone(),
        };
        // a savepoint, so a row the normalizer cannot read does not abort the call's transaction
        let mut observation: Option<ObservationIn> = None;
        let mut sp = Connection::begin(&mut *call.conn).await?;
        match call.svc.normalize_row(&mut sp, &input).await {
            Ok(norm) => {
                sp.commit().await?;
                let mut obs = norm.observation;
                obs.document_id = doc.as_ref().map(|d| d.id);
                for c in norm.checks {
                    checks.push(check(
                        &c.name,
                        c.ok,
                        c.reason.as_deref().unwrap_or_default(),
                        c.detail.as_deref(),
                    ));
                    if let Some(last) = checks.last_mut()
                        && c.reason.is_none()
                    {
                        last["reason"] = Value::Null;
                    }
                }
                observation = Some(obs);
            }
            Err(e) => {
                sp.rollback().await?;
                tracing::warn!(error = %e, "propose: normalizer failed on one row");
                checks.push(check(
                    "normalize",
                    false,
                    "unmapped",
                    Some("the value could not be read"),
                ));
            }
        }
        let mut bbox = None;
        match &doc {
            None => checks.push(check(
                "grounding",
                false,
                "not_grounded",
                Some("document not found on this profile"),
            )),
            Some(d) if d.pages.is_some_and(|n| n != 0 && row.page > i64::from(n)) => {
                checks.push(check(
                    "grounding",
                    false,
                    "not_grounded",
                    Some("page outside the document"),
                ));
            }
            Some(d) => {
                let page = i32::try_from(row.page).unwrap_or(i32::MAX);
                let mut sp = Connection::begin(&mut *call.conn).await?;
                match call
                    .svc
                    .ground_value(&mut sp, &blobs, d.id, page, &row.name, &row.value.text())
                    .await
                {
                    Ok(b) => {
                        sp.commit().await?;
                        bbox = b;
                        checks.push(check(
                            "grounding",
                            bbox.is_some(),
                            "not_grounded",
                            if bbox.is_some() {
                                None
                            } else {
                                Some("value not found on the page word layer")
                            },
                        ));
                    }
                    Err(e) => {
                        sp.rollback().await?;
                        tracing::warn!(error = %e, "propose: grounding failed on one row");
                        checks.push(check(
                            "grounding",
                            false,
                            "not_grounded",
                            Some("the page could not be read"),
                        ));
                    }
                }
            }
        }
        if let Some(d) = &doc {
            let days: Vec<NaiveDate> = [d.collected_at, d.reported_at]
                .iter()
                .flatten()
                .map(|t| t.date_naive())
                .collect();
            if let (Some(lo), Some(hi)) = (days.iter().min(), days.iter().max()) {
                let ok = *lo - Duration::days(1) <= row.date && row.date <= *hi + Duration::days(1);
                checks.push(check(
                    "date_in_document",
                    ok,
                    "date_invalid",
                    if ok { None } else { Some("date outside the report") },
                ));
            }
        }
        let failed: Vec<&Value> = checks
            .iter()
            .filter(|c| c.get("ok") != Some(&Value::Bool(true)))
            .collect();
        reasons.extend(failed_reasons(&checks));
        let grounded = bbox.is_some();
        any_ungrounded |= !grounded;
        let failed_names: Vec<Value> = failed.iter().filter_map(|c| c.get("name").cloned()).collect();
        let first_reason = failed
            .first()
            .map_or(json!("ok"), |c| c.get("reason").cloned().unwrap_or(Value::Null));
        rows_out.push(json!({
            "name": wrap(Some(&row.name)),
            "value": match &row.value { Scalar::Str(s) => wrap(Some(s)), other => other.json() },
            "unit": wrap(row.unit.as_deref()),
            "loinc": observation.as_ref().and_then(|o| o.loinc.clone()),
            "grounded": grounded,
            "bbox": bbox.map(|b| b.to_vec()),
            "check": first_reason,
            "failed": if failed_names.is_empty() { Value::Null } else { json!(failed_names) },
        }));
        let obs_json = match &observation {
            Some(o) => serde_json::to_value(o).map_err(Error::Json)?,
            None => Value::Null,
        };
        stored.push(json!({
            "input": row.dump(),
            "observation": obs_json,
            "checks": checks,
            "grounded": grounded,
            "bbox": bbox.map(|b| b.to_vec()),
        }));
    }
    let mut shown = Map::new();
    shown.insert("rows".into(), json!(rows_out));
    let mut st = Map::new();
    st.insert("rows".into(), json!(stored));
    st.insert("document_id".into(), json!(doc.as_ref().map(|d| d.id.to_string())));
    Ok(Outcome {
        shown,
        stored: st,
        reasons,
        limitations: Vec::new(),
        document_id: doc.map(|d| d.id),
        any_ungrounded,
    })
}

async fn note(call: &mut Call<'_>, p: &NotePayload) -> ToolResult<Outcome> {
    let mut checks = Vec::new();
    if let Some(target) = p.attach_to.as_deref().filter(|a| *a != "status") {
        let res = resolve_src(&mut *call.conn, target).await?;
        let mut visible = res
            .as_ref()
            .is_some_and(|r| r.profile_id == Some(call.profile_id().unwrap_or_default()));
        if visible {
            let perm = call.perm()?.clone();
            let g = call.gate()?;
            visible = gate::hidden_reasons(&mut *call.conn, &perm, g, &[target.to_string()])
                .await?
                .is_empty();
        }
        checks.push(check(
            "attach_to",
            visible,
            "unmapped",
            if visible {
                None
            } else {
                Some("src not found on this profile")
            },
        ));
    }
    Ok(Outcome::checks(checks, Map::new()))
}

async fn finding_status(call: &mut Call<'_>, p: &FindingStatusPayload) -> ToolResult<Outcome> {
    if !call.has("read:genome") {
        return Err(ToolError::fix(
            "scope_missing",
            "Proposals about genome findings need read:genome.",
            "Use a token with read:genome.",
        )
        .into());
    }
    if p.verification.is_none() && p.lifecycle.is_none() {
        return Err(ToolError::fix(
            "bad_input",
            "Name a verification or a lifecycle change.",
            "Set verification or lifecycle.",
        )
        .into());
    }
    let pid = call.profile_id()?;
    let mut row: Option<(Uuid, String, String, Option<String>)> = sqlx::query_as(
        "select id, src_id, status, verification_status from genome_finding where profile_id = $1 \
         and fact_id like $2 and status = 'current' order by recorded_at desc limit 1",
    )
    .bind(pid)
    .bind(format!("{}%", p.finding_id))
    .fetch_optional(&mut *call.conn)
    .await?;
    if let Some((_, src_id, _, _)) = &row {
        let perm = call.perm()?.clone();
        let g = call.gate()?;
        if !gate::hidden_reasons(&mut *call.conn, &perm, g, std::slice::from_ref(src_id))
            .await?
            .is_empty()
        {
            row = None; // a finding the caller cannot see does not exist for it
        }
    }
    let mut checks = vec![check(
        "finding_exists",
        row.is_some(),
        "unmapped",
        if row.is_some() {
            None
        } else {
            Some("no such current finding")
        },
    )];
    if let (Some((_, _, _, current)), Some(v)) = (&row, &p.verification) {
        let ok = Some(v) != current.as_ref();
        let detail = format!("already {v}");
        checks.push(check(
            "transition",
            ok,
            "conflict",
            if ok { None } else { Some(detail.as_str()) },
        ));
    }
    let mut extra = Map::new();
    extra.insert("finding_row".into(), json!(row.map(|r| r.0.to_string())));
    Ok(Outcome::checks(checks, extra))
}

async fn medication(call: &mut Call<'_>, p: &MedicationPayload) -> ToolResult<Outcome> {
    let mut checks = Vec::new();
    let mut limitations = Vec::new();
    let drug = call.svc.resolve_drug(&mut *call.conn, &p.name).await?;
    if drug.is_none() {
        limitations.push("The drug name did not resolve; it is kept as free text.".to_string());
    }
    let mut match_id: Option<String> = None;
    if p.action == "stop" || p.action == "change" {
        if call.has("read:records") {
            let pid = call.profile_id()?;
            let needle = p.name.to_lowercase();
            for m in active_medications(&mut *call.conn, pid, Some(p.date), None, None, true).await? {
                let name = m.get("name").and_then(Value::as_str).unwrap_or_default().to_lowercase();
                if needle.contains(&name) || name.contains(&needle) {
                    match_id = m.get("id").and_then(Value::as_str).map(str::to_string);
                    break;
                }
            }
            checks.push(check(
                "medication_found",
                match_id.is_some(),
                "unmapped",
                if match_id.is_some() {
                    None
                } else {
                    Some("no current medication with that name")
                },
            ));
        } else {
            checks.push(check(
                "medication_found",
                false,
                "unmapped",
                Some("not checked: needs read:records"),
            ));
        }
    }
    let drug = drug.unwrap_or(Value::Null);
    let mut extra = Map::new();
    extra.insert("drug".into(), drug.clone());
    extra.insert("match_id".into(), json!(match_id));
    let mut out = Outcome::checks(checks, extra);
    out.shown.insert("drug".into(), drug);
    out.limitations = limitations;
    Ok(out)
}

// ------------------------------------------------------------------- tool ----

/// based_on plus every src id this token was shown in the last hour (security-privacy.md 4.2 #4).
async fn context_src_ids(call: &mut Call<'_>, based_on: &[String]) -> ToolResult<Vec<String>> {
    let recent: Vec<Option<String>> = sqlx::query_scalar(
        "select distinct unnest(src_ids) from audit_event where token_id = $1 and src_ids is not null \
         and ts > now() - interval '1 hour'",
    )
    .bind(call.principal.token_id)
    .fetch_all(&mut *call.conn)
    .await?;
    let recent: BTreeSet<String> = recent.into_iter().flatten().collect();
    let mut ids: Vec<String> = Vec::new();
    for b in based_on.iter().filter(|b| SRC_RE.is_match(b)).chain(recent.iter()) {
        if !ids.contains(b) {
            ids.push(b.clone());
        }
    }
    ids.truncate(500);
    Ok(ids)
}

pub async fn propose(call: &mut Call<'_>) -> ToolResult<Answer> {
    if !READ_SCOPES.iter().any(|s| call.has(s)) {
        return Err(ToolError::fix(
            "scope_missing",
            "Proposing needs read access to the profile.",
            "Add a read scope.",
        )
        .into());
    }
    let kind = call.arg_str("kind").unwrap_or_default().to_string();
    let based_on: Vec<String> = call
        .args
        .get("based_on")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let raw = call.args.get("payload").cloned().unwrap_or(Value::Null);
    let mut payload = Payload::parse(&kind, &raw).map_err(|(at, msg)| {
        let at = if at.is_empty() { "payload".to_string() } else { at };
        ToolError::fix(
            "bad_input",
            format!("Invalid payload.{at}: {msg}"),
            "Check the payload for this kind.",
        )
    })?;
    if let Payload::Note(n) = &mut payload
        && n.lang.is_none()
    {
        n.lang = Some(call.lang.clone());
    }
    let pid = call.profile_id()?;
    let dumped = payload.dump();
    let phash = hex::encode(Sha256::digest(
        dumps(&json!({"p": pid.to_string(), "k": kind, "x": dumped, "b": based_on})).as_bytes(),
    ));
    let existing: Option<Uuid> = sqlx::query_scalar(
        "select id from review_item where kind = 'agent_proposal' and proposed_by_token = $1 \
         and payload->>'payload_hash' = $2 and status = 'open' limit 1",
    )
    .bind(call.principal.token_id)
    .bind(&phash)
    .fetch_optional(&mut *call.conn)
    .await?;
    if existing.is_none() && call.usage.proposals >= call.limits.proposals_per_day {
        return Err(ToolError::fix(
            "propose_cap",
            "This token reached its proposals for today.",
            "It resets at 00:00.",
        )
        .into());
    }

    let mut limitations = vec![json!(LIMITATION)];
    let out = match &payload {
        Payload::Observations(p) => observations(call, p).await?,
        Payload::Note(p) => note(call, p).await?,
        Payload::FindingStatus(p) => finding_status(call, p).await?,
        Payload::Medication(p) => medication(call, p).await?,
    };
    limitations.extend(out.limitations.iter().map(|l| json!(l)));

    let mut counters = crate::limits::Usage::default();
    let item_id = match existing {
        Some(id) => id,
        None => {
            let mut docs: Vec<Uuid> = Vec::new();
            for b in &based_on {
                if DOC_RE.is_match(b)
                    && let Some(d) = find_document(call, b).await?
                {
                    docs.push(d.id);
                }
            }
            if kind == "observations"
                && let Some(d) = out.document_id
            {
                docs.push(d);
            }
            let mut stored_payload = Map::new();
            stored_payload.insert("kind".into(), json!(kind));
            stored_payload.insert("payload".into(), dumped.clone());
            stored_payload.extend(out.stored.clone());
            stored_payload.insert("token_label".into(), json!(call.principal.token_label));
            stored_payload.insert("based_on".into(), json!(based_on));
            stored_payload.insert("payload_hash".into(), json!(phash));
            let reasons: BTreeSet<String> = out.reasons.iter().cloned().collect();
            let mut unique_docs: Vec<Uuid> = Vec::new();
            for d in &docs {
                if !unique_docs.contains(d) {
                    unique_docs.push(*d);
                }
            }
            let ctx = context_src_ids(call, &based_on).await?;
            let household = call.perm()?.profile.household_id;
            let item = ReviewItemIn {
                kind: "agent_proposal".into(),
                household_id: Some(household),
                profile_id: Some(pid),
                payload: Value::Object(stored_payload),
                reasons: reasons.into_iter().collect(),
                document_id: if kind == "observations" {
                    docs.last().copied()
                } else {
                    None
                },
                row_conf: (kind == "observations" && out.any_ungrounded).then_some(0.0),
                proposed_by_token: call.principal.token_id,
                context_documents: (!unique_docs.is_empty()).then_some(unique_docs),
                context_src_ids: (!ctx.is_empty()).then_some(ctx),
                ..ReviewItemIn::default()
            };
            let id = create_item(&mut *call.conn, &item).await?;
            counters.proposals = 1;
            id
        }
    };
    let handle: String = format!(
        "prop_{}",
        item_id.simple().to_string().chars().take(8).collect::<String>()
    );
    let mut data = Map::new();
    data.insert("proposal_id".into(), json!(handle));
    data.insert("status".into(), json!("queued_for_review"));
    data.insert("kind".into(), json!(kind));
    data.extend(out.shown);
    let base = call.rt.settings.base_url.trim_end_matches('/');
    data.insert(
        "review_url".into(),
        json!(format!("{base}/import-review?item={item_id}")),
    );
    if existing.is_some() {
        data.insert("duplicate_of_open_proposal".into(), Value::Bool(true));
    }
    let mut a = Answer::new(data);
    a.budget = 1000;
    a.limitations = limitations;
    a.counters = counters;
    a.prune = false;
    Ok(a)
}

// ---------------------------------------------------------- accept handler ----

fn proposal_src(item: &ReviewItem, user_id: Uuid) -> SrcIn {
    let mut s = SrcIn::new("agent_proposal");
    s.profile_id = item.profile_id;
    s.actor_kind = "user".into();
    s.actor_id = Some(user_id);
    s.label = Some("agent proposal".into());
    s
}

fn str_of<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

/// A person accepted the proposal: write what it proposed, as a person-accepted fact.
pub async fn accept_proposal(
    conn: &mut PgConnection,
    item: &ReviewItem,
    user_id: Uuid,
    edits: Option<&Value>,
) -> Result<Vec<Uuid>> {
    let empty = Map::new();
    let p = item.payload.as_object().unwrap_or(&empty);
    let kind = str_of(p, "kind").unwrap_or_default();
    let mut body = p.get("payload").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(e) = edits.and_then(|e| e.get("payload")).and_then(Value::as_object) {
        body.extend(e.clone());
    }
    let profile_id = item
        .profile_id
        .ok_or_else(|| Error::Review("this proposal has no profile".into()))?;
    match kind {
        "note" => {
            let sid = ensure_src(&mut *conn, &SrcRef::New(proposal_src(item, user_id)), Some(profile_id)).await?;
            let attach = str_of(&body, "attach_to");
            let note_kind = if attach.is_none_or(|a| a == "status") {
                "priorities"
            } else {
                "journal"
            };
            let text = str_of(&body, "text").ok_or_else(|| Error::Review("the proposal has no text".into()))?;
            let id: Uuid = sqlx::query_scalar(
                "insert into note (profile_id, kind, author_kind, lang, attach_to, body, effective_at, src_id) \
                 values ($1, $2, 'agent', $3, $4, $5, now(), $6) returning id",
            )
            .bind(profile_id)
            .bind(note_kind)
            .bind(str_of(&body, "lang"))
            .bind(attach)
            .bind(text)
            .bind(&sid)
            .fetch_one(&mut *conn)
            .await?;
            Ok(vec![id])
        }
        "observations" => {
            let rows: Vec<Value> = p.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
            let keep: BTreeSet<usize> = match edits.and_then(|e| e.get("rows")).and_then(Value::as_array) {
                Some(k) if !k.is_empty() => k.iter().filter_map(Value::as_u64).map(|i| i as usize).collect(),
                _ => (0..rows.len()).collect(),
            };
            let mut created = Vec::new();
            for (i, row) in rows.iter().enumerate() {
                let Some(obs_v) = row.get("observation").filter(|o| crate::envelope::truthy(o)) else {
                    continue;
                };
                if !keep.contains(&i) {
                    continue;
                }
                let obs = ObservationIn::from_json(obs_v.clone())?;
                let bbox: Option<[f64; 4]> = row
                    .get("bbox")
                    .filter(|b| crate::envelope::truthy(b))
                    .and_then(|b| serde_json::from_value(b.clone()).ok());
                let src = match (bbox, obs.document_id) {
                    (Some(b), Some(doc)) => {
                        let mut s = SrcIn::new("document_region");
                        s.profile_id = Some(profile_id);
                        s.document_id = Some(doc);
                        s.page_no = row
                            .pointer("/input/page")
                            .and_then(Value::as_i64)
                            .and_then(|n| i32::try_from(n).ok());
                        s.bbox = Some(b);
                        s.actor_kind = "user".into();
                        s.actor_id = Some(user_id);
                        s
                    }
                    _ => proposal_src(item, user_id),
                };
                let opts = CommitOptions {
                    reviewed_by: Some(user_id),
                    ..CommitOptions::new("person")
                };
                created.push(commit_observation(&mut *conn, &obs, &SrcRef::New(src), opts).await?.id);
            }
            if created.is_empty() {
                return Err(Error::Review(
                    "no row of this proposal could be normalized; reject it or enter the values by hand".into(),
                ));
            }
            Ok(created)
        }
        "medication" => {
            let when = str_of(&body, "date")
                .and_then(parse_iso_date)
                .ok_or_else(|| Error::Review("the proposal has no valid date".into()))?;
            let action = str_of(&body, "action").unwrap_or_default().to_string();
            let src = SrcRef::New(proposal_src(item, user_id));
            if action == "start" {
                let rec: MedicationIn = serde_json::from_value(json!({
                    "profile_id": profile_id,
                    "name": str_of(&body, "name").unwrap_or_default(),
                    "dose_text": str_of(&body, "dose"),
                    "started": when,
                    "med_status": "active",
                    "reason_text": str_of(&body, "reason"),
                }))?;
                let rec = RecordIn::Medication(rec);
                return Ok(vec![
                    add_record(&mut *conn, &rec, &src, RecordOptions::new("person"))
                        .await?
                        .id,
                ]);
            }
            let Some(match_id) = str_of(p, "match_id").and_then(|m| Uuid::parse_str(m).ok()) else {
                return Err(Error::Review(
                    "no current medication matched this proposal; edit the medication on the records page".into(),
                ));
            };
            let old = get_record(&mut *conn, "medication", match_id)
                .await?
                .ok_or_else(|| Error::Review("the matched medication is gone".into()))?;
            let mut fields = Map::new();
            for k in [
                "profile_id",
                "kind",
                "name",
                "ingredient",
                "strength",
                "dose_text",
                "code_system",
                "code",
                "started",
                "stopped",
                "med_status",
                "reason_text",
            ] {
                fields.insert(k.into(), old.get(k).cloned().unwrap_or(Value::Null));
            }
            if action == "stop" {
                fields.insert("stopped".into(), json!(when));
                fields.insert("med_status".into(), json!("stopped"));
            } else if let Some(d) = str_of(&body, "dose").filter(|d| !d.is_empty()) {
                fields.insert("dose_text".into(), json!(d));
            }
            let new = RecordIn::Medication(serde_json::from_value(Value::Object(fields))?);
            let c = supersede_record(
                &mut *conn,
                match_id,
                &new,
                &src,
                "person",
                &format!("agent proposal {action}"),
            )
            .await?;
            Ok(vec![c.id])
        }
        _ => Err(Error::Review(
            "genome finding changes are applied on the genome page".into(),
        )),
    }
}

fn accept_boxed<'c>(
    conn: &'c mut PgConnection,
    item: &'c ReviewItem,
    user_id: Uuid,
    edits: Option<&'c Value>,
) -> BoxFuture<'c, Result<Vec<Uuid>>> {
    Box::pin(accept_proposal(conn, item, user_id, edits))
}

pub fn register() {
    register_accept_handler("agent_proposal", accept_boxed);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn payloads_dump_like_pydantic() {
        let p = Payload::parse(
            "observations",
            &json!({"doc_id": "doc_x", "rows": [{"name": "Hierro", "value": 2.1, "date": "2026-09-14",
                     "page": 1, "bbox": [1, 2, 3, 4], "extra": true}]}),
        )
        .unwrap();
        assert_eq!(
            dumps(&p.dump()),
            r#"{"doc_id":"doc_x","rows":[{"name":"Hierro","value":2.1,"unit":null,"range":null,"date":"2026-09-14","page":1,"bbox":[1.0,2.0,3.0,4.0]}]}"#
        );
        let e = Payload::parse("note", &json!({"text": "x".repeat(4001)})).unwrap_err();
        assert_eq!(
            e,
            (
                "text".to_string(),
                "String should have at most 4000 characters".to_string()
            )
        );
        let e = Payload::parse("note", &json!({"text": "x", "attach_to": "nope"})).unwrap_err();
        assert_eq!(e.0, "attach_to");
        let e = Payload::parse(
            "medication",
            &json!({"action": "pause", "name": "x", "date": "2026-01-01"}),
        )
        .unwrap_err();
        assert_eq!(e.1, "Input should be 'start', 'stop' or 'change'");
        let e = Payload::parse(
            "observations",
            &json!({"doc_id": "d", "rows": [{"name": "a", "value": 1, "date": "2026-01-01", "page": 0}]}),
        )
        .unwrap_err();
        assert_eq!(
            e,
            (
                "rows.0.page".to_string(),
                "Input should be greater than or equal to 1".to_string()
            )
        );
    }

    #[test]
    fn values_print_like_python_str() {
        assert_eq!(Scalar::Float(1e-7).text(), "1e-07");
        assert_eq!(Scalar::Float(2.0).text(), "2.0");
        assert_eq!(Scalar::Int(97).text(), "97");
    }
}
