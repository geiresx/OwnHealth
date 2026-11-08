//! Lane E: rules and state (`state/`, `rules/`; architecture.md 11, mcp.md 4, 5.6). The rule pack
//! (`data/rules/pack/*.yaml`) and the CPIC snapshot are read unchanged. Canonical
//! JSON and `state_hash` are in `ownhealth_core::canonical` (byte-identical to Python).
//!
//! ```text
//! body     = build_state(conn, profile, as_of, None)   // language-neutral, includes state_hash
//! response = render(&body, lang, units, identity_mode, scopes)
//! ```
//!
//! `status_word` is the single place that maps a value to ok / watch / out / stale / nodata; the UI
//! and MCP never compute it themselves. The body holds codes, parameters and printed values only:
//! language, unit preference, identity mode and the caller are not inputs, so every token sees the
//! same `state_hash` for the same data.

pub mod drugs;
pub mod engine;
pub mod genome;
pub mod glance;
pub mod num;
pub mod pack;
pub mod rendering;
pub mod wording;

use chrono::{DateTime, NaiveDate, Utc};
use futures::future::BoxFuture;
use ownhealth_core::access::{Scopes, today_utc};
use ownhealth_core::jobs::{self, Job, JobContext, JobError};
use ownhealth_core::{Error, Result};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

pub use ownhealth_core::canonical::{canonical_json, state_hash};
pub use pack::{Pack, PackError, cpic_snapshot, load_pack};

pub const STATUS_WORDS: [&str; 5] = ["ok", "watch", "out", "stale", "nodata"];
pub const STATE_SCHEMA: &str = "ownhealth.state/v1";
pub const MAX_STATE_BYTES: usize = 2048;

/// `ownhealth-rules 0.1.0`, or `ownhealth-rules 0.1.0+local.<sha8>` with overrides.
pub fn rules_version() -> Result<String> {
    Ok(load_pack()?.version.clone())
}

/// Drop None values from objects: the body is a byte budget (2 KB).
fn compact(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, compact(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(compact).collect()),
        other => other,
    }
}

fn med(m: &Map<String, Value>) -> Value {
    json!({
        "name": m.get("name"),
        "strength": m.get("strength"),
        "since": m.get("started"),
        "src": m.get("src_id"),
    })
}

/// Project an evaluation to the language-neutral body (no hash). The body repeats nothing the pack
/// already fixes: the kind follows from the reason; severity, the RCV, the trend length and a stale
/// window from the rule; the LOINC code from the group unless the value carried another one.
pub fn body_from(ev: &engine::Evaluation, pack: &Pack) -> Value {
    const DROP: [&str; 5] = ["kind", "severity", "rcv_pct", "n", "window_days"];
    let flags: Vec<Value> = ev
        .flags
        .iter()
        .map(|f| {
            let mut out: Map<String, Value> = f
                .iter()
                .filter(|(k, _)| !DROP.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if f.get("kind").and_then(Value::as_str) == Some("gene_drug") {
                out.shift_remove("date");
            }
            let group = f.get("group").and_then(Value::as_str).unwrap_or_default();
            let loinc = out
                .get("loinc")
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
                .map(str::to_string);
            if loinc.is_some() && loinc == pack.default_loinc(group) {
                out.shift_remove("loinc");
            }
            Value::Object(out)
        })
        .collect();
    let stale: Vec<Value> = ev
        .stale
        .iter()
        .map(|s| {
            Value::Object(
                s.iter()
                    .filter(|(k, _)| !DROP.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
        })
        .collect();
    let sensitive = ev.withheld_sensitive + ev.genome_withheld_sensitive;
    let minor = ev.genome_withheld_minor;
    let withheld = if sensitive != 0 || minor != 0 {
        json!({"sensitive": sensitive, "minor_rule": minor})
    } else {
        Value::Null
    };
    let systems: Map<String, Value> = ev.systems.iter().map(|s| (s.key.clone(), json!(s.status))).collect();
    let body = json!({
        "schema": STATE_SCHEMA,
        "profile": ev.slug,
        "as_of": engine::iso(ev.as_of),
        "rules": pack.version,
        "derivation": if ev.derivation.is_empty() { Value::Null } else { json!(ev.derivation) },
        "flags": flags,
        "stale": stale,
        "never_measured": ev.never_measured,
        "systems": systems,
        "medications": ev.medications.iter().map(med).collect::<Vec<_>>(),
        "supplements": ev.supplements.iter().map(med).collect::<Vec<_>>(),
        "conditions": ev.conditions.iter().map(|c| json!({
            "name": c.get("text"),
            "verification": c.get("verification_status"),
            "since": c.get("onset"),
            "src": c.get("src_id"),
        })).collect::<Vec<_>>(),
        "notes": ev.notes,
        "pending": ev.pending,
        "corrections": ev.corrections,
        "freshness": ev.freshness,
        "withheld": withheld,
    });
    compact(body)
}

/// Upsert the snapshot for (profile, as_of, pack). Writes nothing when the hash is unchanged.
pub async fn store_snapshot(conn: &mut PgConnection, profile_id: Uuid, body: &Value, pack: &Pack) -> Result<bool> {
    let as_of = body
        .get("as_of")
        .and_then(Value::as_str)
        .and_then(engine::parse_date)
        .ok_or_else(|| Error::Invalid("state body without as_of".into()))?;
    let hash = body.get("state_hash").and_then(Value::as_str).unwrap_or_default();
    let hash = hex::decode(hash.split_once(':').map_or(hash, |(_, h)| h)).map_err(|e| Error::Invalid(e.to_string()))?;
    let rules_sha = hex::decode(&pack.sha256).map_err(|e| Error::Invalid(e.to_string()))?;
    let row: Option<i32> = sqlx::query_scalar(
        "insert into state_snapshot (profile_id, as_of, rules_version, rules_sha256, derivation_version, \
         state_hash, body) values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (profile_id, as_of, rules_sha256) do update set state_hash = excluded.state_hash, \
         body = excluded.body, derivation_version = excluded.derivation_version, created_at = now() \
         where state_snapshot.state_hash <> excluded.state_hash returning 1",
    )
    .bind(profile_id)
    .bind(engine::day_end(as_of) - chrono::Duration::days(1))
    .bind(&pack.version)
    .bind(rules_sha)
    .bind(body.get("derivation").and_then(Value::as_str).unwrap_or_default())
    .bind(hash)
    .bind(sqlx::types::Json(body))
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

/// The body and the evaluation behind it: the dashboard renders from the same evaluation the hash
/// covers. Stores the snapshot for the current view (`known_at` None).
pub async fn build_with_evaluation(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    known_at: Option<DateTime<Utc>>,
) -> Result<(Value, engine::Evaluation)> {
    let pack = load_pack()?;
    let ev = engine::evaluate(&mut *conn, profile_id, as_of, &pack, known_at, None).await?;
    let mut body = body_from(&ev, &pack);
    let hash = state_hash(&body)?;
    if let Some(m) = body.as_object_mut() {
        m.insert("state_hash".into(), json!(hash));
    }
    if known_at.is_none() {
        store_snapshot(&mut *conn, profile_id, &body, &pack).await?;
    }
    Ok((body, ev))
}

/// The language-neutral body with `state_hash`, stored in state_snapshot when the hash is new.
/// `known_at` evaluates what the store knew at that moment (rows recorded and not yet corrected
/// then); it selects rows only and is not part of the hash.
pub async fn build_state(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    known_at: Option<DateTime<Utc>>,
) -> Result<Value> {
    Ok(build_with_evaluation(conn, profile_id, as_of, known_at).await?.0)
}

/// Localized text, unit preference and redaction, applied after hashing (never changes the hash).
/// `identity_mode`: full | pseudonymous | minimal. With `scopes`, sections the scopes do not cover
/// are left out and named in `withheld.by_scope`.
pub fn render(body: &Value, lang: &str, units: &str, identity_mode: &str, scopes: Option<&Scopes>) -> Result<Value> {
    rendering::render_with(&*load_pack()?, body, lang, units, identity_mode, scopes)
}

/// ok | watch | out | stale | nodata
pub async fn status_word(
    conn: &mut PgConnection,
    profile_id: Uuid,
    alias_group: &str,
    as_of: NaiveDate,
) -> Result<&'static str> {
    let pack = load_pack()?;
    let ev = engine::evaluate(conn, profile_id, as_of, &pack, None, Some(alias_group)).await?;
    Ok(ev.marker(alias_group).map_or("nodata", |m| m.status))
}

/// The "Health at a glance" page data (ui `Dashboard` type).
pub async fn dashboard(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    lang: &str,
    scopes: &Scopes,
) -> Result<Value> {
    let (body, ev) = build_with_evaluation(&mut *conn, profile_id, as_of, None).await?;
    let pack = load_pack()?;
    glance::build_dashboard(conn, &ev, &body, &pack, lang, scopes).await
}

/// mcp.md 5.6 `data`. An unknown drug is `Error::NotFound` (never "no interaction").
pub async fn check_drug(
    conn: &mut PgConnection,
    profile_id: Uuid,
    drug: &str,
    scopes: &Scopes,
    lang: &str,
    as_of: Option<NaiveDate>,
) -> Result<Value> {
    drugs::check_drug(conn, profile_id, drug, scopes, lang, as_of).await
}

/// es/en generic or brand name, or a CPIC drug id, to `{cpic_drug_id, name}`; None when unknown.
pub async fn resolve_drug(_conn: &mut PgConnection, name: &str) -> Result<Option<Value>> {
    Ok(drugs::resolve(&*load_pack()?, cpic_snapshot()?, name))
}

/// The installed pack for `GET /api/rules`: version, hash and every rule with its citation.
pub fn rules_summary() -> Result<Value> {
    const KEEP: [&str; 10] = [
        "id",
        "kind",
        "file",
        "system",
        "match",
        "window_days",
        "cite",
        "basis",
        "note",
        "verify",
    ];
    let pack = load_pack()?;
    let snap = cpic_snapshot()?;
    let rules: Vec<Value> = pack
        .rules
        .iter()
        .map(|r| {
            Value::Object(
                KEEP.iter()
                    .filter_map(|k| r.get(*k).map(|v| (k.to_string(), v.clone())))
                    .collect(),
            )
        })
        .collect();
    Ok(json!({
        "version": pack.version,
        "sha256": pack.sha256,
        "systems": pack.systems.iter().map(|s| json!({"key": s.key, "file": s.file, "name": s.names, "source": s.source})).collect::<Vec<_>>(),
        "rules": rules,
        "cpic": {"source": snap.get("source"), "license": snap.get("license"), "retrieved": snap.get("retrieved")},
    }))
}

/// Job `state.rebuild`: one profile, or every profile not deceased, as of today (or `as_of`). A
/// rebuild with an unchanged hash writes nothing new.
pub async fn rebuild(ctx: &JobContext, payload: &Value) -> std::result::Result<Value, JobError> {
    let bad = |e: String| JobError::permanent(format!("bad payload: {e}"));
    let present = |k: &str| payload.get(k).filter(|v| pack::truthy(v));
    let as_of = match present("as_of") {
        Some(v) => NaiveDate::parse_from_str(&pack::text(v), "%Y-%m-%d")
            .map_err(|_| bad(format!("Invalid isoformat string: '{}'", pack::text(v))))?,
        None => today_utc(),
    };
    let pid = match present("profile_id") {
        Some(v) => Some(Uuid::parse_str(&pack::text(v)).map_err(|e| bad(e.to_string()))?),
        None => None,
    };
    let mut tx = ctx.pool.begin().await.map_err(Error::from)?;
    let ids: Vec<Uuid> = match pid {
        Some(p) => vec![p],
        None => sqlx::query_scalar("select id from profile where not deceased order by id")
            .fetch_all(&mut *tx)
            .await
            .map_err(Error::from)?,
    };
    let mut hashes = Map::new();
    for id in ids {
        let body = build_state(&mut tx, id, as_of, None).await?;
        hashes.insert(id.to_string(), body.get("state_hash").cloned().unwrap_or(Value::Null));
    }
    tx.commit().await.map_err(Error::from)?;
    Ok(json!({"as_of": engine::iso(as_of), "profiles": hashes.len(), "hashes": hashes}))
}

fn rebuild_job(ctx: JobContext, job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(async move { rebuild(&ctx, &job.payload).await.map(Some) })
}

/// Register lane E's job handler (`state.rebuild`). Lane E has no demo data of its own.
pub fn register() {
    jobs::register_handler("state.rebuild", rebuild_job);
}
