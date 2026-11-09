//! Lane G (v0.2): the source registry over REST (`core::sources`) and the health endpoint per source
//! (ROADMAP v0.2 Ops: "last successful sync" for alerting). Owner or guardian of the household; the
//! secrets inside a source are write-only (never returned).
//!
//! | Route | What |
//! |---|---|
//! | `GET /inboxes` | the household's sources with their health |
//! | `POST /inboxes` | `{kind, label, profile_id?, config, secret?, poll_seconds?}`; a secret is sealed (`core::secrets`) |
//! | `GET /inboxes/{id}`, `PATCH /inboxes/{id}`, `DELETE /inboxes/{id}` | one source |
//! | `GET /inboxes/{id}/runs?limit=` | recent runs and their errors |
//! | `POST /inboxes/{id}/token` | a new push token, shown once (step-up) |
//! | `POST /inboxes/{id}/sync` | poll now |
//! | `GET /inboxes/health` | `[{id, kind, label, profile_id, enabled, last_run_at, last_success_at, last_error_at, last_error, consecutive_failures, stale, last_run_status, last_run_problems, last_run_problem}]` |
//! | `GET /inboxes/{id}/health` | one source's row; a session, or that source's own `oh_ing_` token |
//!
//! Both health routes take `?format=prometheus` and answer in the Prometheus text format, so a
//! Prometheus job (or any monitor that can send a bearer token) alerts on a source that stopped
//! syncing. A person sees the sources of the profiles they can open, plus the household-wide ones when
//! they own or guard a profile. A source token sees its own source and nothing else.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ownhealth_auth::household::{can_manage, require_manage};
use ownhealth_auth::sessions;
use ownhealth_core::access::{Principal, accessible_profiles};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::sources::{self, NewSource, Source, SourceHealth, SourcePatch, TOKEN_PREFIX};
use ownhealth_core::{Error, Runtime};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{self, Body, R, ok};
use crate::Caller;
use crate::error::ApiError;
use crate::extract::SourceCaller;

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/inboxes", get(list).post(create))
        .route("/inboxes/health", get(health_all))
        .route("/inboxes/{id}", get(get_one).patch(patch).delete(remove))
        .route("/inboxes/{id}/health", get(health_one))
        .route("/inboxes/{id}/runs", get(runs))
        .route("/inboxes/{id}/token", post(token))
        .route("/inboxes/{id}/sync", post(sync))
}

// -------------------------------------------------------------- registry ----

/// The secret purpose a kind seals, if it keeps one.
fn secret_purpose(kind: &str) -> Option<&'static str> {
    match kind {
        "email" => Some("imap_password"),
        "paperless" => Some("paperless_token"),
        "nightscout" => Some("nightscout_token"),
        _ => None,
    }
}

/// Kinds whose items always belong to one profile: they cannot work without `profile_id`.
fn needs_profile(kind: &str) -> bool {
    matches!(kind, "shortcut" | "health_auto_export" | "nightscout")
}

/// The poll interval a new source of this kind gets when the body names none; None for push kinds.
fn default_poll_seconds(kind: &str) -> Option<i32> {
    match kind {
        "email" => Some(ownhealth_inbox::email::DEFAULT_POLL_SECONDS),
        "folder" => Some(ownhealth_inbox::folder::DEFAULT_POLL_SECONDS),
        "paperless" => Some(ownhealth_inbox::paperless::DEFAULT_POLL_SECONDS),
        "nightscout" => Some(ownhealth_devices::nightscout::DEFAULT_POLL_SECONDS),
        _ => None,
    }
}

/// The kind's own config check, so a broken source is refused when saved, not at its first run.
fn check_config(kind: &str, config: &Value) -> Result<(), Error> {
    if !config.is_object() {
        return Err(Error::Invalid("config must be an object".into()));
    }
    match kind {
        "email" => serde_json::from_value::<ownhealth_inbox::email::MailboxConfig>(config.clone())
            .map_err(|e| Error::Invalid(format!("the mailbox settings are incomplete: {e}")))?
            .check(),
        "folder" => ownhealth_inbox::folder::check_config(config),
        "paperless" => ownhealth_inbox::paperless::check_config(config),
        "nightscout" => ownhealth_devices::nightscout::check_config(config).map_err(Error::Invalid),
        _ => Ok(()),
    }
}

/// The config fields that decide where a kind sends its stored secret. Changing one without typing the
/// secret again would hand the sealed password or token to a host the person never gave it to.
fn destination(kind: &str, config: &Value) -> Vec<Value> {
    let field = |k: &str| config.get(k).cloned().unwrap_or(Value::Null);
    match kind {
        "email" => ["host", "port", "tls", "ca_file", "username"].map(field).to_vec(),
        "paperless" | "nightscout" => {
            let url = config
                .get("base_url")
                .filter(|v| !v.is_null())
                .or_else(|| config.get("url"))
                .cloned()
                .unwrap_or(Value::Null);
            let private = config.get("allow_private").and_then(Value::as_bool).unwrap_or(false);
            vec![url, Value::Bool(private)]
        }
        _ => Vec::new(),
    }
}

/// Paperless `config.profile_by_tag`: the profiles a tag routes documents into.
fn tag_profiles(config: &Value) -> Vec<Uuid> {
    config
        .get("profile_by_tag")
        .and_then(Value::as_object)
        .map(|m| {
            m.values()
                .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect()
        })
        .unwrap_or_default()
}

/// A field that may be absent (`None`), null (`Some(None)`) or set.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

fn parse_id(id: &str) -> R<Uuid> {
    Ok(Uuid::parse_str(id).map_err(|_| Error::NotFound("Unknown source.".into()))?)
}

/// The caller's household, when they own or guard a profile in it (inbox settings are household-wide).
async fn manager(conn: &mut PgConnection, p: &Principal) -> R<Uuid> {
    let household = p
        .household_id
        .ok_or_else(|| Error::access_denied("forbidden", "Only a person of the household manages its inboxes."))?;
    let perms = accessible_profiles(&mut *conn, p).await?;
    if !perms
        .iter()
        .any(|perm| perm.profile.household_id == household && can_manage(perm))
    {
        return Err(Error::access_denied("forbidden", "Only an owner or guardian manages the inboxes.").into());
    }
    Ok(household)
}

/// The caller may send data into this profile.
async fn manages_profile(conn: &mut PgConnection, p: &Principal, profile_id: Uuid) -> R<()> {
    let perm = Caller(p.clone())
        .require(conn, &profile_id.to_string(), None, None)
        .await?;
    require_manage(perm)?;
    Ok(())
}

/// A source of the caller's household that the caller manages (its profile, if it has one).
async fn managed_source(conn: &mut PgConnection, p: &Principal, id: &str) -> R<Source> {
    let id = parse_id(id)?;
    let household = manager(conn, p).await?;
    let source = match sources::get_source(conn, id).await? {
        Some(s) if s.household_id == household => s,
        _ => return Err(Error::NotFound("Unknown source.".into()).into()),
    };
    if let Some(profile) = source.profile_id {
        manages_profile(conn, p, profile).await?;
    }
    Ok(source)
}

async fn audit(conn: &mut PgConnection, p: &Principal, action: &str, s: &Source) -> R<()> {
    write_audit(
        conn,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some("inbox".into()),
            profile_id: s.profile_id,
            resource_type: Some("source".into()),
            resource_ids: Some(vec![s.id.to_string()]),
            ..AuditEvent::new("user", "ui", action)
        },
    )
    .await?;
    Ok(())
}

/// A source as the Inbox screen reads it: never the cursor, the secret id or the token hash.
fn source_out(s: &Source, h: Option<&SourceHealth>, quarantined: Option<i64>) -> Value {
    let stale = h.is_some_and(|h| h.stale);
    let mut v = json!({
        "id": s.id, "kind": s.kind, "label": s.label, "profile_id": s.profile_id, "config": s.config,
        "enabled": s.enabled, "poll_seconds": s.poll_seconds, "token_prefix": s.token_prefix,
        "has_secret": s.secret_id.is_some(), "last_run_at": s.last_run_at,
        "last_success_at": s.last_success_at, "last_error_at": s.last_error_at, "last_error": s.last_error,
        "consecutive_failures": s.consecutive_failures, "stale": stale, "created_at": s.created_at,
        "last_run_status": h.and_then(|h| h.last_run_status.as_deref()),
        "last_run_problems": h.map_or(0, |h| h.last_run_problems),
        "last_run_problem": h.and_then(|h| h.last_run_problem.as_deref()),
    });
    if let Some(n) = quarantined {
        v["quarantined"] = json!(n);
    }
    v
}

/// The source with its health and, for a mailbox, its held messages.
async fn out_one(conn: &mut PgConnection, s: &Source) -> R<Value> {
    let health = sources::source_health(conn, s.household_id)
        .await?
        .into_iter()
        .find(|h| h.id == s.id);
    let quarantined = if s.kind == "email" {
        Some(
            sqlx::query_scalar("select count(*) from quarantine_item where source_id = $1 and status = 'held'")
                .bind(s.id)
                .fetch_one(&mut *conn)
                .await?,
        )
    } else {
        None
    };
    Ok(source_out(s, health.as_ref(), quarantined))
}

/// The sources this person may see (the same rule as the health routes).
async fn list(State(rt): State<Runtime>, caller: Caller) -> R {
    let mut conn = rt.pool.acquire().await?;
    let health = visible(&mut conn, &caller).await?;
    let Some(household) = caller.0.household_id else {
        return Err(ApiError::unauthenticated().into());
    };
    let health: HashMap<Uuid, &SourceHealth> = health.iter().map(|h| (h.id, h)).collect();
    let held: HashMap<Uuid, i64> = sqlx::query_as::<_, (Uuid, i64)>(
        "select q.source_id, count(*) from quarantine_item q join source s on s.id = q.source_id          where s.household_id = $1 and q.status = 'held' group by q.source_id",
    )
    .bind(household)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let out: Vec<Value> = sources::list_sources(&mut conn, household)
        .await?
        .iter()
        .filter_map(|s| {
            let h = *health.get(&s.id)?;
            let q = (s.kind == "email").then(|| held.get(&s.id).copied().unwrap_or(0));
            Some(source_out(s, Some(h), q))
        })
        .collect();
    ok(Value::Array(out))
}

async fn get_one(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let mut conn = rt.pool.acquire().await?;
    let s = managed_source(&mut conn, &p, &id).await?;
    ok(out_one(&mut conn, &s).await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceIn {
    kind: String,
    label: String,
    #[serde(default)]
    profile_id: Option<Uuid>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default, deserialize_with = "present")]
    poll_seconds: Option<Option<i32>>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn check_label(label: &str) -> Result<String, Error> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 120 {
        return Err(Error::Invalid("label must be 1 to 120 characters".into()));
    }
    Ok(label.to_string())
}

async fn create(State(rt): State<Runtime>, Caller(p): Caller, Body(b): Body<SourceIn>) -> R {
    if !sources::SOURCE_KINDS.contains(&b.kind.as_str()) {
        return Err(Error::Invalid(format!("unknown source kind {:?}", b.kind)).into());
    }
    let label = check_label(&b.label)?;
    let config = b.config.unwrap_or_else(|| json!({}));
    check_config(&b.kind, &config)?;
    if needs_profile(&b.kind) && b.profile_id.is_none() {
        return Err(Error::Invalid(format!("a source of kind {} needs profile_id", b.kind)).into());
    }
    let purpose = secret_purpose(&b.kind);
    if b.secret.is_some() && purpose.is_none() {
        return Err(Error::Invalid(format!("a source of kind {} keeps no secret", b.kind)).into());
    }
    let poll_seconds = match b.poll_seconds {
        None => default_poll_seconds(&b.kind),
        Some(v) => v,
    };
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    if let Some(profile) = b.profile_id {
        manages_profile(&mut tx, &p, profile).await?;
    }
    if b.kind == "paperless" {
        for profile in tag_profiles(&config) {
            manages_profile(&mut tx, &p, profile).await?;
        }
    }
    let secret_id = match (purpose, b.secret.filter(|s| !s.is_empty())) {
        (Some(purpose), Some(value)) => {
            let sb = SecretBox::from_settings(&rt.settings)?;
            Some(secrets::store_secret(&mut tx, &sb, household, purpose, None, &SecretString::new(value)).await?)
        }
        _ => None,
    };
    let id = sources::create_source(
        &mut tx,
        &NewSource {
            profile_id: b.profile_id,
            config,
            secret_id,
            poll_seconds,
            created_by: p.user_id,
            ..NewSource::new(household, &b.kind, &label)
        },
    )
    .await?;
    let mut s = sources::get_source(&mut tx, id)
        .await?
        .ok_or_else(|| Error::NotFound("Unknown source.".into()))?;
    if b.enabled == Some(false) {
        s = sources::update_source(
            &mut tx,
            id,
            &SourcePatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    }
    audit(&mut tx, &p, "source.create", &s).await?;
    let out = out_one(&mut tx, &s).await?;
    tx.commit().await?;
    Ok(auth::answer(StatusCode::CREATED, out, &[]))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchIn {
    #[serde(default)]
    label: Option<String>,
    #[serde(default, deserialize_with = "present")]
    profile_id: Option<Option<Uuid>>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default, deserialize_with = "present")]
    poll_seconds: Option<Option<i32>>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn patch(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>, Body(b): Body<PatchIn>) -> R {
    let mut tx = rt.pool.begin().await?;
    let s = managed_source(&mut tx, &p, &id).await?;
    let label = b.label.as_deref().map(check_label).transpose()?;
    if let Some(c) = &b.config {
        check_config(&s.kind, c)?;
        // the config of a kind with a secret holds where it goes and, for mail, which
        // Authentication-Results count: widening either is a step-up action, like a sender rule
        if secret_purpose(&s.kind).is_some() {
            sessions::require_step_up(&mut tx, &p, sessions::STEP_UP_SECONDS).await?;
            let fresh_secret = b.secret.as_deref().is_some_and(|v| !v.is_empty());
            if destination(&s.kind, c) != destination(&s.kind, &s.config) && !fresh_secret {
                return Err(
                    Error::Invalid("the server address changed: enter the password or token again".into()).into(),
                );
            }
        }
        if s.kind == "paperless" {
            for profile in tag_profiles(c) {
                manages_profile(&mut tx, &p, profile).await?;
            }
        }
    }
    if let Some(profile) = b.profile_id {
        match profile {
            Some(profile) => manages_profile(&mut tx, &p, profile).await?,
            None if needs_profile(&s.kind) => {
                return Err(Error::Invalid(format!("a source of kind {} needs profile_id", s.kind)).into());
            }
            None => {}
        }
    }
    let mut secret_id = None;
    if let Some(value) = b.secret.filter(|v| !v.is_empty()) {
        let purpose = secret_purpose(&s.kind)
            .ok_or_else(|| Error::Invalid(format!("a source of kind {} keeps no secret", s.kind)))?;
        let sb = SecretBox::from_settings(&rt.settings)?;
        let value = SecretString::new(value);
        match s.secret_id {
            Some(old) => secrets::replace_secret(&mut tx, &sb, old, &value).await?,
            None => {
                secret_id = Some(Some(
                    secrets::store_secret(&mut tx, &sb, s.household_id, purpose, None, &value).await?,
                ));
            }
        }
    }
    let updated = sources::update_source(
        &mut tx,
        s.id,
        &SourcePatch {
            label,
            profile_id: b.profile_id,
            config: b.config,
            secret_id,
            enabled: b.enabled,
            poll_seconds: b.poll_seconds,
        },
    )
    .await?;
    audit(&mut tx, &p, "source.update", &updated).await?;
    let out = out_one(&mut tx, &updated).await?;
    tx.commit().await?;
    ok(out)
}

/// Deletes the source and the secrets it holds (its own, and its sender rules' PDF passwords).
/// Documents it brought stay; their `source_id` turns null.
async fn remove(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let mut tx = rt.pool.begin().await?;
    let s = managed_source(&mut tx, &p, &id).await?;
    let pdf_secrets: Vec<Uuid> = sqlx::query_scalar(
        "select pr.secret_id from pdf_password_rule pr join sender_rule r on r.id = pr.sender_rule_id          where r.source_id = $1",
    )
    .bind(s.id)
    .fetch_all(&mut *tx)
    .await?;
    sources::delete_source(&mut tx, s.id).await?;
    for secret in pdf_secrets.into_iter().chain(s.secret_id) {
        secrets::delete_secret(&mut tx, s.household_id, secret).await?;
    }
    audit(&mut tx, &p, "source.delete", &s).await?;
    tx.commit().await?;
    ok(json!({"ok": true}))
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<i64>,
}

async fn runs(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>, Query(q): Query<RunsQuery>) -> R {
    let mut conn = rt.pool.acquire().await?;
    let s = managed_source(&mut conn, &p, &id).await?;
    let runs = sources::list_runs(&mut conn, s.id, q.limit.unwrap_or(10)).await?;
    ok(serde_json::to_value(runs).map_err(Error::from)?)
}

/// A new push token, shown once. It replaces the old one, so it needs step-up.
async fn token(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let mut tx = rt.pool.begin().await?;
    let s = managed_source(&mut tx, &p, &id).await?;
    sessions::require_step_up(&mut tx, &p, sessions::STEP_UP_SECONDS).await?;
    let token = sources::issue_token(&mut tx, s.id).await?;
    audit(&mut tx, &p, "source.token", &s).await?;
    tx.commit().await?;
    // `oh_ing_<id8>`: the secret after it may itself hold `_`
    let prefix: String = token.chars().take(TOKEN_PREFIX.len() + 8).collect();
    ok(json!({"token": token, "prefix": prefix}))
}

/// Poll a source now. Push kinds have nothing to poll.
async fn sync(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let mut conn = rt.pool.acquire().await?;
    let s = managed_source(&mut conn, &p, &id).await?;
    if default_poll_seconds(&s.kind).is_none() {
        return Err(Error::Invalid("this source receives pushes; there is nothing to poll".into()).into());
    }
    if !s.enabled {
        return Err(Error::Invalid("the source is switched off".into()).into());
    }
    let job_id = sources::enqueue_poll(&mut conn, s.id).await?;
    Ok(auth::answer(StatusCode::ACCEPTED, json!({"job_id": job_id}), &[]))
}

// ---------------------------------------------------------------- health ----

/// The Prometheus text exposition format, version 0.0.4.
pub const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Debug, Default, Deserialize)]
struct HealthQuery {
    format: Option<String>,
}

/// Who asks for source health: a person, or a source's own token (a monitor that holds only it).
enum HealthCaller {
    Person(Caller),
    Source(Source),
}

impl FromRequestParts<Runtime> for HealthCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, rt: &Runtime) -> Result<Self, Self::Rejection> {
        let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
        let source_token = header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| t.starts_with(TOKEN_PREFIX))
            || header("x-ownhealth-secret").is_some();
        if source_token {
            let SourceCaller(s) = SourceCaller::from_request_parts(parts, rt).await?;
            Ok(HealthCaller::Source(s))
        } else {
            Ok(HealthCaller::Person(Caller::from_request_parts(parts, rt).await?))
        }
    }
}

/// The household's sources this person may see: those of profiles they can open, and the
/// household-wide ones (no profile) when they own or guard at least one profile.
async fn visible(conn: &mut PgConnection, caller: &Caller) -> Result<Vec<SourceHealth>, ApiError> {
    let Some(household_id) = caller.0.household_id else {
        return Err(ApiError::unauthenticated());
    };
    let perms = accessible_profiles(conn, &caller.0).await?;
    let manager = perms.iter().any(|p| matches!(p.role.as_str(), "owner" | "guardian"));
    let profiles: HashSet<Uuid> = perms.iter().map(|p| p.profile.id).collect();
    Ok(sources::source_health(conn, household_id)
        .await?
        .into_iter()
        .filter(|h| h.profile_id.map_or(manager, |p| profiles.contains(&p)))
        .collect())
}

async fn health_all(State(rt): State<Runtime>, who: HealthCaller, Query(q): Query<HealthQuery>) -> Response {
    let rows = async {
        let mut conn = rt.pool.acquire().await?;
        match who {
            HealthCaller::Person(c) => visible(&mut conn, &c).await,
            HealthCaller::Source(s) => Ok(sources::source_health(&mut conn, s.household_id)
                .await?
                .into_iter()
                .filter(|h| h.id == s.id)
                .collect()),
        }
    };
    match rows.await {
        Ok(rows) => answer(&rows, &rows, &q),
        Err(e) => e.into_response(),
    }
}

async fn health_one(
    State(rt): State<Runtime>,
    who: HealthCaller,
    Path(id): Path<String>,
    Query(q): Query<HealthQuery>,
) -> Response {
    let row = async {
        let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
        let mut conn = rt.pool.acquire().await?;
        let rows = match who {
            HealthCaller::Person(c) => visible(&mut conn, &c).await?,
            // a token answers for its own source only; another id is not found, not forbidden
            HealthCaller::Source(s) if s.id == id => sources::source_health(&mut conn, s.household_id).await?,
            HealthCaller::Source(_) => Vec::new(),
        };
        rows.into_iter().find(|h| h.id == id).ok_or_else(ApiError::not_found)
    };
    match row.await {
        Ok(row) => answer(&row, std::slice::from_ref(&row), &q),
        Err(e) => e.into_response(),
    }
}

/// `json` as JSON, or `rows` as Prometheus text.
fn answer<T: serde::Serialize>(json: &T, rows: &[SourceHealth], q: &HealthQuery) -> Response {
    match q.format.as_deref() {
        None | Some("json") => Json(json).into_response(),
        Some("prometheus") => ([(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], prometheus(rows)).into_response(),
        Some(_) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "format must be json or prometheus.",
        )
        .into_response(),
    }
}

/// The health rows as Prometheus gauges. Labels are the source id and kind only: never the label,
/// the profile or the last error, which are the user's words (architecture.md section 15).
pub fn prometheus(rows: &[SourceHealth]) -> String {
    type Metric = (&'static str, &'static str, fn(&SourceHealth) -> f64);
    let metrics: [Metric; 6] = [
        ("ownhealth_source_enabled", "1 when the source is switched on.", |h| {
            f64::from(u8::from(h.enabled))
        }),
        (
            "ownhealth_source_stale",
            "1 when an enabled polled source has had no successful sync for 3 poll intervals, or never.",
            |h| f64::from(u8::from(h.stale)),
        ),
        (
            "ownhealth_source_consecutive_failures",
            "Failed runs since the last successful one.",
            |h| f64::from(h.consecutive_failures),
        ),
        (
            "ownhealth_source_last_run_ok",
            "1 when the newest finished run brought everything in (or no run has finished yet); 0 when it was partial or failed.",
            |h| f64::from(u8::from(h.last_run_status.as_deref().is_none_or(|s| s == "ok"))),
        ),
        (
            "ownhealth_source_last_success_timestamp_seconds",
            "Unix time of the last successful sync; 0 means never.",
            |h| unix(h.last_success_at),
        ),
        (
            "ownhealth_source_last_run_timestamp_seconds",
            "Unix time of the last run, successful or not; 0 means never.",
            |h| unix(h.last_run_at),
        ),
    ];
    let mut out = String::new();
    for (name, help, value) in metrics {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
        for h in rows {
            let _ = writeln!(
                out,
                "{name}{{source=\"{}\",kind=\"{}\"}} {}",
                h.id,
                escape(&h.kind),
                value(h)
            );
        }
    }
    out
}

/// Unix seconds; 0 for never.
fn unix(t: Option<chrono::DateTime<chrono::Utc>>) -> f64 {
    t.map_or(0.0, |t| t.timestamp_millis() as f64 / 1000.0)
}

/// A label value: backslash, double quote and newline escaped.
fn escape(v: &str) -> String {
    v.replace('\\', r"\\").replace('"', "\\\"").replace('\n', r"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn row(kind: &str) -> SourceHealth {
        SourceHealth {
            id: Uuid::nil(),
            kind: kind.into(),
            label: "Correo de Alex \"labs\"".into(),
            profile_id: Some(Uuid::max()),
            enabled: true,
            last_run_at: Utc.timestamp_opt(1_762_700_000, 0).single(),
            last_success_at: None,
            last_error_at: None,
            last_error: Some("LOGIN failed for alex.rivera".into()),
            consecutive_failures: 2,
            stale: true,
            last_run_status: Some("partial".into()),
            last_run_problems: 1,
            last_run_problem: Some("move_failed: could not move scan.pdf".into()),
        }
    }

    #[test]
    fn prometheus_text_carries_ids_and_kinds_only() {
        let text = prometheus(&[row("email")]);
        let labels = r#"{source="00000000-0000-0000-0000-000000000000",kind="email"}"#;
        assert!(text.contains(&format!("ownhealth_source_stale{labels} 1\n")), "{text}");
        assert!(text.contains(&format!("ownhealth_source_consecutive_failures{labels} 2\n")));
        assert!(
            text.contains(&format!("ownhealth_source_last_run_ok{labels} 0\n")),
            "{text}"
        );
        assert!(text.contains(&format!("ownhealth_source_last_success_timestamp_seconds{labels} 0\n")));
        assert!(text.contains(&format!(
            "ownhealth_source_last_run_timestamp_seconds{labels} 1762700000\n"
        )));
        assert!(text.contains("# TYPE ownhealth_source_enabled gauge\n"));
        for private in ["Correo", "alex", "ffffffff", "scan.pdf"] {
            assert!(!text.contains(private), "{private} leaked: {text}");
        }
        // every sample line is `name{labels} value`
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let (_, value) = line.rsplit_once(' ').unwrap_or_default();
            assert!(value.parse::<f64>().is_ok(), "{line}");
        }
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape("a\"b\\c\nd"), r#"a\"b\\c\nd"#);
    }
}
