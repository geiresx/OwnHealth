//! Review queue: candidate rows, agent proposals and anything else a person must confirm
//! (`core/review.py`).
//!
//! Importers call `submit_candidate` (document rows) or `route_fact` (structured imports). Both
//! apply the auto-accept rule; a row that passes is committed with accepted_by='auto', anything
//! else becomes an open review item with its reason codes and check results. Nothing is dropped.
//!
//! A person then accepts (optionally with edits), rejects with a reason, reopens, or bulk-accepts
//! the rows of one document that passed every check. Kinds other than candidate rows register an
//! accept handler with `register_accept_handler` (lane F for agent proposals, lane C for FHIR, ...).

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, RwLock};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::acceptance::{Check, DEFAULT_THRESHOLD, auto_accept, failed_reasons};
use crate::error::{Error, Result};
use crate::observations::{CommitOptions, ObservationIn, commit_observation, supersede_observation};
use crate::provenance::{SrcIn, SrcRef, region_label};
use crate::pyjson::row;

pub const REVIEW_KINDS: [&str; 10] = [
    "candidate_row",
    "agent_proposal",
    "fhir_unmapped",
    "document_identity",
    "amended_report",
    "rederive_diff",
    "genome_reinterpretation",
    "job_failed",
    "restore_check_failed",
    "pending_document",
];

/// Edits correct what was read; they never move a fact to another profile, household, document or
/// source. The result is always written where the item is (document_identity names its target and
/// is checked by the route).
pub const FIXED_EDIT_KEYS: [&str; 5] = ["document_id", "household_id", "profile_id", "src", "src_id"];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReviewItemIn {
    pub kind: String,
    pub household_id: Option<Uuid>,
    pub profile_id: Option<Uuid>,
    pub payload: Value,
    pub reasons: Vec<String>,
    pub document_id: Option<Uuid>,
    pub candidate_row_id: Option<Uuid>,
    pub observation_id: Option<Uuid>,
    pub ref_table: Option<String>,
    pub ref_id: Option<Uuid>,
    pub row_conf: Option<f64>,
    pub proposed_by_token: Option<Uuid>,
    pub context_documents: Option<Vec<Uuid>>,
    pub context_src_ids: Option<Vec<String>>,
}

/// A review_item row. `Serialize` gives what FastAPI sent for the row dict.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct ReviewItem {
    pub id: Uuid,
    pub household_id: Option<Uuid>,
    pub profile_id: Option<Uuid>,
    pub kind: String,
    pub reasons: Vec<String>,
    pub document_id: Option<Uuid>,
    pub candidate_row_id: Option<Uuid>,
    pub observation_id: Option<Uuid>,
    pub ref_table: Option<String>,
    pub ref_id: Option<Uuid>,
    pub payload: Value,
    #[serde(serialize_with = "row::opt_f32")]
    pub row_conf: Option<f32>,
    pub proposed_by_token: Option<Uuid>,
    pub context_documents: Option<Vec<Uuid>>,
    pub context_src_ids: Option<Vec<String>>,
    pub status: String,
    pub decided_by: Option<Uuid>,
    #[serde(serialize_with = "row::opt_ts")]
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_reason: Option<String>,
    pub result_ids: Option<Vec<Uuid>>,
    #[serde(serialize_with = "row::ts")]
    pub created_at: DateTime<Utc>,
}

/// One row an extractor read from a page, already normalized into an `ObservationIn`.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateIn {
    pub page_no: i32,
    pub bbox: [f64; 4],
    /// name, value, unit, ref_range, flag: {text, bbox, conf}
    pub cells: Value,
    pub row_conf: f64,
    pub observation: ObservationIn,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteResult {
    /// `auto_accepted` | `in_review`
    pub state: &'static str,
    pub observation_id: Option<Uuid>,
    pub src_id: Option<String>,
    pub review_item_id: Option<Uuid>,
    pub reasons: Vec<String>,
}

/// `fn(conn, item, user_id, edits) -> created row ids` for one review kind.
pub type AcceptHandler =
    for<'c> fn(&'c mut PgConnection, &'c ReviewItem, Uuid, Option<&'c Value>) -> BoxFuture<'c, Result<Vec<Uuid>>>;

static ACCEPT_HANDLERS: LazyLock<RwLock<HashMap<String, AcceptHandler>>> = LazyLock::new(|| {
    let mut m: HashMap<String, AcceptHandler> = HashMap::new();
    m.insert("candidate_row".into(), accept_candidate_boxed);
    RwLock::new(m)
});

/// Register the accept handler of one review kind (lanes call this from their `register()`).
pub fn register_accept_handler(kind: &str, handler: AcceptHandler) {
    if let Ok(mut m) = ACCEPT_HANDLERS.write() {
        m.insert(kind.to_string(), handler);
    }
}

fn accept_handler(kind: &str) -> Option<AcceptHandler> {
    ACCEPT_HANDLERS.read().ok().and_then(|m| m.get(kind).copied())
}

// ------------------------------------------------------------- extraction runs ----

pub async fn create_extraction_run(
    conn: &mut PgConnection,
    document_id: Uuid,
    extractor: &str,
    extractor_version: &str,
    config_hash: &[u8],
    engine: Option<&str>,
    engine_version: Option<&str>,
) -> Result<Uuid> {
    Ok(sqlx::query_scalar(
        "insert into extraction_run (document_id, engine, engine_version, extractor, extractor_version, config_hash) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(document_id)
    .bind(engine)
    .bind(engine_version)
    .bind(extractor)
    .bind(extractor_version)
    .bind(config_hash)
    .fetch_one(conn)
    .await?)
}

/// `status`: done | failed.
pub async fn finish_extraction_run(
    conn: &mut PgConnection,
    run_id: Uuid,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query("update extraction_run set status = $1, error = $2, finished_at = now() where id = $3")
        .bind(status)
        .bind(error)
        .bind(run_id)
        .execute(conn)
        .await?;
    Ok(())
}

// -------------------------------------------------------------------- routing ----

pub async fn create_item(conn: &mut PgConnection, item: &ReviewItemIn) -> Result<Uuid> {
    if !REVIEW_KINDS.contains(&item.kind.as_str()) {
        return Err(Error::Invalid(format!("kind: unknown value {:?}", item.kind)));
    }
    Ok(sqlx::query_scalar(
        "insert into review_item (kind, household_id, profile_id, payload, reasons, document_id, candidate_row_id, \
         observation_id, ref_table, ref_id, row_conf, proposed_by_token, context_documents, context_src_ids) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) returning id",
    )
    .bind(&item.kind)
    .bind(item.household_id)
    .bind(item.profile_id)
    .bind(&item.payload)
    .bind(&item.reasons)
    .bind(item.document_id)
    .bind(item.candidate_row_id)
    .bind(item.observation_id)
    .bind(&item.ref_table)
    .bind(item.ref_id)
    .bind(item.row_conf.map(|c| c as f32))
    .bind(item.proposed_by_token)
    .bind(&item.context_documents)
    .bind(&item.context_src_ids)
    .fetch_one(conn)
    .await?)
}

async fn household_of_profile(conn: &mut PgConnection, profile_id: Uuid) -> Result<Uuid> {
    Ok(sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(profile_id)
        .fetch_one(conn)
        .await?)
}

/// What `route_fact` needs about one fact.
pub struct RouteFact<'a> {
    pub observation: &'a ObservationIn,
    pub src: &'a SrcIn,
    pub checks: &'a [Check],
    pub row_conf: f64,
    /// A `SOURCE_ALLOWANCES` value.
    pub allowance: &'a str,
    /// matched | hinted | mismatch | missing
    pub identity: &'a str,
    pub threshold: f64,
    pub provisional: bool,
    pub candidate_row_id: Option<Uuid>,
    /// Merged into the review payload.
    pub extra_payload: Option<Map<String, Value>>,
}

impl<'a> RouteFact<'a> {
    pub fn new(
        observation: &'a ObservationIn,
        src: &'a SrcIn,
        checks: &'a [Check],
        row_conf: f64,
        allowance: &'a str,
        identity: &'a str,
    ) -> Self {
        RouteFact {
            observation,
            src,
            checks,
            row_conf,
            allowance,
            identity,
            threshold: DEFAULT_THRESHOLD,
            provisional: false,
            candidate_row_id: None,
            extra_payload: None,
        }
    }
}

/// Apply the auto-accept rule to one fact: commit it, or queue it for a person.
pub async fn route_fact(conn: &mut PgConnection, f: RouteFact<'_>) -> Result<RouteResult> {
    let decision = auto_accept(f.row_conf, f.checks, f.allowance, f.identity, f.threshold);
    if decision.auto {
        let status = if f.provisional { "provisional" } else { "current" };
        let committed = commit_observation(
            conn,
            f.observation,
            &SrcRef::New(f.src.clone()),
            CommitOptions {
                status,
                ..CommitOptions::new("auto")
            },
        )
        .await?;
        return Ok(RouteResult {
            state: "auto_accepted",
            observation_id: Some(committed.id),
            src_id: Some(committed.src_id),
            review_item_id: None,
            reasons: Vec::new(),
        });
    }
    let mut payload = Map::new();
    payload.insert("observation".into(), serde_json::to_value(f.observation)?);
    payload.insert("src".into(), serde_json::to_value(f.src)?);
    payload.insert("checks".into(), serde_json::to_value(f.checks)?);
    payload.insert("allowance".into(), json!(f.allowance));
    payload.insert("identity".into(), json!(f.identity));
    payload.insert("row_conf".into(), json!(f.row_conf));
    payload.insert("why_not".into(), json!(decision.why_not));
    payload.insert("provisional".into(), json!(f.provisional));
    if let Some(extra) = f.extra_payload {
        payload.extend(extra);
    }
    let household_id = household_of_profile(&mut *conn, f.observation.profile_id).await?;
    let item_id = create_item(
        conn,
        &ReviewItemIn {
            kind: "candidate_row".into(),
            household_id: Some(household_id),
            profile_id: Some(f.observation.profile_id),
            payload: Value::Object(payload),
            reasons: decision.reasons.clone(),
            document_id: f.observation.document_id,
            candidate_row_id: f.candidate_row_id,
            row_conf: Some(f.row_conf),
            ..ReviewItemIn::default()
        },
    )
    .await?;
    Ok(RouteResult {
        state: "in_review",
        observation_id: None,
        src_id: None,
        review_item_id: Some(item_id),
        reasons: decision.reasons,
    })
}

/// Store a candidate row from a document and route it (auto-accept or review).
pub async fn submit_candidate(
    conn: &mut PgConnection,
    run_id: Uuid,
    cand: &CandidateIn,
    allowance: &str,
    identity: &str,
    threshold: f64,
    provisional: bool,
) -> Result<RouteResult> {
    let document_id: Option<Uuid> = sqlx::query_scalar("select document_id from extraction_run where id = $1")
        .bind(run_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(document_id) = document_id else {
        return Err(Error::Review(format!("unknown extraction run {run_id}")));
    };
    let reasons = failed_reasons(&cand.checks);
    let bbox: Vec<f32> = cand.bbox.iter().map(|v| *v as f32).collect();
    let row_id: Uuid = sqlx::query_scalar(
        "insert into candidate_row (extraction_run_id, page_no, bbox, cells, row_conf, reasons) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(run_id)
    .bind(cand.page_no)
    .bind(&bbox)
    .bind(&cand.cells)
    .bind(cand.row_conf as f32)
    .bind(&reasons)
    .fetch_one(&mut *conn)
    .await?;
    let obs = ObservationIn {
        document_id: Some(document_id),
        candidate_row_id: Some(row_id),
        ..cand.observation.clone()
    };
    let src = SrcIn {
        profile_id: Some(obs.profile_id),
        document_id: Some(document_id),
        page_no: Some(cand.page_no),
        bbox: Some(cand.bbox),
        label: Some(region_label(&mut *conn, document_id, cand.page_no, Some(&obs.effective_at)).await?),
        extraction_run_id: Some(run_id),
        candidate_row_id: Some(row_id),
        ..SrcIn::new("document_region")
    }
    .validate()?;
    let mut extra = Map::new();
    extra.insert("cells".into(), cand.cells.clone());
    extra.insert("page_no".into(), json!(cand.page_no));
    extra.insert("bbox".into(), json!(cand.bbox));
    let result = route_fact(
        &mut *conn,
        RouteFact {
            threshold,
            provisional,
            candidate_row_id: Some(row_id),
            extra_payload: Some(extra),
            ..RouteFact::new(&obs, &src, &cand.checks, cand.row_conf, allowance, identity)
        },
    )
    .await?;
    sqlx::query("update candidate_row set state = $1, observation_id = $2 where id = $3")
        .bind(result.state)
        .bind(result.observation_id)
        .bind(row_id)
        .execute(conn)
        .await?;
    Ok(result)
}

// ---------------------------------------------------------------- decisions ----

pub async fn get_item(conn: &mut PgConnection, item_id: Uuid) -> Result<Option<ReviewItem>> {
    Ok(sqlx::query_as("select * from review_item where id = $1")
        .bind(item_id)
        .fetch_optional(conn)
        .await?)
}

async fn lock_item(conn: &mut PgConnection, item_id: Uuid, want: &str) -> Result<ReviewItem> {
    let item: Option<ReviewItem> = sqlx::query_as("select * from review_item where id = $1 for update")
        .bind(item_id)
        .fetch_optional(conn)
        .await?;
    let item = item.ok_or_else(|| Error::Review(format!("review item {item_id} not found")))?;
    if item.status != want {
        return Err(Error::Review(format!(
            "review item {item_id} is {}, not {want}",
            item.status
        )));
    }
    Ok(item)
}

/// Which parts of a FHIR Observation each column feeds (`fhir::observation` builds them).
fn fhir_parts(column: &str) -> &'static [&'static str] {
    const VALUE_X: &[&str] = &["valueQuantity", "valueCodeableConcept", "valueString"];
    match column {
        "value_num" | "value_cmp" | "value_code" | "value_text" | "printed_value" => VALUE_X,
        "printed_unit" | "unit_ucum" => &["valueQuantity", "valueCodeableConcept", "valueString", "referenceRange"],
        "ref_low" | "ref_high" | "printed_ref_range" => &["referenceRange"],
        "loinc" | "printed_name" => &["code"],
        "effective_at" => &["effectiveDateTime"],
        "interpretation" => &["interpretation"],
        "category" => &["category"],
        _ => &[],
    }
}

/// Keep a received resource, but rebuild the parts an edited column feeds, so the export never
/// disagrees with the stored columns.
fn reconcile_fhir(mut obs: ObservationIn, edited: &BTreeSet<String>) -> ObservationIn {
    let Some(Value::Object(received)) = obs.fhir.clone() else {
        return obs;
    };
    let parts: BTreeSet<&str> = edited.iter().flat_map(|c| fhir_parts(c).iter().copied()).collect();
    if parts.is_empty() {
        return obs;
    }
    let built = crate::fhir::observation(
        &ObservationIn {
            fhir: None,
            ..obs.clone()
        },
        "",
    );
    let mut resource: Map<String, Value> = received
        .into_iter()
        .filter(|(k, _)| !parts.contains(k.as_str()))
        .collect();
    for part in &parts {
        if let Some(v) = built.get(*part) {
            resource.insert((*part).to_string(), v.clone());
        }
    }
    obs.fhir = Some(Value::Object(resource));
    obs
}

fn accept_candidate_boxed<'c>(
    conn: &'c mut PgConnection,
    item: &'c ReviewItem,
    user_id: Uuid,
    edits: Option<&'c Value>,
) -> BoxFuture<'c, Result<Vec<Uuid>>> {
    Box::pin(accept_candidate(conn, item, user_id, edits))
}

async fn accept_candidate(
    conn: &mut PgConnection,
    item: &ReviewItem,
    user_id: Uuid,
    edits: Option<&Value>,
) -> Result<Vec<Uuid>> {
    let payload = &item.payload;
    let received = payload
        .get("observation")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // The resource is what arrived; a reviewer edits columns, never the resource itself.
    let edits: Map<String, Value> = edits
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| k.as_str() != "fhir")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut data = received.clone();
    data.extend(edits.clone());
    let obs = ObservationIn::from_json(Value::Object(data))?;
    let edited: BTreeSet<String> = edits
        .iter()
        .filter(|(k, v)| received.get(k.as_str()) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    let obs = reconcile_fhir(obs, &edited);
    let mut src_json = payload
        .get("src")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    src_json.insert("actor_kind".into(), json!("user"));
    src_json.insert("actor_id".into(), json!(user_id.to_string()));
    let src: SrcIn = serde_json::from_value(Value::Object(src_json)).map_err(|e| Error::Invalid(e.to_string()))?;
    let src = SrcRef::New(src.validate()?);
    let mut committed = None;
    if let Some(old) = payload
        .get("supersedes")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        // A changed upstream version replaces the value it corrects; if that one left the current
        // view meanwhile (retracted), the correction is committed as a new fact.
        let old = Uuid::parse_str(old).map_err(|e| Error::Invalid(e.to_string()))?;
        match supersede_observation(
            &mut *conn,
            old,
            &obs,
            &src,
            "person",
            "updated at the source",
            Some(user_id),
        )
        .await
        {
            Ok(c) => committed = Some(c),
            Err(Error::Lifecycle(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let committed = match committed {
        Some(c) => c,
        None => {
            let provisional = payload.get("provisional").and_then(Value::as_bool).unwrap_or(false);
            let status = if provisional { "provisional" } else { "current" };
            commit_observation(
                &mut *conn,
                &obs,
                &src,
                CommitOptions {
                    status,
                    reviewed_by: Some(user_id),
                    ..CommitOptions::new("person")
                },
            )
            .await?
        }
    };
    if let Some(row_id) = item.candidate_row_id {
        sqlx::query("update candidate_row set state = 'confirmed', observation_id = $1 where id = $2")
            .bind(committed.id)
            .bind(row_id)
            .execute(conn)
            .await?;
    }
    Ok(vec![committed.id])
}

/// Accept an open item (with optional edits). Returns the ids of the rows it created.
pub async fn accept(conn: &mut PgConnection, item_id: Uuid, user_id: Uuid, edits: Option<&Value>) -> Result<Vec<Uuid>> {
    let item = lock_item(&mut *conn, item_id, "open").await?;
    let handler = accept_handler(&item.kind).ok_or_else(|| {
        Error::Review(format!(
            "items of kind {} cannot be accepted; dismiss them instead",
            item.kind
        ))
    })?;
    let edits = edits.filter(|e| !e.is_null() && e.as_object().is_none_or(|m| !m.is_empty()));
    if item.kind != "document_identity"
        && let Some(Value::Object(e)) = edits
    {
        let nested: Vec<&String> = e
            .get("payload")
            .and_then(Value::as_object)
            .map(|m| m.keys().collect())
            .unwrap_or_default();
        let keys: BTreeSet<&str> = e.keys().chain(nested).map(String::as_str).collect();
        let fixed: Vec<&str> = FIXED_EDIT_KEYS.iter().copied().filter(|k| keys.contains(k)).collect();
        if !fixed.is_empty() {
            return Err(Error::Review(format!("edits cannot change {}", fixed.join(", "))));
        }
    }
    let created = handler(&mut *conn, &item, user_id, edits).await?;
    sqlx::query("update review_item set status = 'accepted', decided_by = $1, decided_at = now(), result_ids = $2 where id = $3")
        .bind(user_id)
        .bind(&created)
        .bind(item_id)
        .execute(conn)
        .await?;
    Ok(created)
}

/// A rejected row never becomes a fact; the item keeps what was read, so it can be reopened.
pub async fn reject(conn: &mut PgConnection, item_id: Uuid, user_id: Uuid, reason: &str) -> Result<()> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(Error::Review("a rejection needs a reason".into()));
    }
    let item = lock_item(&mut *conn, item_id, "open").await?;
    sqlx::query(
        "update review_item set status = 'rejected', decided_by = $1, decided_at = now(), decision_reason = $2 where id = $3",
    )
    .bind(user_id)
    .bind(reason)
    .bind(item_id)
    .execute(&mut *conn)
    .await?;
    if let Some(row_id) = item.candidate_row_id {
        sqlx::query("update candidate_row set state = 'rejected' where id = $1")
            .bind(row_id)
            .execute(conn)
            .await?;
    }
    Ok(())
}

pub async fn reopen(conn: &mut PgConnection, item_id: Uuid, _user_id: Uuid) -> Result<()> {
    let item = lock_item(&mut *conn, item_id, "rejected").await?;
    sqlx::query(
        "update review_item set status = 'open', decided_by = null, decided_at = null, decision_reason = null where id = $1",
    )
    .bind(item_id)
    .execute(&mut *conn)
    .await?;
    if let Some(row_id) = item.candidate_row_id {
        sqlx::query("update candidate_row set state = 'in_review' where id = $1")
            .bind(row_id)
            .execute(conn)
            .await?;
    }
    Ok(())
}

/// For informational items (job_failed, restore_check_failed, rederive_diff).
pub async fn dismiss(conn: &mut PgConnection, item_id: Uuid, user_id: Uuid, reason: Option<&str>) -> Result<()> {
    lock_item(&mut *conn, item_id, "open").await?;
    sqlx::query(
        "update review_item set status = 'dismissed', decided_by = $1, decided_at = now(), decision_reason = $2 where id = $3",
    )
    .bind(user_id)
    .bind(reason)
    .bind(item_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Accept every open candidate row of a document that passed every check and has
/// row_conf >= min_row_conf. Rows with any reason code stay open. Returns accepted item ids.
pub async fn bulk_accept(
    conn: &mut PgConnection,
    document_id: Uuid,
    user_id: Uuid,
    min_row_conf: f64,
) -> Result<Vec<Uuid>> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select id from review_item where document_id = $1 and kind = 'candidate_row' and status = 'open' \
         and reasons = '{}' and coalesce(row_conf, 0) >= $2 order by created_at, id",
    )
    .bind(document_id)
    .bind(min_row_conf)
    .fetch_all(&mut *conn)
    .await?;
    for id in &ids {
        accept(&mut *conn, *id, user_id, None).await?;
    }
    Ok(ids)
}

/// The filters of `list_items`. `profile_ids: Some(..)` keeps those profiles and household items
/// (profile_id null); `status: None` means every status.
#[derive(Debug, Clone)]
pub struct ItemQuery {
    pub household_id: Option<Uuid>,
    pub profile_ids: Option<Vec<Uuid>>,
    pub status: Option<String>,
    pub kind: Option<String>,
    pub document_id: Option<Uuid>,
    pub limit: i64,
    pub offset: i64,
}

impl Default for ItemQuery {
    fn default() -> Self {
        ItemQuery {
            household_id: None,
            profile_ids: None,
            status: Some("open".into()),
            kind: None,
            document_id: None,
            limit: 100,
            offset: 0,
        }
    }
}

pub async fn list_items(conn: &mut PgConnection, q: &ItemQuery) -> Result<Vec<ReviewItem>> {
    Ok(sqlx::query_as(
        "select * from review_item where ($1::uuid is null or household_id = $1) \
         and ($2::uuid[] is null or profile_id = any($2) or profile_id is null) \
         and ($3::text is null or status = $3) and ($4::text is null or kind = $4) \
         and ($5::uuid is null or document_id = $5) order by created_at, id limit $6 offset $7",
    )
    .bind(q.household_id)
    .bind(&q.profile_ids)
    .bind(q.status.as_deref().filter(|s| !s.is_empty()))
    .bind(q.kind.as_deref().filter(|s| !s.is_empty()))
    .bind(q.document_id)
    .bind(q.limit)
    .bind(q.offset)
    .fetch_all(conn)
    .await?)
}

pub async fn count_open(conn: &mut PgConnection, household_id: Uuid, profile_ids: Option<&[Uuid]>) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from review_item where household_id = $1 and status = 'open' \
         and ($2::uuid[] is null or profile_id = any($2) or profile_id is null)",
    )
    .bind(household_id)
    .bind(profile_ids)
    .fetch_one(conn)
    .await?)
}
