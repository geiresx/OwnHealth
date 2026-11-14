//! Records: conditions, medications (and supplements), allergies, immunizations, procedures and
//! visits (`core/records.py`; the visit kind is new in 0.4.0 and writes the `encounter` table).
//!
//! Same lifecycle as observations: add, supersede with a new version, retract with a reason. The
//! typed columns are the query source; `fhir` is built from them unless an importer passes the
//! received resource.

use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::AssertSqlSafe;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::lifecycle;
use crate::observations::{ACCEPTED_BY, Committed, SENSITIVITY};
use crate::provenance::{SrcIn, SrcRef, ensure_src};
use crate::pyjson;

/// Record kind -> table.
pub const KINDS: [(&str, &str); 6] = [
    ("condition", "condition"),
    ("medication", "medication_statement"),
    ("allergy", "allergy"),
    ("immunization", "immunization"),
    ("procedure", "procedure"),
    ("visit", "encounter"),
];

pub fn table_of(kind: &str) -> Result<&'static str> {
    KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, t)| *t)
        .ok_or_else(|| Error::Invalid(format!("unknown record kind {kind}")))
}

fn active() -> String {
    "active".into()
}
fn provisional() -> String {
    "provisional".into()
}
fn confirmed() -> String {
    "confirmed".into()
}
fn medication() -> String {
    "medication".into()
}

/// The fields every record has (`_Common`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Common {
    pub profile_id: Uuid,
    #[serde(default, with = "pyjson::opt_datetime")]
    pub effective_at: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    pub sensitivity: Vec<String>,
    #[serde(default)]
    pub fhir: Option<Value>,
}

impl Common {
    pub fn new(profile_id: Uuid) -> Common {
        Common {
            profile_id,
            effective_at: None,
            sensitivity: Vec::new(),
            fhir: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConditionIn {
    #[serde(flatten)]
    pub common: Common,
    pub text: String,
    #[serde(default)]
    pub code_system: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default = "active")]
    pub clinical_status: String,
    /// provisional | confirmed | refuted | unconfirmed | differential | entered-in-error
    #[serde(default = "provisional")]
    pub verification_status: String,
    #[serde(default)]
    pub onset: Option<NaiveDate>,
    #[serde(default)]
    pub abatement: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MedicationIn {
    #[serde(flatten)]
    pub common: Common,
    /// medication | supplement
    #[serde(default = "medication")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub ingredient: Option<String>,
    #[serde(default)]
    pub strength: Option<String>,
    #[serde(default)]
    pub dose_text: Option<String>,
    #[serde(default)]
    pub code_system: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub started: Option<NaiveDate>,
    #[serde(default)]
    pub stopped: Option<NaiveDate>,
    /// active | stopped | on_hold
    #[serde(default = "active")]
    pub med_status: String,
    #[serde(default)]
    pub reason_text: Option<String>,
    // Migration 1100. Left out of the column list when absent, so a row without them is written as
    // before and reads them as null.
    /// When it is taken, as written ("twice daily").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_text: Option<String>,
    /// Who prescribed it, as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prescriber: Option<String>,
    /// Every coding the source sent, as received. The primary code stays in `code_system`/`code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codes: Option<Vec<MedCode>>,
    // Migration 1101, left out the same way. The person's own choice, never advice; it stays out of
    // the state body.
    /// keep | stop | do_not_repurchase
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Why, as the person wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    /// The day the person decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_on: Option<NaiveDate>,
}

/// The decisions a person can record on a medicine or supplement.
pub const DECISIONS: [&str; 3] = ["keep", "stop", "do_not_repurchase"];

/// The `status_reason` of a version replaced by a new decision: a code, which the UI says in the
/// reader's language.
pub const DECISION_RECORDED: &str = "decision recorded";

/// One coding of a medicine as the source sent it (system URL, code, display).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MedCode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllergyIn {
    #[serde(flatten)]
    pub common: Common,
    pub substance: String,
    #[serde(default)]
    pub code_system: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    /// low | high | unable-to-assess
    #[serde(default)]
    pub criticality: Option<String>,
    #[serde(default)]
    pub reaction_text: Option<String>,
    #[serde(default = "active")]
    pub clinical_status: String,
    #[serde(default = "confirmed")]
    pub verification_status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImmunizationIn {
    #[serde(flatten)]
    pub common: Common,
    pub vaccine: String,
    #[serde(default)]
    pub code_system: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub occurred: Option<NaiveDate>,
    #[serde(default)]
    pub dose_number: Option<i32>,
    #[serde(default)]
    pub lot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureIn {
    #[serde(flatten)]
    pub common: Common,
    pub text: String,
    #[serde(default)]
    pub code_system: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub performed: Option<NaiveDate>,
}

/// A visit to a clinician (a row of `encounter`). It always has a start, so it has a place on the
/// timeline; the other fields are as written or as the source sent them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisitIn {
    #[serde(flatten)]
    pub common: Common,
    /// The FHIR Encounter class code as received (AMB, EMER, IMP, VR ...).
    #[serde(default)]
    pub class: Option<String>,
    /// The clinic or clinician, as written.
    #[serde(default)]
    pub provider_name: Option<String>,
    #[serde(default, with = "pyjson::opt_datetime")]
    pub period_start: Option<DateTime<FixedOffset>>,
    #[serde(default, with = "pyjson::opt_datetime")]
    pub period_end: Option<DateTime<FixedOffset>>,
    /// Why the visit happened, as written ("Knee pain").
    #[serde(default)]
    pub reason_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecordIn {
    Condition(ConditionIn),
    Medication(MedicationIn),
    Allergy(AllergyIn),
    Immunization(ImmunizationIn),
    Procedure(ProcedureIn),
    Visit(VisitIn),
}

fn check(field: &str, v: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{field}: unknown value {v:?}")))
    }
}

impl RecordIn {
    pub fn kind(&self) -> &'static str {
        match self {
            RecordIn::Condition(_) => "condition",
            RecordIn::Medication(_) => "medication",
            RecordIn::Allergy(_) => "allergy",
            RecordIn::Immunization(_) => "immunization",
            RecordIn::Procedure(_) => "procedure",
            RecordIn::Visit(_) => "visit",
        }
    }

    pub fn table(&self) -> &'static str {
        match self {
            RecordIn::Medication(_) => "medication_statement",
            RecordIn::Visit(_) => "encounter",
            _ => self.kind(),
        }
    }

    pub fn common(&self) -> &Common {
        match self {
            RecordIn::Condition(r) => &r.common,
            RecordIn::Medication(r) => &r.common,
            RecordIn::Allergy(r) => &r.common,
            RecordIn::Immunization(r) => &r.common,
            RecordIn::Procedure(r) => &r.common,
            RecordIn::Visit(r) => &r.common,
        }
    }

    /// `MODELS[kind].model_validate(data)`.
    pub fn from_json(kind: &str, v: Value) -> Result<RecordIn> {
        let invalid = |e: serde_json::Error| Error::Invalid(e.to_string());
        let r = match kind {
            "condition" => RecordIn::Condition(serde_json::from_value(v).map_err(invalid)?),
            "medication" => RecordIn::Medication(serde_json::from_value(v).map_err(invalid)?),
            "allergy" => RecordIn::Allergy(serde_json::from_value(v).map_err(invalid)?),
            "immunization" => RecordIn::Immunization(serde_json::from_value(v).map_err(invalid)?),
            "procedure" => RecordIn::Procedure(serde_json::from_value(v).map_err(invalid)?),
            "visit" => RecordIn::Visit(serde_json::from_value(v).map_err(invalid)?),
            other => return Err(Error::Invalid(format!("unknown record kind {other}"))),
        };
        r.validate()?;
        Ok(r)
    }

    /// The Literal checks of the pydantic models.
    pub fn validate(&self) -> Result<()> {
        if let Some(s) = self
            .common()
            .sensitivity
            .iter()
            .find(|s| !SENSITIVITY.contains(&s.as_str()))
        {
            return Err(Error::Invalid(format!("sensitivity: unknown value {s:?}")));
        }
        match self {
            RecordIn::Condition(c) => check(
                "verification_status",
                &c.verification_status,
                &[
                    "provisional",
                    "confirmed",
                    "refuted",
                    "unconfirmed",
                    "differential",
                    "entered-in-error",
                ],
            ),
            RecordIn::Medication(m) => {
                check("kind", &m.kind, &["medication", "supplement"])?;
                if m.codes.iter().flatten().any(|c| c.code.trim().is_empty()) {
                    return Err(Error::Invalid("codes: a coding needs a code".into()));
                }
                if let Some(d) = &m.decision {
                    check("decision", d, &DECISIONS)?;
                }
                check("med_status", &m.med_status, &["active", "stopped", "on_hold"])
            }
            RecordIn::Allergy(a) => match &a.criticality {
                Some(c) => check("criticality", c, &["low", "high", "unable-to-assess"]),
                None => Ok(()),
            },
            RecordIn::Visit(v) => match (v.period_start, v.period_end) {
                (None, _) => Err(Error::Invalid("period_start: a visit needs the day it happened".into())),
                (Some(s), Some(e)) if e < s => Err(Error::Invalid("period_end: before period_start".into())),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    fn build_fhir(&self) -> Value {
        match self {
            RecordIn::Condition(r) => crate::fhir::condition(r),
            RecordIn::Medication(r) => crate::fhir::medication_statement(r),
            RecordIn::Allergy(r) => crate::fhir::allergy(r),
            RecordIn::Immunization(r) => crate::fhir::immunization(r),
            RecordIn::Procedure(r) => crate::fhir::procedure(r),
            RecordIn::Visit(r) => crate::fhir::encounter(r),
        }
    }

    /// The typed columns as JSON (the model dump without `fhir`), in model order.
    fn columns(&self) -> Result<Map<String, Value>> {
        let v = match self {
            RecordIn::Condition(r) => serde_json::to_value(r)?,
            RecordIn::Medication(r) => serde_json::to_value(r)?,
            RecordIn::Allergy(r) => serde_json::to_value(r)?,
            RecordIn::Immunization(r) => serde_json::to_value(r)?,
            RecordIn::Procedure(r) => serde_json::to_value(r)?,
            RecordIn::Visit(r) => serde_json::to_value(r)?,
        };
        let Value::Object(mut m) = v else {
            return Err(Error::Invalid("a record model is not an object".into()));
        };
        m.shift_remove("fhir");
        Ok(m)
    }
}

/// The SQL type a record column is cast to from its JSON text (`jsonb_populate_record` would need
/// a composite type per table; the casts keep one statement for every kind).
fn sql_type(column: &str) -> &'static str {
    match column {
        "profile_id" => "uuid",
        "effective_at" | "period_start" | "period_end" => "timestamptz",
        "sensitivity" => "text[]",
        "onset" | "abatement" | "started" | "stopped" | "occurred" | "performed" | "decision_on" => "date",
        "dose_number" => "int",
        "codes" => "jsonb",
        _ => "text",
    }
}

pub struct RecordOptions<'a> {
    pub accepted_by: &'a str,
    pub status: &'a str,
    pub supersedes: Option<Uuid>,
}

impl<'a> RecordOptions<'a> {
    pub fn new(accepted_by: &'a str) -> Self {
        RecordOptions {
            accepted_by,
            status: "current",
            supersedes: None,
        }
    }
}

pub async fn add_record(
    conn: &mut PgConnection,
    record: &RecordIn,
    src: &SrcRef,
    opts: RecordOptions<'_>,
) -> Result<Committed> {
    record.validate()?;
    if !ACCEPTED_BY.contains(&opts.accepted_by) {
        return Err(Error::Invalid(format!(
            "accepted_by: unknown value {:?}",
            opts.accepted_by
        )));
    }
    let profile_id = record.common().profile_id;
    let src = match src {
        SrcRef::New(s) if s.profile_id.is_none() => SrcRef::New(SrcIn {
            profile_id: Some(profile_id),
            ..s.clone()
        }),
        other => other.clone(),
    };
    let src_id = ensure_src(&mut *conn, &src, Some(profile_id)).await?;
    let mut resource = record.common().fhir.clone().unwrap_or_else(|| record.build_fhir());
    if let RecordIn::Medication(m) = record {
        crate::fhir::set_medication_decision(&mut resource, m);
    }
    let cols = record.columns()?;
    // One jsonb parameter holds the typed columns; each is read back with its SQL type.
    let names: Vec<&String> = cols.keys().collect();
    let values: Vec<String> = names
        .iter()
        .map(|c| match sql_type(c) {
            "text[]" => format!("array(select jsonb_array_elements_text($1->'{c}'))"),
            "jsonb" => format!("$1->'{c}'"),
            t => format!("($1->>'{c}')::{t}"),
        })
        .collect();
    let sql = format!(
        "insert into {} ({}, src_id, accepted_by, status, supersedes, fhir) values ({}, $2, $3, $4, $5, $6) returning id",
        record.table(),
        names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", "),
        values.join(", ")
    );
    let id: Uuid = sqlx::query_scalar(AssertSqlSafe(sql.as_str()))
        .bind(Value::Object(cols.clone()))
        .bind(&src_id)
        .bind(opts.accepted_by)
        .bind(opts.status)
        .bind(opts.supersedes)
        .bind(&resource)
        .fetch_one(conn)
        .await?;
    Ok(Committed { id, src_id })
}

/// A correction: the new version replaces the old one, which becomes superseded.
pub async fn supersede_record(
    conn: &mut PgConnection,
    old_id: Uuid,
    new: &RecordIn,
    src: &SrcRef,
    accepted_by: &str,
    reason: &str,
) -> Result<Committed> {
    let table = new.table();
    // One transaction (a savepoint inside the caller's), as in `supersede_observation`.
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let old = lifecycle::lock_current(&mut tx, table, old_id).await?;
    if old.get("profile_id").and_then(Value::as_str) != Some(new.common().profile_id.to_string().as_str()) {
        return Err(Error::Lifecycle(
            "a correction cannot move a record to another profile".into(),
        ));
    }
    lifecycle::mark_superseded(&mut tx, table, old_id, Some(reason)).await?;
    // A new version that says nothing of the decision (a source update, a correction from a form
    // without it) keeps the person's recorded decision, as the label photos below do.
    let carried;
    let new = match new {
        RecordIn::Medication(m) if m.decision.is_none() && m.decision_reason.is_none() && m.decision_on.is_none() => {
            let text = |k: &str| old.get(k).and_then(Value::as_str).map(str::to_string);
            let mut m = m.clone();
            m.decision = text("decision");
            m.decision_reason = text("decision_reason");
            m.decision_on = text("decision_on").and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok());
            carried = RecordIn::Medication(m);
            &carried
        }
        other => other,
    };
    let committed = add_record(
        &mut tx,
        new,
        src,
        RecordOptions {
            accepted_by,
            status: "current",
            supersedes: Some(old_id),
        },
    )
    .await?;
    if table == "medication_statement" {
        // the label photos follow the record to its new version
        sqlx::query(
            "insert into medication_label (medication_id, document_id, created_at) \
             select $2, document_id, created_at from medication_label where medication_id = $1",
        )
        .bind(old_id)
        .bind(committed.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(committed)
}

/// Link a stored document (a label photo) to a medicine or supplement version.
pub async fn add_label(conn: &mut PgConnection, medication_id: Uuid, document_id: Uuid) -> Result<()> {
    sqlx::query("insert into medication_label (medication_id, document_id) values ($1, $2) on conflict do nothing")
        .bind(medication_id)
        .bind(document_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// The label photos of each medication version, newest first, leaving out retracted documents:
/// `(medication_id, [document_id])`.
pub async fn labels_of(conn: &mut PgConnection, medication_ids: &[Uuid]) -> Result<Vec<(Uuid, Vec<Uuid>)>> {
    Ok(sqlx::query_as(
        "select l.medication_id, array_agg(l.document_id order by l.created_at desc, l.document_id) \
         from medication_label l join document d on d.id = l.document_id \
         where l.medication_id = any($1) and d.status = 'active' group by l.medication_id",
    )
    .bind(medication_ids)
    .fetch_all(conn)
    .await?)
}

pub async fn retract_record(conn: &mut PgConnection, kind: &str, record_id: Uuid, reason: &str) -> Result<()> {
    lifecycle::retract(conn, table_of(kind)?, record_id, reason).await
}

pub async fn get_record(conn: &mut PgConnection, kind: &str, record_id: Uuid) -> Result<Option<Map<String, Value>>> {
    let row = sqlx::query(AssertSqlSafe(format!(
        "select * from {} where id = $1",
        table_of(kind)?
    )))
    .bind(record_id)
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(pyjson::row_to_json).transpose()
}

pub async fn record_history(conn: &mut PgConnection, kind: &str, record_id: Uuid) -> Result<Vec<Map<String, Value>>> {
    lifecycle::history(conn, table_of(kind)?, record_id).await
}

/// Visible records of one kind for a profile (current view, or as of a past moment).
pub async fn list_records(
    conn: &mut PgConnection,
    kind: &str,
    profile_id: Uuid,
    as_of: Option<DateTime<Utc>>,
    include_sensitive: bool,
) -> Result<Vec<Map<String, Value>>> {
    let sql = format!(
        "select * from {} where profile_id = $1 and {} and ($3 or sensitivity = '{{}}') order by recorded_at desc, id",
        table_of(kind)?,
        lifecycle::visible_sql("", "$2")
    );
    let rows = sqlx::query(AssertSqlSafe(sql.as_str()))
        .bind(profile_id)
        .bind(as_of)
        .bind(include_sensitive)
        .fetch_all(conn)
        .await?;
    rows.iter().map(pyjson::row_to_json).collect()
}

fn date_of(row: &Map<String, Value>, key: &str) -> Option<NaiveDate> {
    row.get(key)
        .and_then(Value::as_str)
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

/// Medications taken on a date: med_status active, or started <= on < stopped.
/// `on` defaults to the date of `as_of`, else today (UTC). `kind`: medication | supplement.
pub async fn active_medications(
    conn: &mut PgConnection,
    profile_id: Uuid,
    on: Option<NaiveDate>,
    as_of: Option<DateTime<Utc>>,
    kind: Option<&str>,
    include_sensitive: bool,
) -> Result<Vec<Map<String, Value>>> {
    let on = on
        .or(as_of.map(|a| a.date_naive()))
        .unwrap_or_else(|| Utc::now().date_naive());
    let rows = list_records(conn, "medication", profile_id, as_of, include_sensitive).await?;
    let mut out: Vec<Map<String, Value>> = rows
        .into_iter()
        .filter(|r| kind.is_none_or(|k| r.get("kind").and_then(Value::as_str) == Some(k)))
        .filter(|r| {
            let started = date_of(r, "started");
            let stopped = date_of(r, "stopped");
            let status = r.get("med_status").and_then(Value::as_str).unwrap_or_default();
            let started_ok = started.is_none_or(|s| s <= on);
            let stopped_ok = stopped.is_none_or(|s| s > on);
            (status == "active" && started_ok && stopped_ok)
                || (status != "on_hold" && started.is_some() && started_ok && stopped.is_some() && stopped_ok)
        })
        .collect();
    let key = |r: &Map<String, Value>| {
        (
            r.get("name").and_then(Value::as_str).unwrap_or_default().to_lowercase(),
            r.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
        )
    };
    out.sort_by_key(key);
    Ok(out)
}
