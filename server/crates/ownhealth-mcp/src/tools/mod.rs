//! The eight tools and the one pipeline every call goes through (`mcp/tools/__init__.py`, mcp.md 3,
//! 5, 9):
//!
//! ```text
//! rate bucket -> visible? -> arguments -> profile permission -> tool -> prune hidden facts
//! -> identity mode -> budget fit + cursor -> egress budget -> usage + audit -> CallToolResult
//! ```
//!
//! Tools never check permissions themselves beyond their own extra rules: the pipeline resolves the
//! profile through `core::access::require` and drops any cited fact the caller cannot read.

pub mod drug;
pub mod genome;
pub mod observations;
pub mod propose;
pub mod pyschema;
pub mod source;
pub mod status;

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Utc};
use ownhealth_core::access::{self, Permission, Principal, ProfileRef, Scopes};
use ownhealth_core::audit::{AuditEvent, hash_arg, write_audit};
use ownhealth_core::pyjson::py_isoformat;
use ownhealth_core::{Error, Runtime};
use regex::Regex;
use serde_json::{Map, Value, json};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::envelope::{
    self, Fields, ToolError, collect_src_ids, count_facts, decode_cursor, dumps, encode_cursor, estimate_tokens,
    query_hash, tokens_of,
};
use crate::gate::{self, GenomeGate, Withheld};
use crate::limits::{self, Limits, Usage};
use crate::redact;
use crate::services::Services;

pub const DEFAULT_BUDGET: usize = 4000;
pub const MAX_BUDGET: usize = 15000;

/// The outputSchema of every tool.
pub fn output_schema() -> Value {
    json!({
        "type": "object",
        "required": ["profile", "as_of", "generated_at", "lang", "identity_mode", "data", "withheld", "budget", "notice"],
        "properties": {
            "profile": {"type": ["string", "null"]},
            "as_of": {"type": "string"},
            "generated_at": {"type": "string"},
            "lang": {"enum": ["es", "en"]},
            "identity_mode": {"enum": ["full", "pseudonymous", "minimal"]},
            "data": {"type": "object"},
            "evidence": {"type": "array"},
            "limitations": {"type": "array"},
            "not_assessed": {"type": "array"},
            "withheld": {
                "type": "object",
                "required": ["sensitive", "by_scope", "minor_rule"],
                "properties": {
                    "sensitive": {"type": "integer"},
                    "by_scope": {"type": "array", "items": {"type": "string"}},
                    "minor_rule": {"type": "integer"},
                },
            },
            "db_versions": {"type": "object"},
            "next_cursor": {"type": ["string", "null"]},
            "budget": {
                "type": "object",
                "required": ["tokens_est", "truncated"],
                "properties": {"tokens_est": {"type": "integer"}, "truncated": {"type": "boolean"}},
            },
            "notice": {"type": "string"},
        },
    })
}

const DENIED_CODES: [&str; 7] = [
    "profile_not_granted",
    "scope_missing",
    "rate_limited",
    "egress_budget_exhausted",
    "genome_lookup_cap",
    "propose_cap",
    "unknown",
];

// ------------------------------------------------------------------ types ----

/// What a tool can fail with: a tool error the agent sees, or a core error the pipeline maps.
#[derive(Debug)]
pub enum Fail {
    /// Boxed: a ToolError is large, and every tool returns this on its error path.
    Tool(Box<ToolError>),
    Core(Error),
}

impl From<ToolError> for Fail {
    fn from(e: ToolError) -> Self {
        Fail::Tool(Box::new(e))
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Core(e)
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Fail::Core(Error::Db(e))
    }
}

pub type ToolResult<T> = Result<T, Fail>;

/// A last rewrite of a tool's data after the budget fit (get_trend sorts its points back).
pub type Finalize = fn(Map<String, Value>) -> Map<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct AsOf {
    /// The date rules and status use.
    pub day: NaiveDate,
    /// Cutoff for lists; None is the current view (the pipeline pins it to the database clock).
    pub at: Option<DateTime<FixedOffset>>,
    /// What the envelope shows.
    pub label: String,
}

impl AsOf {
    pub fn at_utc(&self) -> Option<DateTime<Utc>> {
        self.at.map(|a| a.with_timezone(&Utc))
    }
}

/// One call's context: the transaction, the caller and the parsed common arguments.
pub struct Call<'c> {
    pub conn: &'c mut PgConnection,
    pub rt: &'c Runtime,
    pub svc: &'c dyn Services,
    pub principal: &'c Principal,
    pub perm: Option<Permission>,
    pub args: Map<String, Value>,
    pub as_of: AsOf,
    pub lang: String,
    pub units: String,
    pub mode: String,
    pub offset: usize,
    pub inner_cursor: Option<Value>,
    pub usage: Usage,
    pub limits: Limits,
    pub tool: String,
}

impl Call<'_> {
    pub fn perm(&self) -> ToolResult<&Permission> {
        self.perm
            .as_ref()
            .ok_or_else(|| Fail::Core(Error::Invalid("tool called without a profile".into())))
    }

    pub fn profile_id(&self) -> ToolResult<Uuid> {
        Ok(self.perm()?.profile.id)
    }

    pub fn scopes(&self) -> Scopes {
        self.perm.as_ref().map(|p| p.scopes.clone()).unwrap_or_default()
    }

    pub fn has(&self, scope: &str) -> bool {
        self.perm.as_ref().is_some_and(|p| p.has(scope))
    }

    pub fn gate(&self) -> ToolResult<GenomeGate> {
        Ok(gate::genome_gate(
            self.perm()?,
            self.as_of.day,
            self.rt.settings.age_of_majority,
        ))
    }

    pub fn arg_str(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(Value::as_str)
    }
}

/// What a tool returns; the pipeline turns it into the envelope.
pub struct Answer {
    pub data: Map<String, Value>,
    /// Key in data of the list the budget may cut and the cursor resumes.
    pub pageable: Option<&'static str>,
    /// Tokens for structuredContent plus the text block.
    pub budget: usize,
    /// get_health_status: the whole structuredContent under 2 KB.
    pub max_bytes: Option<usize>,
    pub limitations: Vec<Value>,
    pub not_assessed: Vec<Value>,
    pub evidence: Vec<Value>,
    pub db_versions: Map<String, Value>,
    pub withheld: Option<Value>,
    pub notice: Option<String>,
    /// Extra usage: lookups, proposals.
    pub counters: Usage,
    /// A cursor of the backing service, wrapped in ours.
    pub inner_next_cursor: Option<Value>,
    /// Reorder after the cut (trend points ascending).
    pub finalize: Option<Finalize>,
    /// For tools that are not profile-bound.
    pub profile_slug: Option<String>,
    pub audit_profile_id: Option<Uuid>,
    pub prune: bool,
}

impl Answer {
    pub fn new(data: Map<String, Value>) -> Answer {
        Answer {
            data,
            pageable: None,
            budget: DEFAULT_BUDGET,
            max_bytes: None,
            limitations: Vec::new(),
            not_assessed: Vec::new(),
            evidence: Vec::new(),
            db_versions: Map::new(),
            withheld: None,
            notice: None,
            counters: Usage::default(),
            inner_next_cursor: None,
            finalize: None,
            profile_slug: None,
            audit_profile_id: None,
            prune: true,
        }
    }
}

pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub input_schema: fn() -> Value,
    /// Listed and callable with any one of these...
    pub any_scope: &'static [&'static str],
    /// ...and every one of these.
    pub all_scopes: &'static [&'static str],
    pub profile_bound: bool,
    pub read_only: bool,
}

impl ToolSpec {
    pub fn visible(&self, scopes: &Scopes) -> bool {
        self.any_scope.iter().any(|s| scopes.contains(*s)) && self.all_scopes.iter().all(|s| scopes.contains(*s))
    }

    /// The tools/list entry.
    pub fn as_mcp(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": (self.input_schema)(),
            "outputSchema": output_schema(),
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false,
            },
        })
    }
}

/// Every tool, sorted by name (tools/list order).
pub static REGISTRY: [&ToolSpec; 8] = [
    &drug::SPEC,
    &genome::FINDINGS_SPEC,
    &status::SPEC,
    &source::SPEC,
    &observations::TREND_SPEC,
    &genome::LOOKUP_SPEC,
    &propose::SPEC,
    &observations::SEARCH_SPEC,
];

pub fn spec(name: &str) -> Option<&'static ToolSpec> {
    REGISTRY.iter().copied().find(|s| s.name == name)
}

#[allow(clippy::expect_used)] // the schemas are literals checked by the tests
static VALIDATORS: LazyLock<HashMap<&'static str, jsonschema::Validator>> = LazyLock::new(|| {
    REGISTRY
        .iter()
        .map(|s| {
            let v = jsonschema::options()
                .should_validate_formats(false)
                .build(&(s.input_schema)())
                .expect("tool input schema compiles");
            (s.name, v)
        })
        .collect()
});

async fn run_tool(call: &mut Call<'_>) -> ToolResult<Answer> {
    match call.tool.as_str() {
        "check_drug" => drug::check_drug(call).await,
        "get_genome_findings" => genome::get_genome_findings(call).await,
        "get_health_status" => status::get_health_status(call).await,
        "get_source" => source::get_source(call).await,
        "get_trend" => observations::get_trend(call).await,
        "lookup_variant" => genome::lookup_variant(call).await,
        "propose" => propose::propose(call).await,
        "search_observations" => observations::search_observations(call).await,
        other => Err(ToolError::fix(
            "bad_input",
            format!("Unknown tool '{other}'."),
            "Call tools/list for the tools this token has.",
        )
        .into()),
    }
}

// ----------------------------------------------------------------- helpers ----

/// Python's `datetime.fromisoformat` for the forms an agent sends: a date, then `T` or a space, a
/// time of 2 to 6 parts with an optional fraction, and an optional `Z` or offset. Naive is UTC.
pub fn parse_iso_datetime(value: &str) -> Option<DateTime<FixedOffset>> {
    let v = value.replace('Z', "+00:00");
    if v.len() < 10 || !v.is_char_boundary(10) {
        return None;
    }
    let date = parse_iso_date(&v[..10])?;
    let rest = &v[10..];
    if rest.is_empty() {
        return Some(Utc.fix().from_utc_datetime(&date.and_time(NaiveTime::MIN)));
    }
    let mut it = rest.chars();
    it.next()?;
    let rest = it.as_str();
    let (time_s, offset) = match rest.find(['+', '-']) {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest, None),
    };
    let time = ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M", "%H%M%S", "%H%M"]
        .iter()
        .find_map(|f| NaiveTime::parse_from_str(time_s, f).ok())
        .or_else(|| {
            (time_s.len() == 2)
                .then(|| {
                    time_s
                        .parse::<u32>()
                        .ok()
                        .and_then(|h| NaiveTime::from_hms_opt(h, 0, 0))
                })
                .flatten()
        })?;
    let naive = NaiveDateTime::new(date, time);
    let tz = match offset {
        None => Utc.fix(),
        Some(o) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let digits: String = o[1..].chars().filter(char::is_ascii_digit).collect();
            if !matches!(digits.len(), 2 | 4 | 6) {
                return None;
            }
            let h: i32 = digits[..2].parse().ok()?;
            let m: i32 = digits.get(2..4).map_or(Ok(0), str::parse).ok()?;
            let s: i32 = digits.get(4..6).map_or(Ok(0), str::parse).ok()?;
            FixedOffset::east_opt(sign * (h * 3600 + m * 60 + s))?
        }
    };
    tz.from_local_datetime(&naive).single()
}

/// Python's `date.fromisoformat`: `YYYY-MM-DD` or `YYYYMMDD`, zero-padded.
pub fn parse_iso_date(value: &str) -> Option<NaiveDate> {
    let b = value.as_bytes();
    let ok = match b.len() {
        10 => b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        }),
        8 => b.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if !ok {
        return None;
    }
    NaiveDate::parse_from_str(value, if b.len() == 10 { "%Y-%m-%d" } else { "%Y%m%d" }).ok()
}

/// `datetime.combine(d, time.max, UTC)`.
pub fn end_of_day(d: NaiveDate) -> DateTime<FixedOffset> {
    let t = NaiveTime::from_hms_micro_opt(23, 59, 59, 999_999).unwrap_or(NaiveTime::MIN);
    Utc.fix().from_utc_datetime(&d.and_time(t))
}

pub fn start_of_day(d: NaiveDate) -> DateTime<FixedOffset> {
    Utc.fix().from_utc_datetime(&d.and_time(NaiveTime::MIN))
}

pub async fn parse_as_of(conn: &mut PgConnection, value: Option<&str>) -> ToolResult<AsOf> {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        let d = limits::today(conn).await?;
        return Ok(AsOf {
            day: d,
            at: None,
            label: d.format("%Y-%m-%d").to_string(),
        });
    };
    let bad = || {
        ToolError::fix(
            "bad_input",
            format!("as_of '{value}' is not an ISO date."),
            "Use YYYY-MM-DD.",
        )
    };
    if value.contains('T') || value.contains(' ') {
        let dt = parse_iso_datetime(value).ok_or_else(bad)?;
        return Ok(AsOf {
            day: dt.date_naive(),
            at: Some(dt),
            label: value.to_string(),
        });
    }
    let d = parse_iso_date(value).ok_or_else(bad)?;
    Ok(AsOf {
        day: d,
        at: Some(end_of_day(d)),
        label: value.to_string(),
    })
}

/// The database's clock: `recorded_at` comes from it, so a cutoff must too.
pub async fn db_now(conn: &mut PgConnection) -> ToolResult<DateTime<FixedOffset>> {
    let now: DateTime<Utc> = sqlx::query_scalar("select now()").fetch_one(conn).await?;
    Ok(now.fixed_offset())
}

/// What this token can reach on at least one of its profiles; tools/list is filtered by it.
pub async fn union_scopes(conn: &mut PgConnection, principal: &Principal) -> Result<Scopes, Error> {
    let mut out = Scopes::new();
    for perm in access::accessible_profiles(conn, principal).await? {
        out.extend(perm.scopes);
    }
    Ok(out)
}

#[allow(clippy::unwrap_used)]
static SAFE_ARG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_:.\-*/>+]{1,64}\n?$").unwrap());
const HASHED_ARGS: [&str; 4] = ["query", "drug", "payload", "text"];
const OMITTED_ARGS: [&str; 1] = ["cursor"];

fn chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Codes and ids only; free text hashed (mcp.md 9.5).
pub fn args_summary(args: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    let mut parts = Vec::new();
    for k in keys {
        let v = &args[k];
        if OMITTED_ARGS.contains(&k.as_str()) || v.is_null() {
            continue;
        }
        match v {
            Value::Bool(b) => parts.push(format!("{k}={}", if *b { "True" } else { "False" })),
            Value::Number(n) => parts.push(format!("{k}={}", envelope::py_number(n))),
            Value::String(s) if !HASHED_ARGS.contains(&k.as_str()) => {
                if SAFE_ARG.is_match(s) {
                    parts.push(format!("{k}={s}"));
                } else {
                    parts.push(format!("{k}={}", hash_arg(s)));
                }
            }
            Value::String(s) => parts.push(format!("{k}={}", hash_arg(s))),
            Value::Array(a) if a.iter().all(|x| x.as_str().is_some_and(|s| SAFE_ARG.is_match(s))) => {
                let joined: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
                parts.push(format!("{k}={}", chars(&joined.join(","), 200)));
            }
            other => parts.push(format!("{k}={}", hash_arg(&dumps(other)))),
        }
    }
    chars(&parts.join(" "), 1000)
}

pub async fn household_names(conn: &mut PgConnection, household_id: Option<Uuid>) -> Result<Vec<String>, Error> {
    let rows: Vec<String> = sqlx::query_scalar(
        "select display_name from app_user where household_id = $1 \
         union select display_name from profile where household_id = $1",
    )
    .bind(household_id)
    .fetch_all(conn)
    .await?;
    Ok(redact::name_dictionary(&rows))
}

/// The HTTP request behind a call: protocol version, client address, user agent.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub protocol_version: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

pub fn error_result(err: &ToolError) -> Value {
    json!({
        "content": [{"type": "text", "text": dumps(&err.as_dict())}],
        "structuredContent": Value::Object(err.body()),
        "isError": true,
    })
}

pub fn token_event(principal: &Principal, meta: &Meta, action: String) -> AuditEvent {
    let mut e = AuditEvent::new("token", "mcp", &action);
    e.actor_id = principal.user_id;
    e.token_id = principal.token_id;
    e.client = principal.token_label.clone();
    e.protocol_version = meta.protocol_version.clone();
    e.ip = meta.ip.clone();
    e
}

fn tool_event(principal: &Principal, name: &str, meta: &Meta) -> AuditEvent {
    let mut e = token_event(principal, meta, format!("tools/call:{name}"));
    e.tool = Some(name.to_string());
    e.user_agent = meta
        .user_agent
        .as_deref()
        .map(|u| chars(u, 200))
        .filter(|u| !u.is_empty());
    e
}

pub fn access_error(e: Error) -> Fail {
    match e {
        Error::AccessDenied { code, message, allowed }
            if code == "profile_not_granted" || code == "profile_required" =>
        {
            let list = if allowed.is_empty() {
                "none".to_string()
            } else {
                allowed.join(", ")
            };
            ToolError::fix("profile_not_granted", message, format!("Use one of: {list}."))
                .with("allowed", json!(allowed))
                .into()
        }
        Error::AccessDenied { message, .. } => ToolError::fix(
            "scope_missing",
            message,
            "Ask the profile's owner for a token with that scope.",
        )
        .into(),
        other => Fail::Core(other),
    }
}

/// A core error as the tool error the agent sees.
pub fn tool_error_of(f: Fail, tool: &str) -> ToolError {
    match f {
        Fail::Tool(t) => *t,
        Fail::Core(e @ Error::AccessDenied { .. }) => match access_error(e) {
            Fail::Tool(t) => *t,
            Fail::Core(_) => internal(),
        },
        Fail::Core(e) => {
            tracing::error!(error = %e, "tool {tool} failed");
            internal()
        }
    }
}

fn internal() -> ToolError {
    ToolError::fix(
        "internal_error",
        "OwnHealth could not answer this call.",
        "Try again or report it.",
    )
}

// ---------------------------------------------------------------- pipeline ----

/// Run one tools/call and return the CallToolResult as JSON.
pub async fn execute(
    rt: &Runtime,
    svc: &dyn Services,
    name: &str,
    args: Map<String, Value>,
    principal: &Principal,
    meta: &Meta,
) -> Result<Value, Error> {
    let token_id = principal.token_id.unwrap_or_default();
    let mut conn = rt.pool.acquire().await?;
    let (lim, day, wait) = {
        let mut tx = conn.begin().await?;
        let lim = limits::limits_of(&mut tx, principal.token_id).await?;
        let day = limits::today(&mut tx).await?;
        let wait = limits::take_call(&mut tx, token_id, lim.rate_per_min, None).await?;
        tx.commit().await?;
        (lim, day, wait)
    };
    let spec = spec(name);
    let outcome: Result<Value, ToolError> = async {
        if let Some(w) = wait {
            return Err(ToolError::fix(
                "rate_limited",
                "Too many calls for this token.",
                format!("Retry in {} s.", envelope::py_float(w)),
            )
            .with("retry_after_s", json!(w)));
        }
        let Some(spec) = spec else {
            return Err(ToolError::fix(
                "bad_input",
                format!("Unknown tool '{name}'."),
                "Call tools/list for the tools this token has.",
            ));
        };
        let mut tx = conn.begin().await.map_err(|e| tool_error_of(Fail::from(e), name))?;
        match run(&mut tx, rt, svc, spec, &args, principal, meta, lim, day).await {
            Ok(v) => {
                tx.commit().await.map_err(|e| tool_error_of(Fail::from(e), name))?;
                Ok(v)
            }
            Err(f) => Err(tool_error_of(f, name)),
        }
    }
    .await;
    match outcome {
        Ok(v) => Ok(v),
        Err(err) => {
            let mut tx = conn.begin().await?;
            limits::add_usage(
                &mut tx,
                token_id,
                day,
                Usage {
                    calls: 1,
                    ..Usage::default()
                },
            )
            .await?;
            let mut e = tool_event(principal, if spec.is_some() { name } else { "unknown" }, meta);
            e.args_summary = Some(args_summary(&args));
            e.result_kind = Some(
                if DENIED_CODES.contains(&err.code.as_str()) {
                    "denied"
                } else {
                    "error"
                }
                .into(),
            );
            e.rows_out = Some(0);
            e.bytes_out = Some(0);
            write_audit(&mut tx, &e).await?;
            tx.commit().await?;
            Ok(error_result(&err))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    conn: &mut PgConnection,
    rt: &Runtime,
    svc: &dyn Services,
    spec: &ToolSpec,
    args: &Map<String, Value>,
    principal: &Principal,
    meta: &Meta,
    lim: Limits,
    day: NaiveDate,
) -> ToolResult<Value> {
    if !spec.visible(&union_scopes(&mut *conn, principal).await?) {
        return Err(ToolError::fix(
            "scope_missing",
            format!("This token cannot use {}.", spec.name),
            "Use a tool from tools/list.",
        )
        .into());
    }
    if let Some(v) = VALIDATORS.get(spec.name) {
        let instance = Value::Object(args.clone());
        if let Some(e) = v.iter_errors(&instance).next() {
            let (place, message) = pyschema::python_error(args, &(spec.input_schema)()).unwrap_or_else(|| {
                let segs: Vec<String> = e
                    .instance_path()
                    .as_str()
                    .split('/')
                    .skip(1)
                    .map(|s| s.replace("~1", "/").replace("~0", "~"))
                    .collect();
                let place = if segs.is_empty() {
                    "arguments".to_string()
                } else {
                    segs.join(".")
                };
                (place, e.to_string())
            });
            return Err(ToolError::fix(
                "bad_input",
                format!("Invalid {place}: {}", chars(&message, 200)),
                "Check the tool's input schema.",
            )
            .into());
        }
    }

    let mut perm = None;
    if spec.profile_bound {
        let pref = ProfileRef::from(args.get("profile").and_then(Value::as_str));
        let p = access::require(&mut *conn, principal, pref, None, None)
            .await
            .map_err(access_error)?;
        if !spec.visible(&p.scopes) {
            let missing: Vec<&str> = spec
                .all_scopes
                .iter()
                .chain(spec.any_scope)
                .copied()
                .filter(|s| !p.scopes.contains(*s))
                .collect();
            let needs = if spec.all_scopes.is_empty() {
                spec.any_scope.join(" or ")
            } else {
                spec.all_scopes.join(" and ")
            };
            return Err(ToolError::fix(
                "scope_missing",
                format!("This token cannot use {} on profile '{}'.", spec.name, p.profile.slug),
                format!("Needs {needs}; missing {}.", missing.join(", ")),
            )
            .into());
        }
        perm = Some(p);
    }

    let mut rest = Map::new();
    for (k, v) in args {
        if k != "cursor" {
            rest.insert(k.clone(), v.clone());
        }
    }
    let qhash = query_hash(&[
        json!(spec.name),
        json!(
            principal
                .token_id
                .map(|t| t.to_string())
                .unwrap_or_else(|| "None".into())
        ),
        perm.as_ref()
            .map_or(Value::Null, |p: &Permission| json!(p.profile.id.to_string())),
        Value::Object(rest),
    ]);
    let mut offset = 0usize;
    let mut inner = None;
    let mut as_of = parse_as_of(&mut *conn, args.get("as_of").and_then(Value::as_str)).await?;
    if as_of.at.is_none() {
        // pin "now", so every page of a paginated answer sees the same store
        as_of.at = Some(db_now(&mut *conn).await?);
    }
    let secret = rt.settings.secret_key.clone();
    if let Some(cursor) = args.get("cursor").and_then(Value::as_str).filter(|c| !c.is_empty()) {
        let (cur_as_of, key) = decode_cursor(&secret, cursor, &qhash).map_err(|e| {
            ToolError::fix(
                "bad_input",
                format!("Bad cursor: {e}."),
                "Repeat the call without cursor.",
            )
        })?;
        offset = match key.get("off") {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as usize,
            _ => 0,
        };
        inner = key.get("inner").cloned().filter(|v| !v.is_null());
        if let Some(a) = cur_as_of {
            let at = DateTime::parse_from_rfc3339(&a)
                .ok()
                .or_else(|| parse_iso_datetime(&a))
                .ok_or_else(|| {
                    ToolError::fix(
                        "bad_input",
                        "Bad cursor: malformed cursor.",
                        "Repeat the call without cursor.",
                    )
                })?;
            as_of.at = Some(at);
        }
    }

    let token_id = principal.token_id.unwrap_or_default();
    let usage = limits::usage(&mut *conn, token_id, day).await?;
    let mode = redact::stricter(&principal.identity_mode, None);
    let lang = if ["es", "en"].contains(&principal.lang.as_str()) {
        principal.lang.clone()
    } else {
        "es".into()
    };
    let mut call = Call {
        conn: &mut *conn,
        rt,
        svc,
        principal,
        perm,
        args: args.clone(),
        as_of,
        lang,
        units: principal.units.clone(),
        mode,
        offset,
        inner_cursor: inner,
        usage,
        limits: lim,
        tool: spec.name.to_string(),
    };
    let answer = run_tool(&mut call).await?;
    let counters = answer.counters;
    let audit_profile = answer.audit_profile_id;
    let (env, body_text) = finish(&mut call, answer, &qhash, &secret).await?;
    let profile_id = if spec.profile_bound {
        call.perm.as_ref().map(|p| p.profile.id)
    } else {
        audit_profile
    };
    drop(call);
    let facts = count_facts(env.get("data").unwrap_or(&Value::Null)) as i64;
    let nbytes = (dumps(&env).len() + body_text.len()) as i64;
    if usage.facts + facts > lim.egress_facts_per_day || usage.bytes + nbytes > lim.egress_bytes_per_day {
        return Err(ToolError::fix(
            "egress_budget_exhausted",
            "This token's daily budget is used up.",
            "It resets at 00:00 in the install's timezone; a person can raise it in Settings.",
        )
        .with("facts_today", json!(usage.facts))
        .with("bytes_today", json!(usage.bytes))
        .into());
    }
    let add = Usage {
        calls: 1,
        facts,
        bytes: nbytes,
        lookups: counters.lookups,
        proposals: counters.proposals,
    };
    limits::add_usage(&mut *conn, token_id, day, add).await?;
    let data = env.get("data").cloned().unwrap_or(Value::Null);
    let mut e = tool_event(principal, spec.name, meta);
    e.profile_id = profile_id;
    e.args_summary = Some(args_summary(args));
    let truncated = env
        .pointer("/budget/truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    e.result_kind = Some(if truncated { "budget_cut" } else { "ok" }.into());
    e.rows_out = Some(i32::try_from(facts).unwrap_or(i32::MAX));
    e.bytes_out = Some(i32::try_from(nbytes).unwrap_or(i32::MAX));
    let srcs: Vec<String> = collect_src_ids(&data).into_iter().take(500).collect();
    e.src_ids = (!srcs.is_empty()).then_some(srcs);
    e.withheld = env.get("withheld").cloned();
    write_audit(&mut *conn, &e).await?;
    Ok(json!({
        "content": [{"type": "text", "text": body_text}],
        "structuredContent": env,
        "isError": false,
    }))
}

/// Prune, redact, cut to the budget, sign the cursor, render the text block.
pub async fn finish(call: &mut Call<'_>, mut answer: Answer, qhash: &str, secret: &str) -> ToolResult<(Value, String)> {
    let mut data = Value::Object(std::mem::take(&mut answer.data));
    let mut withheld = Withheld::default();
    withheld.merge(answer.withheld.as_ref());
    if let Some(perm) = call.perm.as_ref()
        && answer.prune
    {
        let g = gate::genome_gate(perm, call.as_of.day, call.rt.settings.age_of_majority);
        let ids = collect_src_ids(&data);
        let hidden = gate::hidden_reasons(&mut *call.conn, perm, g, &ids).await?;
        data = gate::prune(data, &hidden, &mut withheld);
    }
    let names = if call.mode != "full" {
        household_names(&mut *call.conn, call.principal.household_id).await?
    } else {
        Vec::new()
    };
    let day = call.as_of.day;
    let data = redact::apply(data, &call.mode, day, &names);
    let limitations = redact::apply(
        Value::Array(std::mem::take(&mut answer.limitations)),
        &call.mode,
        day,
        &names,
    );
    let not_assessed = redact::apply(
        Value::Array(std::mem::take(&mut answer.not_assessed)),
        &call.mode,
        day,
        &names,
    );
    let as_vec = |v: Value| match v {
        Value::Array(a) => a,
        _ => Vec::new(),
    };
    let limitations = as_vec(limitations);
    let not_assessed = as_vec(not_assessed);
    let data = match data {
        Value::Object(m) => m,
        _ => Map::new(),
    };

    let budget = answer.budget.min(MAX_BUDGET);
    let key = answer.pageable;
    let items: Vec<Value> = match key.and_then(|k| data.get(k)) {
        Some(Value::Array(a)) => a.iter().skip(call.offset).cloned().collect(),
        _ => Vec::new(),
    };
    let pin = match call.as_of.at {
        Some(a) => a,
        None => db_now(&mut *call.conn).await?,
    };
    let pin_s = py_isoformat(&pin);
    let profile = match call.perm.as_ref() {
        Some(p) => json!(p.profile.slug),
        None => json!(answer.profile_slug),
    };

    let build = |n: usize| -> (Value, String, bool) {
        let mut d = data.clone();
        let mut more = false;
        let mut cursor_key = None;
        if let Some(k) = key {
            d.insert(k.to_string(), Value::Array(items[..n.min(items.len())].to_vec()));
            more = n < items.len();
            if more {
                cursor_key = Some(json!({"off": call.offset + n}));
            }
        }
        if let Some(inner) = &answer.inner_next_cursor
            && !more
        {
            cursor_key = Some(json!({"inner": inner}));
        }
        if let Some(f) = answer.finalize {
            d = f(d);
        }
        let mut env = envelope::envelope(
            profile.clone(),
            &call.as_of.label,
            &call.lang,
            &call.mode,
            Value::Object(d),
            Fields {
                evidence: answer.evidence.clone(),
                limitations: limitations.clone(),
                not_assessed: not_assessed.clone(),
                withheld: Some(withheld.as_map()),
                db_versions: answer.db_versions.clone(),
                next_cursor: cursor_key.map(|k| encode_cursor(secret, qhash, &pin_s, &k)),
                budget: Some(json!({"tokens_est": 0, "truncated": key.is_some() && n < items.len()})),
                notice: answer.notice.clone(),
                ..Fields::default()
            },
        );
        let body_text = crate::text::render(&env, &call.tool);
        let tokens = estimate_tokens(&env) + tokens_of(&body_text);
        if let Some(b) = env.get_mut("budget").and_then(Value::as_object_mut) {
            b.insert("tokens_est".into(), json!(tokens));
        }
        let fits = tokens <= budget && answer.max_bytes.is_none_or(|m| dumps(&env).len() < m);
        (env, body_text, fits)
    };

    let (env, body_text, fits) = build(items.len());
    if fits || items.is_empty() {
        return Ok((env, body_text));
    }
    // always return at least one item, so paging moves forward
    let (mut lo, mut hi) = (1usize, items.len() - 1);
    let mut best = build(1);
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let attempt = build(mid);
        if attempt.2 {
            best = attempt;
            lo = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        }
    }
    Ok((best.0, best.1))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_input_schema_compiles_and_the_list_is_sorted() {
        let names: Vec<&str> = REGISTRY.iter().map(|s| s.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert_eq!(VALIDATORS.len(), 8);
    }

    #[test]
    fn args_summary_hashes_free_text() {
        let args = json!({"query": "colesterol de mi padre", "profile": "alex", "detail": "full",
                          "flagged_only": true, "cursor": "x", "based_on": ["src_a", "doc_1"], "n": 2.5});
        let s = args_summary(args.as_object().unwrap());
        assert_eq!(
            s,
            format!(
                "based_on=src_a,doc_1 detail=full flagged_only=True n=2.5 profile=alex query={}",
                hash_arg("colesterol de mi padre")
            )
        );
    }

    #[test]
    fn iso_datetimes() {
        let dt = parse_iso_datetime("2026-11-07T23:30:00-05:00").unwrap();
        assert_eq!(dt.date_naive(), NaiveDate::from_ymd_opt(2026, 11, 7).unwrap());
        assert_eq!(py_isoformat(&dt), "2026-11-07T23:30:00-05:00");
        assert_eq!(
            py_isoformat(&parse_iso_datetime("2026-11-07 10:00").unwrap()),
            "2026-11-07T10:00:00+00:00"
        );
        assert_eq!(
            py_isoformat(&parse_iso_datetime("2026-11-07T10:00:00.123456Z").unwrap()),
            "2026-11-07T10:00:00.123456+00:00"
        );
        assert!(parse_iso_datetime("2026-13-07T10:00").is_none());
        assert_eq!(
            py_isoformat(&end_of_day(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap())),
            "2024-01-01T23:59:59.999999+00:00"
        );
    }
}
