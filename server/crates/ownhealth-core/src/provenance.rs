//! Provenance: every fact points to one `src` row, and its id is the citation id over MCP
//! (`core/provenance.py`).
//!
//! A document value needs document, page and bbox; an import needs its `import_ref`.
//! `resolve_src` is what both `get_source` (MCP) and `GET /api/sources/{id}` (UI) use.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::AssertSqlSafe;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::ids::{is_src_id, new_src_id};
use crate::pyjson::{py_date, row_to_json};

pub const SRC_KINDS: [&str; 8] = [
    "document_region",
    "fhir_import",
    "apple_health",
    "genome_facts",
    "manual",
    "device",
    "agent_proposal",
    "calculated",
];

/// Tables whose rows carry src_id, in lookup order for `resolve_src`.
pub const FACT_TABLES: [&str; 11] = [
    "observation",
    "condition",
    "medication_statement",
    "allergy",
    "immunization",
    "procedure",
    "encounter",
    "genome_finding",
    "note",
    "imaging_study",
    "genome_dataset",
];

fn system() -> String {
    "system".into()
}

/// A new source. Build it, then `validate()` (the pydantic model validator). Field order matches
/// the Python model, so `serde_json::to_value` equals its `model_dump(mode="json")`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SrcIn {
    pub kind: String,
    #[serde(default)]
    pub profile_id: Option<Uuid>,
    #[serde(default)]
    pub document_id: Option<Uuid>,
    #[serde(default)]
    pub page_no: Option<i32>,
    #[serde(default)]
    pub bbox: Option<[f64; 4]>,
    #[serde(default)]
    pub extraction_run_id: Option<Uuid>,
    #[serde(default)]
    pub candidate_row_id: Option<Uuid>,
    #[serde(default)]
    pub import_ref: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// user | token | system
    #[serde(default = "system")]
    pub actor_kind: String,
    #[serde(default)]
    pub actor_id: Option<Uuid>,
    /// An explicit id (fixtures); otherwise generated.
    #[serde(default)]
    pub id: Option<String>,
}

impl SrcIn {
    pub fn new(kind: &str) -> SrcIn {
        SrcIn {
            kind: kind.into(),
            profile_id: None,
            document_id: None,
            page_no: None,
            bbox: None,
            extraction_run_id: None,
            candidate_row_id: None,
            import_ref: None,
            label: None,
            actor_kind: system(),
            actor_id: None,
            id: None,
        }
    }

    /// A person typed it.
    pub fn manual(user_id: Option<Uuid>) -> SrcIn {
        SrcIn {
            actor_kind: "user".into(),
            actor_id: user_id,
            ..SrcIn::new("manual")
        }
    }

    /// The checks of the pydantic model validator.
    pub fn validate(self) -> Result<SrcIn> {
        if !SRC_KINDS.contains(&self.kind.as_str()) {
            return Err(Error::Invalid(format!("unknown src kind {}", self.kind)));
        }
        if !["user", "token", "system"].contains(&self.actor_kind.as_str()) {
            return Err(Error::Invalid(format!("unknown actor_kind {}", self.actor_kind)));
        }
        if self.kind == "document_region"
            && (self.document_id.is_none() || self.page_no.is_none() || self.bbox.is_none())
        {
            return Err(Error::Provenance(
                "a document_region source needs document_id, page_no and bbox".into(),
            ));
        }
        if ["fhir_import", "apple_health", "genome_facts"].contains(&self.kind.as_str())
            && self.import_ref.as_deref().is_none_or(str::is_empty)
        {
            return Err(Error::Provenance(format!("a {} source needs import_ref", self.kind)));
        }
        if let Some(id) = &self.id
            && !is_src_id(id)
        {
            return Err(Error::Provenance(format!("bad src id {id:?}")));
        }
        Ok(self)
    }
}

/// What a fact cites: an existing src id, or a new source to create.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // built once per fact, never stored in bulk
pub enum SrcRef {
    Id(String),
    New(SrcIn),
}

impl From<SrcIn> for SrcRef {
    fn from(s: SrcIn) -> Self {
        SrcRef::New(s)
    }
}

impl From<&str> for SrcRef {
    fn from(s: &str) -> Self {
        SrcRef::Id(s.into())
    }
}

impl From<String> for SrcRef {
    fn from(s: String) -> Self {
        SrcRef::Id(s)
    }
}

pub async fn create_src(conn: &mut PgConnection, src: &SrcIn) -> Result<String> {
    let src = src.clone().validate()?;
    let sid = src.id.clone().unwrap_or_else(new_src_id);
    let bbox: Option<Vec<f32>> = src.bbox.map(|b| b.iter().map(|v| *v as f32).collect());
    sqlx::query(
        "insert into src (id, profile_id, kind, document_id, page_no, bbox, extraction_run_id, candidate_row_id, \
         import_ref, label, actor_kind, actor_id) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(&sid)
    .bind(src.profile_id)
    .bind(&src.kind)
    .bind(src.document_id)
    .bind(src.page_no)
    .bind(bbox)
    .bind(src.extraction_run_id)
    .bind(src.candidate_row_id)
    .bind(&src.import_ref)
    .bind(&src.label)
    .bind(&src.actor_kind)
    .bind(src.actor_id)
    .execute(conn)
    .await?;
    Ok(sid)
}

/// Accept an existing src id or create one from a `SrcIn`.
///
/// With `profile_id` (the citing fact's profile), an existing src scoped to another profile is
/// refused, so a fact never cites, and `resolve_src` never lists it under, someone else's source.
pub async fn ensure_src(conn: &mut PgConnection, src: &SrcRef, profile_id: Option<Uuid>) -> Result<String> {
    match src {
        SrcRef::Id(id) => {
            let row: Option<(Option<Uuid>,)> = sqlx::query_as("select profile_id from src where id = $1")
                .bind(id)
                .fetch_optional(conn)
                .await?;
            let Some((owner,)) = row else {
                return Err(Error::Provenance(format!("unknown src {id}")));
            };
            if let (Some(want), Some(owner)) = (profile_id, owner)
                && want != owner
            {
                return Err(Error::Provenance(format!("src {id} belongs to another profile")));
            }
            Ok(id.clone())
        }
        SrcRef::New(new) => {
            if let (Some(want), Some(own)) = (profile_id, new.profile_id)
                && want != own
            {
                return Err(Error::Provenance(
                    "a new src belongs to the profile of the fact that cites it".into(),
                ));
            }
            create_src(conn, new).await
        }
    }
}

/// The provenance text of a document value: `"LabNorte · 2026-09-14 · p1"`.
pub async fn region_label(
    conn: &mut PgConnection,
    document_id: Uuid,
    page_no: i32,
    effective_at: Option<&DateTime<FixedOffset>>,
) -> Result<String> {
    let provider: Option<Option<String>> = sqlx::query_scalar("select provider_name from document where id = $1")
        .bind(document_id)
        .fetch_optional(conn)
        .await?;
    let mut parts = vec![
        provider
            .flatten()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "Lab report".into()),
    ];
    if let Some(at) = effective_at {
        parts.push(py_date(&at.date_naive()));
    }
    parts.push(format!("p{page_no}"));
    Ok(parts.join(" · "))
}

pub async fn get_src(conn: &mut PgConnection, src_id: &str) -> Result<Option<Map<String, Value>>> {
    let row = sqlx::query("select * from src where id = $1")
        .bind(src_id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(row_to_json).transpose()
}

/// The src row, its document summary, and the facts that cite it with their lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSrc {
    pub src: Map<String, Value>,
    pub profile_id: Option<Uuid>,
    /// `{table, id, profile_id, status, status_reason}` for every citing row.
    pub facts: Vec<Map<String, Value>>,
    pub document: Option<Map<String, Value>>,
}

fn uuid_of(v: Option<&Value>) -> Option<Uuid> {
    v.and_then(Value::as_str).and_then(|s| Uuid::parse_str(s).ok())
}

/// None for an unknown id. Callers check the profile permission before returning anything, and
/// answer `unknown` for a profile the caller cannot read, so ids leak nothing.
pub async fn resolve_src(conn: &mut PgConnection, src_id: &str) -> Result<Option<ResolvedSrc>> {
    if !is_src_id(src_id) {
        return Ok(None);
    }
    let Some(src) = get_src(&mut *conn, src_id).await? else {
        return Ok(None);
    };
    let mut facts = Vec::new();
    for table in FACT_TABLES {
        let sql = format!("select id, profile_id, status, status_reason from {table} where src_id = $1");
        for r in sqlx::query(AssertSqlSafe(sql.as_str()))
            .bind(src_id)
            .fetch_all(&mut *conn)
            .await?
        {
            let mut m = Map::new();
            m.insert("table".into(), Value::String(table.into()));
            m.extend(row_to_json(&r)?);
            facts.push(m);
        }
    }
    let document = match uuid_of(src.get("document_id")) {
        Some(d) => sqlx::query(
            "select id, profile_id, doc_type, mime, pages, provider_name, report_status, collected_at, reported_at, \
             encode(blob_sha256, 'hex') as sha256, status from document where id = $1",
        )
        .bind(d)
        .fetch_optional(&mut *conn)
        .await?
        .as_ref()
        .map(row_to_json)
        .transpose()?,
        None => None,
    };
    let profile_id = uuid_of(src.get("profile_id"))
        .or_else(|| facts.iter().find_map(|f| uuid_of(f.get("profile_id"))))
        .or_else(|| document.as_ref().and_then(|d| uuid_of(d.get("profile_id"))));
    Ok(Some(ResolvedSrc {
        src,
        profile_id,
        facts,
        document,
    }))
}
