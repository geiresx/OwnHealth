//! Records: conditions, medications (and supplements), allergies, immunizations, procedures
//! (`core/records.py`).
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
pub const KINDS: [(&str, &str); 5] = [
    ("condition", "condition"),
    ("medication", "medication_statement"),
    ("allergy", "allergy"),
    ("immunization", "immunization"),
    ("procedure", "procedure"),
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

#[derive(Debug, Clone, PartialEq)]
pub enum RecordIn {
    Condition(ConditionIn),
    Medication(MedicationIn),
    Allergy(AllergyIn),
    Immunization(ImmunizationIn),
    Procedure(ProcedureIn),
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
        }
    }

    pub fn table(&self) -> &'static str {
        match self {
            RecordIn::Medication(_) => "medication_statement",
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
                check("med_status", &m.med_status, &["active", "stopped", "on_hold"])
            }
            RecordIn::Allergy(a) => match &a.criticality {
                Some(c) => check("criticality", c, &["low", "high", "unable-to-assess"]),
                None => Ok(()),
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
        "effective_at" => "timestamptz",
        "sensitivity" => "text[]",
        "onset" | "abatement" | "started" | "stopped" | "occurred" | "performed" => "date",
        "dose_number" => "int",
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
    let resource = record.common().fhir.clone().unwrap_or_else(|| record.build_fhir());
    let cols = record.columns()?;
    // One jsonb parameter holds the typed columns; each is read back with its SQL type.
    let names: Vec<&String> = cols.keys().collect();
    let values: Vec<String> = names
        .iter()
        .map(|c| match sql_type(c) {
            "text[]" => format!("array(select jsonb_array_elements_text($1->'{c}'))"),
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
    let old = lifecycle::lock_current(&mut *conn, table, old_id).await?;
    if old.get("profile_id").and_then(Value::as_str) != Some(new.common().profile_id.to_string().as_str()) {
        return Err(Error::Lifecycle(
            "a correction cannot move a record to another profile".into(),
        ));
    }
    lifecycle::mark_superseded(&mut *conn, table, old_id, Some(reason)).await?;
    add_record(
        conn,
        new,
        src,
        RecordOptions {
            accepted_by,
            status: "current",
            supersedes: Some(old_id),
        },
    )
    .await
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
