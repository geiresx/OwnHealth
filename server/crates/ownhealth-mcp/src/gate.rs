//! What one caller may see on one profile, and a last pass over every response that drops any cited
//! fact the caller cannot read, whoever built the response (`mcp/gate.py`).
//!
//! ```text
//! effective scopes = token.scopes ∩ grant.categories − profile.locks   (core::access)
//! ```
//!
//! A fact is hidden when its table needs a scope the caller lacks, when it is sensitive and the
//! caller lacks read:sensitive, when it is a sensitive genome finding and the profile's agents switch
//! is off, when the minor rule withholds it, or when it belongs to another profile. Hidden facts are
//! counted in `withheld`, never described.

use std::collections::{BTreeSet, HashMap};

use chrono::NaiveDate;
use ownhealth_core::Result;
use ownhealth_core::access::Permission;
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::envelope::src_values;

pub fn obs_category_scope(category: Option<&str>) -> &'static str {
    match category {
        Some("vital-signs" | "survey" | "exam") => "read:vitals",
        Some("activity") => "read:wearables",
        _ => "read:labs",
    }
}

const RECORD_TABLES: [&str; 7] = [
    "condition",
    "medication_statement",
    "allergy",
    "immunization",
    "procedure",
    "encounter",
    "imaging_study",
];

pub const READ_SCOPES: [&str; 6] = [
    "read:summary",
    "read:labs",
    "read:vitals",
    "read:wearables",
    "read:records",
    "read:genome",
];

/// Every category a document can carry: an unextracted page can hold any of them (mcp.md 5.7).
const PAGE_ALL_SCOPES: [&str; 5] = [
    "read:labs",
    "read:vitals",
    "read:records",
    "read:genome",
    "read:sensitive",
];

/// 'read:labs' -> 'labs', the word used in withheld.by_scope.
pub fn scope_label(scope: &str) -> String {
    scope.split_once(':').map_or(scope, |(_, l)| l).to_string()
}

/// The genome gate of one caller on one profile (Python `genome.Gate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenomeGate {
    pub include_sensitive: bool,
    pub minor: bool,
}

/// Sensitive genome facts need the agents switch, read:genome and read:sensitive (mcp.md 9.7).
pub fn genome_gate(perm: &Permission, as_of: NaiveDate, age_of_majority: i32) -> GenomeGate {
    GenomeGate {
        include_sensitive: perm.profile.genome_sensitive_agents
            && perm.has("read:genome")
            && perm.has("read:sensitive"),
        minor: perm.profile.is_minor(as_of, age_of_majority),
    }
}

/// The same gate in lane D's shape.
pub fn lane_gate(perm: &Permission, gate: GenomeGate) -> ownhealth_genome::Gate {
    ownhealth_genome::Gate {
        surface: "mcp".into(),
        genome: perm.has("read:genome"),
        raw: perm.has("read:genome_raw"),
        sensitive: gate.include_sensitive,
        minor: gate.minor,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    Scope(String),
    Sensitive,
    Minor,
    Foreign,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Withheld {
    pub sensitive: i64,
    pub minor_rule: i64,
    pub by_scope: BTreeSet<String>,
}

impl Withheld {
    pub fn merge(&mut self, other: Option<&Value>) {
        let Some(o) = other else { return };
        let num = |k: &str| match o.get(k) {
            Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
            Some(Value::Bool(b)) => i64::from(*b),
            _ => 0,
        };
        self.sensitive += num("sensitive");
        self.minor_rule += num("minor_rule");
        if let Some(a) = o.get("by_scope").and_then(Value::as_array) {
            self.by_scope
                .extend(a.iter().filter_map(|v| v.as_str().map(str::to_string)));
        }
    }

    pub fn as_map(&self) -> Map<String, Value> {
        let Value::Object(m) =
            json!({"sensitive": self.sensitive, "by_scope": self.by_scope, "minor_rule": self.minor_rule})
        else {
            return Map::new();
        };
        m
    }
}

/// src id -> why the caller may not see it. Visible and unknown ids are absent.
pub async fn hidden_reasons(
    conn: &mut PgConnection,
    perm: &Permission,
    gate: GenomeGate,
    src_ids: &[String],
) -> Result<HashMap<String, Why>> {
    let mut out: HashMap<String, Why> = HashMap::new();
    if src_ids.is_empty() {
        return Ok(out);
    }
    let pid = perm.profile.id;
    let mut ids: Vec<String> = Vec::new();
    for s in src_ids {
        if !ids.contains(s) {
            ids.push(s.clone());
        }
    }
    // the first reason wins, except that a foreign profile always wins
    fn hide(out: &mut HashMap<String, Why>, sid: &str, why: Why) {
        if !out.contains_key(sid) || why == Why::Foreign {
            out.insert(sid.to_string(), why);
        }
    }
    let need = |out: &mut HashMap<String, Why>, sid: &str, scope: &str| {
        if !perm.scopes.contains(scope) {
            hide(out, sid, Why::Scope(scope_label(scope)));
        }
    };
    let sensitive_ok = perm.scopes.contains("read:sensitive");
    let foreign = |p: Option<Uuid>| p.is_some_and(|p| p != pid);

    let rows: Vec<(String, String, Option<Uuid>)> =
        sqlx::query_as("select id, kind, profile_id from src where id = any($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
    for (id, kind, profile_id) in rows {
        if foreign(profile_id) {
            hide(&mut out, &id, Why::Foreign);
        }
        if kind == "genome_facts" {
            need(&mut out, &id, "read:genome");
        }
    }

    let rows: Vec<(String, Uuid, String, Vec<String>)> =
        sqlx::query_as("select src_id, profile_id, category, sensitivity from observation where src_id = any($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
    for (sid, profile_id, category, sensitivity) in rows {
        if profile_id != pid {
            hide(&mut out, &sid, Why::Foreign);
        }
        need(&mut out, &sid, obs_category_scope(Some(&category)));
        if !sensitivity.is_empty() && !sensitive_ok {
            hide(&mut out, &sid, Why::Sensitive);
        }
    }

    for table in RECORD_TABLES {
        let sens = if table == "imaging_study" {
            "'{}'::text[] as sensitivity"
        } else {
            "sensitivity"
        };
        let sql = format!("select src_id, profile_id, {sens} from {table} where src_id = any($1)");
        let rows: Vec<(String, Uuid, Vec<String>)> = sqlx::query_as(AssertSqlSafe(sql.as_str()))
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
        for (sid, profile_id, sensitivity) in rows {
            if profile_id != pid {
                hide(&mut out, &sid, Why::Foreign);
            }
            need(&mut out, &sid, "read:records");
            if !sensitivity.is_empty() && !sensitive_ok {
                hide(&mut out, &sid, Why::Sensitive);
            }
        }
    }

    let rows: Vec<(String, Uuid, Vec<String>)> =
        sqlx::query_as("select src_id, profile_id, sensitivity from note where src_id = any($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
    for (sid, profile_id, sensitivity) in rows {
        if profile_id != pid {
            hide(&mut out, &sid, Why::Foreign);
        }
        need(&mut out, &sid, "read:summary");
        if !sensitivity.is_empty() && !sensitive_ok {
            hide(&mut out, &sid, Why::Sensitive);
        }
    }

    let rows: Vec<(String, Uuid, bool, bool, bool)> = sqlx::query_as(
        "select src_id, profile_id, sensitive, adult_onset, carrier from genome_finding where src_id = any($1)",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    for (sid, profile_id, sensitive, adult_onset, carrier) in rows {
        if profile_id != pid {
            hide(&mut out, &sid, Why::Foreign);
        }
        need(&mut out, &sid, "read:genome");
        if sensitive && !gate.include_sensitive {
            hide(&mut out, &sid, Why::Sensitive);
        }
        if gate.minor && (adult_onset || sensitive || carrier) {
            hide(&mut out, &sid, Why::Minor);
        }
    }

    let rows: Vec<(String, Uuid)> =
        sqlx::query_as("select src_id, profile_id from genome_dataset where src_id = any($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
    for (sid, profile_id) in rows {
        if profile_id != pid {
            hide(&mut out, &sid, Why::Foreign);
        }
        need(&mut out, &sid, "read:genome");
    }
    Ok(out)
}

fn cites<'h>(node: &Value, hidden: &'h HashMap<String, Why>) -> Option<&'h Why> {
    let src = node.as_object()?.get("src")?;
    src_values(src).iter().find_map(|s| hidden.get(s))
}

fn count(why: &Why, withheld: &mut Withheld) {
    match why {
        Why::Scope(l) => {
            withheld.by_scope.insert(l.clone());
        }
        Why::Sensitive => withheld.sensitive += 1,
        Why::Minor => withheld.minor_rule += 1,
        // a foreign fact is dropped and not counted: its existence is not this profile's business
        Why::Foreign => {}
    }
}

fn go(node: &Value, hidden: &HashMap<String, Why>, withheld: &mut Withheld) -> Value {
    match node {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(why) = cites(item, hidden) {
                    count(why, withheld);
                    continue;
                }
                out.push(go(item, hidden, withheld));
            }
            Value::Array(out)
        }
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                if !v.is_array()
                    && let Some(why) = cites(v, hidden)
                {
                    count(why, withheld);
                    out.insert(k.clone(), Value::Null);
                    continue;
                }
                out.insert(k.clone(), go(v, hidden, withheld));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// A copy without the facts that cite a hidden src. List items are removed; a dict field that is
/// itself such a fact becomes null.
pub fn prune(obj: Value, hidden: &HashMap<String, Why>, withheld: &mut Withheld) -> Value {
    if hidden.is_empty() {
        return obj;
    }
    go(&obj, hidden, withheld)
}

/// The page still waits for extraction: an open pending_document item lists it (or lists no pages).
pub async fn page_pending(conn: &mut PgConnection, document_id: Uuid, page_no: i32) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select exists (select 1 from review_item where document_id = $1 and kind = 'pending_document' \
         and status = 'open' and (jsonb_typeof(payload->'pages') is distinct from 'array' \
         or payload->'pages' @> to_jsonb($2::int)))",
    )
    .bind(document_id)
    .bind(page_no)
    .fetch_one(conn)
    .await?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageRefusal {
    Scope(Vec<String>),
    Sensitive,
    Minor,
    Foreign,
}

/// Why the caller may not read one page's text (mcp.md 5.7, 9.7), or None. A finished page is
/// permitted when the caller can read every fact extracted from it (committed or still queued); a
/// pending page can hold anything, so it needs every category, the agents genome switch, and a
/// profile that is not a minor. The document-wide sensitivity label is checked by the caller.
pub async fn page_refusal(
    conn: &mut PgConnection,
    perm: &Permission,
    gate: GenomeGate,
    document_id: Uuid,
    page_no: i32,
) -> Result<Option<PageRefusal>> {
    if page_pending(&mut *conn, document_id, page_no).await? {
        let missing: Vec<String> = PAGE_ALL_SCOPES
            .iter()
            .filter(|s| !perm.scopes.contains(**s))
            .map(|s| scope_label(s))
            .collect();
        if !missing.is_empty() {
            return Ok(Some(PageRefusal::Scope(missing)));
        }
        if gate.minor {
            return Ok(Some(PageRefusal::Minor));
        }
        if !gate.include_sensitive {
            return Ok(Some(PageRefusal::Sensitive));
        }
        return Ok(None);
    }
    let src_ids: Vec<String> = sqlx::query_scalar("select id from src where document_id = $1 and page_no = $2")
        .bind(document_id)
        .bind(page_no)
        .fetch_all(&mut *conn)
        .await?;
    let mut reasons: Vec<Why> = hidden_reasons(&mut *conn, perm, gate, &src_ids)
        .await?
        .into_values()
        .collect();
    // rows read from the page that wait in the review queue are on the page too
    let queued: Vec<Option<Value>> = sqlx::query_scalar(
        "select payload->'observation' from review_item where document_id = $1 and status = 'open' \
         and kind = 'candidate_row' and coalesce(payload->>'page_no', payload->'src'->>'page_no') = $2",
    )
    .bind(document_id)
    .bind(page_no.to_string())
    .fetch_all(&mut *conn)
    .await?;
    for obs in queued {
        let obs = obs.unwrap_or(Value::Null);
        let scope = obs_category_scope(obs.get("category").and_then(Value::as_str));
        if !perm.scopes.contains(scope) {
            reasons.push(Why::Scope(scope_label(scope)));
        }
        if obs.get("sensitivity").is_some_and(crate::envelope::truthy) && !perm.scopes.contains("read:sensitive") {
            reasons.push(Why::Sensitive);
        }
    }
    if reasons.contains(&Why::Foreign) {
        return Ok(Some(PageRefusal::Foreign));
    }
    let scopes: BTreeSet<String> = reasons
        .iter()
        .filter_map(|r| match r {
            Why::Scope(l) => Some(l.clone()),
            _ => None,
        })
        .collect();
    if !scopes.is_empty() {
        return Ok(Some(PageRefusal::Scope(scopes.into_iter().collect())));
    }
    if reasons.contains(&Why::Sensitive) {
        return Ok(Some(PageRefusal::Sensitive));
    }
    if reasons.contains(&Why::Minor) {
        return Ok(Some(PageRefusal::Minor));
    }
    Ok(None)
}
