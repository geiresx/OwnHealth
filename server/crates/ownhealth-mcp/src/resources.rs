//! MCP resources (`mcp/resources.py`, mcp.md 7). Each read goes through the same rate bucket,
//! permission, fact gate, identity mode, egress budget and audit as a tool call; every result is
//! `cacheScope: private`.
//!
//! | URI                                   | Content                                   | Needs                      |
//! |---------------------------------------|-------------------------------------------|----------------------------|
//! | ownhealth://profiles                  | profiles this token may read              | read:summary on one        |
//! | ownhealth://profile/{id}/status       | get_health_status JSON                    | read:summary               |
//! | ownhealth://profile/{id}/status.md    | its markdown text block                   | read:summary               |
//! | ownhealth://source/{src_id}           | get_source without text                   | the fact's scope           |
//! | ownhealth://rules/{version}           | the rule pack in force                    | a valid token              |
//! | ownhealth://profile/{id}/access-log   | this profile's audit entries, 30 days     | owner or guardian          |

use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::access::{Permission, Principal, ProfileRef, accessible_profiles, require};
use ownhealth_core::audit::write_audit;
use ownhealth_core::pyjson::py_date;
use ownhealth_core::text::untrusted;
use ownhealth_core::{Error, Runtime};
use regex::Regex;
use serde_json::{Map, Value, json};
use sqlx::{Connection, PgConnection};

use crate::envelope::{ToolError, collect_src_ids, count_facts, dumps, py_float, query_hash, z_time};
use crate::limits::{self, Limits, Usage};
use crate::redact;
use crate::services::Services;
use crate::tools::{AsOf, Call, Fail, Meta, db_now, finish, parse_as_of, token_event, union_scopes};

pub const MINUTE: u64 = 60_000;
pub const DAY: u64 = 86_400_000;
pub const RESOURCE_NOT_FOUND: i32 = -32002;
pub const INVALID_PARAMS: i32 = -32602;

#[allow(clippy::unwrap_used)]
static PROFILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ownhealth://profile/([a-z0-9_-]+)/(status|status\.md|access-log)$").unwrap());
#[allow(clippy::unwrap_used)]
static SOURCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ownhealth://source/(src_[a-z0-9_]+)$").unwrap());
#[allow(clippy::unwrap_used)]
static RULES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ownhealth://rules/(.+)$").unwrap());

fn resource(uri: String, name: String, mime: &str, description: &str) -> Value {
    json!({"uri": uri, "name": name, "mimeType": mime, "description": description})
}

/// The resources/list entries for this token.
pub async fn list_resources(
    conn: &mut PgConnection,
    svc: &dyn Services,
    principal: &Principal,
) -> Result<Vec<Value>, Error> {
    let mut out = Vec::new();
    if union_scopes(&mut *conn, principal).await?.contains("read:summary") {
        out.push(resource(
            "ownhealth://profiles".into(),
            "profiles".into(),
            "application/json",
            "Profiles this token may read: slug, relationship, age band, data freshness.",
        ));
    }
    out.push(resource(
        format!("ownhealth://rules/{}", svc.rules_version()?),
        "rules".into(),
        "application/json",
        "The rule pack in force, to explain a flag by rule id.",
    ));
    for perm in accessible_profiles(&mut *conn, principal).await? {
        let slug = &perm.profile.slug;
        if perm.has("read:summary") {
            out.push(resource(
                format!("ownhealth://profile/{slug}/status"),
                format!("{slug} status"),
                "application/json",
                "The health status of this profile.",
            ));
            out.push(resource(
                format!("ownhealth://profile/{slug}/status.md"),
                format!("{slug} status (text)"),
                "text/markdown",
                "The health status as markdown.",
            ));
        }
        if perm.role == "owner" || perm.role == "guardian" {
            out.push(resource(
                format!("ownhealth://profile/{slug}/access-log"),
                format!("{slug} access log"),
                "application/json",
                "Who read this profile, 30 days.",
            ));
        }
    }
    Ok(out)
}

pub fn list_templates() -> Vec<Value> {
    let t = |uri: &str, name: &str, mime: &str, description: &str| json!({"uriTemplate": uri, "name": name, "mimeType": mime, "description": description});
    vec![
        t(
            "ownhealth://source/{src_id}",
            "source",
            "application/json",
            "Where a cited value came from (get_source without text).",
        ),
        t(
            "ownhealth://profile/{id}/status",
            "status",
            "application/json",
            "The health status of one profile.",
        ),
        t(
            "ownhealth://profile/{id}/status.md",
            "status.md",
            "text/markdown",
            "The health status as markdown.",
        ),
        t(
            "ownhealth://profile/{id}/access-log",
            "access-log",
            "application/json",
            "This profile's audit entries, 30 days.",
        ),
    ]
}

/// An MCP error of a resource read: (code, message).
pub type ReadError = (i32, String);

#[allow(clippy::too_many_arguments)]
async fn make_call<'c>(
    conn: &'c mut PgConnection,
    rt: &'c Runtime,
    svc: &'c dyn Services,
    principal: &'c Principal,
    perm: Option<Permission>,
    args: Map<String, Value>,
    tool: &str,
    usage: Usage,
    lim: Limits,
) -> Result<Call<'c>, Fail> {
    let as_of = parse_as_of(&mut *conn, None).await?;
    let at = db_now(&mut *conn).await?;
    Ok(Call {
        conn,
        rt,
        svc,
        principal,
        perm,
        args,
        as_of: AsOf {
            day: as_of.day,
            at: Some(at),
            label: as_of.label,
        },
        lang: if ["es", "en"].contains(&principal.lang.as_str()) {
            principal.lang.clone()
        } else {
            "es".into()
        },
        units: principal.units.clone(),
        mode: redact::stricter(&principal.identity_mode, None),
        offset: 0,
        inner_cursor: None,
        usage,
        limits: lim,
        tool: tool.into(),
    })
}

async fn profiles(conn: &mut PgConnection, principal: &Principal) -> Result<Value, Error> {
    let today = limits::today(&mut *conn).await?;
    let mut items = Vec::new();
    for perm in accessible_profiles(&mut *conn, principal).await? {
        if !perm.has("read:summary") {
            continue;
        }
        let p = &perm.profile;
        let mut fresh = Map::new();
        for (label, cat, scope) in [
            ("labs", "laboratory", "read:labs"),
            ("vitals", "vital-signs", "read:vitals"),
        ] {
            if perm.has(scope) {
                let d: Option<NaiveDate> = sqlx::query_scalar(
                    "select max(effective_at)::date from observation where profile_id = $1 and category = $2 \
                     and status = 'current'",
                )
                .bind(p.id)
                .bind(cat)
                .fetch_one(&mut *conn)
                .await?;
                fresh.insert(label.into(), json!(d.map(|d| py_date(&d))));
            }
        }
        if perm.has("read:wearables") {
            let d: Option<NaiveDate> =
                sqlx::query_scalar("select max(t)::date from wearable_rollup where profile_id = $1")
                    .bind(p.id)
                    .fetch_one(&mut *conn)
                    .await?;
            fresh.insert("wearables".into(), json!(d.map(|d| py_date(&d))));
        }
        let mut scopes: Vec<&String> = perm.scopes.iter().collect();
        scopes.sort();
        let mut item = Map::new();
        item.insert("slug".into(), json!(p.slug));
        item.insert("relationship".into(), json!(p.relationship));
        item.insert("role".into(), json!(perm.role));
        item.insert("age_band".into(), json!(redact::age_band(p.age_on(today))));
        item.insert("scopes".into(), json!(scopes));
        item.insert("data_freshness".into(), Value::Object(fresh));
        item.insert("default".into(), json!(Some(p.id) == principal.default_profile));
        if principal.identity_mode == "full" {
            item.insert("display_name".into(), json!(p.display_name));
        }
        items.push(Value::Object(item));
    }
    Ok(json!({"profiles": items}))
}

async fn access_log(conn: &mut PgConnection, perm: &Permission) -> Result<Value, Error> {
    type Row = (
        DateTime<Utc>,
        String,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<Value>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select ts, actor_kind, client, via, action, tool, result_kind, rows_out, withheld from audit_event \
         where profile_id = $1 and ts > now() - interval '30 days' order by id desc limit 200",
    )
    .bind(perm.profile.id)
    .fetch_all(&mut *conn)
    .await?;
    let mut entries = Vec::new();
    for (ts, actor_kind, client, via, action, tool, result_kind, rows_out, withheld) in rows {
        let client = match client.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => untrusted(Some(c), "person_note")?,
            None => Value::Null,
        };
        entries.push(json!({
            "ts": z_time(&ts),
            "actor_kind": actor_kind,
            "client": client,
            "via": via,
            "action": action,
            "tool": tool,
            "result_kind": result_kind,
            "rows_out": rows_out,
            "withheld": withheld,
        }));
    }
    Ok(json!({"entries": entries, "days": 30}))
}

/// `uri.split("/")[2]`.
fn resource_type(uri: &str) -> Option<String> {
    uri.split('/').nth(2).map(str::to_string)
}

fn tool_err(code: &str, message: &str) -> Fail {
    Fail::Tool(Box::new(ToolError::new(code, message, None)))
}

/// One resources/read: the ReadResourceResult as JSON, or an MCP error.
pub async fn read(
    rt: &Runtime,
    svc: &dyn Services,
    uri: &str,
    principal: &Principal,
    meta: &Meta,
) -> Result<Value, ReadError> {
    let internal = |e: Error| -> ReadError {
        tracing::error!(error = %e, "resources/read failed");
        (-32603, "Internal error".into())
    };
    let token_id = principal.token_id.unwrap_or_default();
    let mut conn = rt.pool.acquire().await.map_err(|e| internal(Error::Db(e)))?;
    let (lim, day, wait) = async {
        let mut tx = conn.begin().await?;
        let lim = limits::limits_of(&mut tx, principal.token_id).await?;
        let day = limits::today(&mut tx).await?;
        let wait = limits::take_call(&mut tx, token_id, lim.rate_per_min, None).await?;
        tx.commit().await?;
        Ok::<_, Error>((lim, day, wait))
    }
    .await
    .map_err(internal)?;
    if let Some(w) = wait {
        return Err((INVALID_PARAMS, format!("rate_limited: retry in {} s", py_float(w))));
    }
    let outcome = async {
        let mut tx = conn.begin().await?;
        let r = read_in(&mut tx, rt, svc, uri, principal, meta, lim, day).await;
        if r.is_ok() {
            tx.commit().await?;
        }
        Ok::<_, Error>(r)
    }
    .await
    .map_err(internal)?;
    let code = match outcome {
        Ok(v) => return Ok(v),
        Err(Fail::Tool(t)) => t.code,
        Err(Fail::Core(Error::AccessDenied { code, .. })) => code,
        Err(Fail::Core(e)) => return Err(internal(e)),
    };
    async {
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
        let mut e = token_event(principal, meta, "resources/read".into());
        e.resource_type = if uri.matches('/').count() >= 2 {
            resource_type(uri)
        } else {
            None
        };
        e.resource_ids = Some(vec![uri.chars().take(200).collect()]);
        e.result_kind = Some("denied".into());
        write_audit(&mut tx, &e).await?;
        tx.commit().await?;
        Ok::<_, Error>(())
    }
    .await
    .map_err(internal)?;
    // one answer for every refusal, so a uri never tells whether its target exists
    Err((
        RESOURCE_NOT_FOUND,
        format!("Resource not available to this token ({code})."),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn read_in(
    conn: &mut PgConnection,
    rt: &Runtime,
    svc: &dyn Services,
    uri: &str,
    principal: &Principal,
    meta: &Meta,
    lim: Limits,
    day: NaiveDate,
) -> Result<Value, Fail> {
    let token_id = principal.token_id.unwrap_or_default();
    let usage = limits::usage(&mut *conn, token_id, day).await?;
    let mut mime = "application/json";
    let mut ttl = MINUTE;
    let mut profile_id = None;
    let content: Value;
    let body: String;
    let qhash = || {
        query_hash(&[
            json!(uri),
            json!(principal.token_id.map_or_else(|| "None".to_string(), |t| t.to_string())),
        ])
    };
    let secret = rt.settings.secret_key.clone();
    if uri == "ownhealth://profiles" {
        if !union_scopes(&mut *conn, principal).await?.contains("read:summary") {
            return Err(tool_err("scope_missing", "needs read:summary"));
        }
        content = profiles(&mut *conn, principal).await?;
        body = dumps(&content);
    } else if let Some(m) = RULES_RE.captures(uri) {
        if m[1] != svc.rules_version()? {
            return Err(tool_err("unknown", "no such rule pack version"));
        }
        // the files in force, local overrides included
        content = svc.rule_pack()?;
        body = dumps(&content);
        ttl = DAY;
    } else if let Some(m) = SOURCE_RE.captures(uri) {
        let src_id = m[1].to_string();
        let mut args = Map::new();
        args.insert("src_id".into(), json!(src_id));
        let mut call = make_call(&mut *conn, rt, svc, principal, None, args, "get_source", usage, lim).await?;
        let answer = crate::tools::source::source_data(&mut call, &src_id, false).await?;
        let (env, _) = finish(&mut call, answer, &qhash(), &secret).await?;
        profile_id = call.perm.as_ref().map(|p| p.profile.id);
        body = dumps(&env);
        content = env;
        ttl = DAY;
    } else if let Some(m) = PROFILE_RE.captures(uri) {
        let slug = m[1].to_string();
        let what = m[2].to_string();
        let perm = require(&mut *conn, principal, ProfileRef::Name(&slug), None, None).await?;
        profile_id = Some(perm.profile.id);
        if what == "access-log" {
            if perm.role != "owner" && perm.role != "guardian" {
                return Err(tool_err("scope_missing", "owner or guardian only"));
            }
            content = access_log(&mut *conn, &perm).await?;
            body = dumps(&content);
        } else {
            if !perm.has("read:summary") {
                return Err(tool_err("scope_missing", "needs read:summary"));
            }
            let mut args = Map::new();
            args.insert("profile".into(), json!(slug));
            let mut call = make_call(
                &mut *conn,
                rt,
                svc,
                principal,
                Some(perm),
                args,
                "get_health_status",
                usage,
                lim,
            )
            .await?;
            let answer = crate::tools::status::get_health_status(&mut call).await?;
            let (env, text_block) = finish(&mut call, answer, &qhash(), &secret).await?;
            if what == "status.md" {
                body = text_block;
                mime = "text/markdown";
            } else {
                body = dumps(&env);
            }
            content = env;
        }
    } else {
        return Err(tool_err("unknown", "unknown resource"));
    }

    let facts = count_facts(&content) as i64;
    let nbytes = body.len() as i64;
    if usage.facts + facts > lim.egress_facts_per_day || usage.bytes + nbytes > lim.egress_bytes_per_day {
        return Err(tool_err("egress_budget_exhausted", "daily budget used up"));
    }
    limits::add_usage(
        &mut *conn,
        token_id,
        day,
        Usage {
            calls: 1,
            facts,
            bytes: nbytes,
            ..Usage::default()
        },
    )
    .await?;
    let mut e = token_event(principal, meta, "resources/read".into());
    e.resource_type = resource_type(uri);
    e.resource_ids = Some(vec![uri.chars().take(200).collect()]);
    e.profile_id = profile_id;
    let srcs: Vec<String> = collect_src_ids(&content).into_iter().take(500).collect();
    e.src_ids = (!srcs.is_empty()).then_some(srcs);
    e.rows_out = Some(i32::try_from(facts).unwrap_or(i32::MAX));
    e.bytes_out = Some(i32::try_from(nbytes).unwrap_or(i32::MAX));
    write_audit(&mut *conn, &e).await?;
    Ok(json!({
        "contents": [{"uri": uri, "mimeType": mime, "text": body}],
        "ttlMs": ttl,
        "cacheScope": "private",
    }))
}
