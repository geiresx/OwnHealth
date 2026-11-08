//! Observation commit API: insert, supersede, retract, confirm, query (`core/observations.py`).
//!
//! Every observation needs a source (`SrcIn` or an existing src id). The printed fields are the
//! truth layer and never change; the derived fields (loinc, unit_ucum, value_canonical,
//! interpretation) come from the normalizer (lane A) and carry `derivation_version`.

use chrono::{DateTime, FixedOffset, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::AssertSqlSafe;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::lifecycle;
use crate::provenance::{SrcRef, ensure_src};
use crate::pyjson::{self, row};

pub const CATEGORIES: [&str; 5] = ["laboratory", "vital-signs", "survey", "exam", "activity"];
pub const SOURCE_KINDS: [&str; 5] = ["clinical", "consumer", "device", "self_reported", "agent_written"];
pub const ACCEPTED_BY: [&str; 3] = ["auto", "person", "import"];
pub const SENSITIVITY: [&str; 3] = ["mental_health", "reproductive", "sexual_health"];
pub const COMPARATORS: [&str; 4] = ["<", "<=", ">", ">="];
pub const INTERPRETATIONS: [&str; 6] = ["L", "N", "H", "LL", "HH", "A"];

fn laboratory() -> String {
    "laboratory".into()
}

fn clinical() -> String {
    "clinical".into()
}

/// One observation to commit. Field order is the pydantic model's, so `serde_json::to_value`
/// equals `ObservationIn.model_dump(mode="json")` (decimals as strings, datetimes with `Z`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationIn {
    pub profile_id: Uuid,
    #[serde(default = "laboratory")]
    pub category: String,
    #[serde(default = "clinical")]
    pub source_kind: String,
    // truth layer
    pub printed_name: String,
    pub printed_value: String,
    #[serde(default)]
    pub printed_unit: Option<String>,
    #[serde(default)]
    pub printed_ref_range: Option<String>,
    #[serde(default)]
    pub printed_flag: Option<String>,
    // parsed
    #[serde(default, with = "pyjson::opt_decimal")]
    pub value_num: Option<Decimal>,
    #[serde(default)]
    pub value_cmp: Option<String>,
    #[serde(default)]
    pub value_text: Option<String>,
    #[serde(default)]
    pub value_code: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default, with = "pyjson::opt_decimal")]
    pub ref_low: Option<Decimal>,
    #[serde(default, with = "pyjson::opt_decimal")]
    pub ref_high: Option<Decimal>,
    #[serde(default)]
    pub ref_applies: Option<Value>,
    #[serde(default)]
    pub specimen: Option<String>,
    // derived
    #[serde(default)]
    pub loinc: Option<String>,
    pub alias_group: String,
    #[serde(default)]
    pub unit_ucum: Option<String>,
    #[serde(default, with = "pyjson::opt_decimal")]
    pub value_canonical: Option<Decimal>,
    #[serde(default)]
    pub unit_canonical: Option<String>,
    #[serde(default)]
    pub interpretation: Option<String>,
    pub derivation_version: String,
    // time and links
    #[serde(with = "pyjson::datetime")]
    pub effective_at: DateTime<FixedOffset>,
    #[serde(default)]
    pub document_id: Option<Uuid>,
    #[serde(default)]
    pub candidate_row_id: Option<Uuid>,
    #[serde(default)]
    pub report_key: Option<String>,
    #[serde(default)]
    pub sensitivity: Vec<String>,
    /// The received resource for imports; built from the columns otherwise.
    #[serde(default)]
    pub fhir: Option<Value>,
}

impl ObservationIn {
    /// The Literal checks of the pydantic model.
    pub fn validate(&self) -> Result<()> {
        let bad = |field: &str, v: &str| Err(Error::Invalid(format!("{field}: unknown value {v:?}")));
        if !CATEGORIES.contains(&self.category.as_str()) {
            return bad("category", &self.category);
        }
        if !SOURCE_KINDS.contains(&self.source_kind.as_str()) {
            return bad("source_kind", &self.source_kind);
        }
        if let Some(c) = &self.value_cmp
            && !COMPARATORS.contains(&c.as_str())
        {
            return bad("value_cmp", c);
        }
        if let Some(i) = &self.interpretation
            && !INTERPRETATIONS.contains(&i.as_str())
        {
            return bad("interpretation", i);
        }
        if let Some(s) = self.sensitivity.iter().find(|s| !SENSITIVITY.contains(&s.as_str())) {
            return bad("sensitivity", s);
        }
        Ok(())
    }

    /// `ObservationIn.model_validate(dict)`.
    pub fn from_json(v: Value) -> Result<ObservationIn> {
        let obs: ObservationIn = serde_json::from_value(v).map_err(|e| Error::Invalid(e.to_string()))?;
        obs.validate()?;
        Ok(obs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub id: Uuid,
    pub src_id: String,
}

/// An observation row as stored. `Serialize` gives the JSON FastAPI sent for the row dict
/// (decimals per `decimal_json`, timestamps per `py_isoformat`).
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Observation {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub category: String,
    pub document_id: Option<Uuid>,
    pub candidate_row_id: Option<Uuid>,
    pub source_kind: String,
    pub printed_name: String,
    pub printed_value: String,
    pub printed_unit: Option<String>,
    pub printed_ref_range: Option<String>,
    pub printed_flag: Option<String>,
    #[serde(serialize_with = "row::decimal")]
    pub value_num: Option<Decimal>,
    pub value_cmp: Option<String>,
    pub value_text: Option<String>,
    pub value_code: Option<String>,
    pub method: Option<String>,
    #[serde(serialize_with = "row::decimal")]
    pub ref_low: Option<Decimal>,
    #[serde(serialize_with = "row::decimal")]
    pub ref_high: Option<Decimal>,
    pub ref_applies: Option<Value>,
    pub specimen: Option<String>,
    pub loinc: Option<String>,
    pub alias_group: String,
    pub unit_ucum: Option<String>,
    #[serde(serialize_with = "row::decimal")]
    pub value_canonical: Option<Decimal>,
    pub unit_canonical: Option<String>,
    pub interpretation: Option<String>,
    pub derivation_version: String,
    #[serde(serialize_with = "row::ts")]
    pub effective_at: DateTime<Utc>,
    #[serde(serialize_with = "row::ts")]
    pub recorded_at: DateTime<Utc>,
    pub src_id: String,
    pub report_key: Option<String>,
    pub accepted_by: String,
    pub reviewed_by: Option<Uuid>,
    #[serde(serialize_with = "row::opt_ts")]
    pub reviewed_at: Option<DateTime<Utc>>,
    pub status: String,
    pub status_reason: Option<String>,
    pub supersedes: Option<Uuid>,
    #[serde(serialize_with = "row::opt_ts")]
    pub superseded_at: Option<DateTime<Utc>>,
    pub sensitivity: Vec<String>,
    pub fhir: Value,
}

/// How a fact was accepted: `auto` (the auto-accept rule), `person` (a click or manual entry), `import`.
/// `status`: `current` or `provisional`.
pub struct CommitOptions<'a> {
    pub accepted_by: &'a str,
    pub status: &'a str,
    pub reviewed_by: Option<Uuid>,
    pub supersedes: Option<Uuid>,
}

impl<'a> CommitOptions<'a> {
    pub fn new(accepted_by: &'a str) -> Self {
        CommitOptions {
            accepted_by,
            status: "current",
            reviewed_by: None,
            supersedes: None,
        }
    }
}

/// Insert one observation with its provenance. Use `supersede_observation` for corrections.
pub async fn commit_observation(
    conn: &mut PgConnection,
    obs: &ObservationIn,
    src: &SrcRef,
    opts: CommitOptions<'_>,
) -> Result<Committed> {
    obs.validate()?;
    if !ACCEPTED_BY.contains(&opts.accepted_by) {
        return Err(Error::Invalid(format!(
            "accepted_by: unknown value {:?}",
            opts.accepted_by
        )));
    }
    if opts.status != "current" && opts.status != "provisional" {
        return Err(Error::Invalid(format!("status: unknown value {:?}", opts.status)));
    }
    let src = match src {
        SrcRef::New(s) if s.profile_id.is_none() => SrcRef::New(crate::provenance::SrcIn {
            profile_id: Some(obs.profile_id),
            ..s.clone()
        }),
        other => other.clone(),
    };
    let src_id = ensure_src(&mut *conn, &src, Some(obs.profile_id)).await?;
    let resource = obs
        .fhir
        .clone()
        .unwrap_or_else(|| crate::fhir::observation(obs, opts.status));
    let id: Uuid = sqlx::query_scalar(
        "insert into observation (profile_id, category, source_kind, printed_name, printed_value, printed_unit, \
         printed_ref_range, printed_flag, value_num, value_cmp, value_text, value_code, method, ref_low, ref_high, \
         ref_applies, specimen, loinc, alias_group, unit_ucum, value_canonical, unit_canonical, interpretation, \
         derivation_version, effective_at, document_id, candidate_row_id, report_key, sensitivity, src_id, \
         accepted_by, status, reviewed_by, supersedes, fhir, reviewed_at) values ($1, $2, $3, $4, $5, $6, $7, $8, \
         $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, \
         $30, $31, $32, $33, $34, $35, case when $33::uuid is null then null else now() end) returning id",
    )
    .bind(obs.profile_id)
    .bind(&obs.category)
    .bind(&obs.source_kind)
    .bind(&obs.printed_name)
    .bind(&obs.printed_value)
    .bind(&obs.printed_unit)
    .bind(&obs.printed_ref_range)
    .bind(&obs.printed_flag)
    .bind(obs.value_num)
    .bind(&obs.value_cmp)
    .bind(&obs.value_text)
    .bind(&obs.value_code)
    .bind(&obs.method)
    .bind(obs.ref_low)
    .bind(obs.ref_high)
    .bind(&obs.ref_applies)
    .bind(&obs.specimen)
    .bind(&obs.loinc)
    .bind(&obs.alias_group)
    .bind(&obs.unit_ucum)
    .bind(obs.value_canonical)
    .bind(&obs.unit_canonical)
    .bind(&obs.interpretation)
    .bind(&obs.derivation_version)
    .bind(obs.effective_at)
    .bind(obs.document_id)
    .bind(obs.candidate_row_id)
    .bind(&obs.report_key)
    .bind(&obs.sensitivity)
    .bind(&src_id)
    .bind(opts.accepted_by)
    .bind(opts.status)
    .bind(opts.reviewed_by)
    .bind(opts.supersedes)
    .bind(&resource)
    .fetch_one(conn)
    .await?;
    Ok(Committed { id, src_id })
}

/// A correction or an amended report: the new row replaces the old, which becomes superseded.
pub async fn supersede_observation(
    conn: &mut PgConnection,
    old_id: Uuid,
    new: &ObservationIn,
    src: &SrcRef,
    accepted_by: &str,
    reason: &str,
    reviewed_by: Option<Uuid>,
) -> Result<Committed> {
    let old = lifecycle::lock_current(&mut *conn, "observation", old_id).await?;
    if old.get("profile_id").and_then(Value::as_str) != Some(new.profile_id.to_string().as_str()) {
        return Err(Error::Lifecycle(
            "a correction cannot move a fact to another profile".into(),
        ));
    }
    lifecycle::mark_superseded(&mut *conn, "observation", old_id, Some(reason)).await?;
    commit_observation(
        conn,
        new,
        src,
        CommitOptions {
            accepted_by,
            status: "current",
            reviewed_by,
            supersedes: Some(old_id),
        },
    )
    .await
}

pub async fn retract_observation(conn: &mut PgConnection, obs_id: Uuid, reason: &str) -> Result<()> {
    lifecycle::retract(conn, "observation", obs_id, reason).await
}

pub async fn confirm_observation(conn: &mut PgConnection, obs_id: Uuid) -> Result<()> {
    lifecycle::confirm_provisional(conn, "observation", obs_id).await
}

pub async fn get_observation(conn: &mut PgConnection, obs_id: Uuid) -> Result<Option<Observation>> {
    Ok(sqlx::query_as("select * from observation where id = $1")
        .bind(obs_id)
        .fetch_optional(conn)
        .await?)
}

pub async fn observation_history(conn: &mut PgConnection, obs_id: Uuid) -> Result<Vec<Map<String, Value>>> {
    lifecycle::history(conn, "observation", obs_id).await
}

/// The filters of `list_observations`. Empty strings and lists filter nothing, as in Python.
#[derive(Debug, Clone, Default)]
pub struct ObservationQuery {
    pub alias_group: Option<String>,
    pub loinc: Vec<String>,
    pub category: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    /// What the store knew then (the current view when None).
    pub as_of: Option<DateTime<Utc>>,
    /// Callers that serve a token without read:sensitive pass false and count the withheld rows
    /// themselves.
    pub exclude_sensitive: bool,
    pub limit: Option<i64>,
}

/// Visible observations for one profile, newest first.
pub async fn list_observations(
    conn: &mut PgConnection,
    profile_id: Uuid,
    q: &ObservationQuery,
) -> Result<Vec<Observation>> {
    let sql = format!(
        "select * from observation where profile_id = $1 and {} \
         and ($3::text is null or alias_group = $3) and ($4::text[] is null or loinc = any($4)) \
         and ($5::text is null or category = $5) and ($6::timestamptz is null or effective_at >= $6) \
         and ($7::timestamptz is null or effective_at <= $7) and ($2::timestamptz is null or effective_at <= $2) \
         and (not $8 or sensitivity = '{{}}') order by effective_at desc, id limit $9",
        lifecycle::visible_sql("", "$2")
    );
    let loinc = (!q.loinc.is_empty()).then_some(&q.loinc);
    Ok(sqlx::query_as(AssertSqlSafe(sql.as_str()))
        .bind(profile_id)
        .bind(q.as_of)
        .bind(q.alias_group.as_deref().filter(|s| !s.is_empty()))
        .bind(loinc)
        .bind(q.category.as_deref().filter(|s| !s.is_empty()))
        .bind(q.since)
        .bind(q.until)
        .bind(q.exclude_sensitive)
        .bind(q.limit.filter(|l| *l != 0))
        .fetch_all(conn)
        .await?)
}

/// The newest visible observation per alias_group.
pub async fn latest_per_group(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: Option<DateTime<Utc>>,
    category: Option<&str>,
) -> Result<Vec<Observation>> {
    let sql = format!(
        "select distinct on (alias_group) * from observation where profile_id = $1 and {} \
         and ($3::text is null or category = $3) and ($2::timestamptz is null or effective_at <= $2) \
         order by alias_group, effective_at desc, recorded_at desc",
        lifecycle::visible_sql("", "$2")
    );
    Ok(sqlx::query_as(AssertSqlSafe(sql.as_str()))
        .bind(profile_id)
        .bind(as_of)
        .bind(category.filter(|c| !c.is_empty()))
        .fetch_all(conn)
        .await?)
}
