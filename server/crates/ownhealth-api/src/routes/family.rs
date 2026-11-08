//! Lane G: `api/family.py`. Household: members, relatives without a login, invites, grants, locks,
//! sensitive switches, deletion.
//!
//! Changes that widen access (a grant, removing a lock, turning a sensitive switch on) and deletions
//! need step-up; changes that only narrow access do not.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use ownhealth_auth::household::{self, Deleted};
use ownhealth_auth::{AuthError, sessions};
use ownhealth_core::Runtime;
use ownhealth_core::access::Scopes;
use ownhealth_core::access::{
    Permission, Principal, accessible_profiles, all_scopes, effective_permission, revoke_grant, scope_set,
    set_genome_switches, set_grant, set_locks,
};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::pyjson::{parse_datetime, pydantic_datetime_utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Body, Check, Fail, Params, R, answer, date_field, delete_cookie, detail, ok, uuid_param};
use super::core::{local_today, summaries};
use crate::error::ApiError;
use crate::extract::{Caller, SessionCaller};

/// `family._fail`: 409 for `already_claimed`, `last_manager` and `delete_blocked`, else 400.
fn fail(e: AuthError) -> Fail {
    match e {
        AuthError::Refused { code, message, extra } => {
            let status = if matches!(code.as_str(), "already_claimed" | "last_manager" | "delete_blocked") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            let mut err = ApiError::new(status, &code, message);
            err.extra = Box::new(extra);
            Fail::Api(err)
        }
        AuthError::Core(e) => e.into(),
    }
}

struct Audit<'a> {
    action: &'a str,
    tool: &'a str,
    profile_id: Option<Uuid>,
    resource_type: Option<&'a str>,
    resource_ids: Option<Vec<String>>,
    args_summary: Option<String>,
    rows_out: Option<i32>,
}

impl<'a> Audit<'a> {
    fn new(action: &'a str, tool: &'a str, profile_id: Option<Uuid>) -> Audit<'a> {
        Audit {
            action,
            tool,
            profile_id,
            resource_type: None,
            resource_ids: None,
            args_summary: None,
            rows_out: None,
        }
    }
}

async fn audit(conn: &mut PgConnection, p: &Principal, a: Audit<'_>) -> R<()> {
    write_audit(
        conn,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some(a.tool.into()),
            profile_id: a.profile_id,
            resource_type: a.resource_type.map(str::to_string),
            resource_ids: a.resource_ids,
            args_summary: a.args_summary,
            rows_out: a.rows_out,
            ..AuditEvent::new("user", "ui", a.action)
        },
    )
    .await?;
    Ok(())
}

async fn manage(conn: &mut PgConnection, p: &Principal, r: &str) -> R<Permission> {
    let perm = Caller(p.clone()).require(conn, r, None, None).await?;
    Ok(household::require_manage(perm)?)
}

async fn step_up(conn: &mut PgConnection, p: &Principal) -> R<()> {
    sessions::require_step_up(conn, p, sessions::STEP_UP_SECONDS).await?;
    Ok(())
}

// ------------------------------------------------------------------- read ----

#[derive(sqlx::FromRow)]
struct Member {
    id: Uuid,
    display_name: String,
    locale: String,
    passkeys: i64,
    oidc: bool,
    password: bool,
    invite_pending: bool,
    own_profile: Option<String>,
}

#[derive(sqlx::FromRow)]
struct GrantRow {
    user_id: Uuid,
    slug: String,
    role: String,
    categories: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    granted_at: DateTime<Utc>,
}

async fn get_household(State(rt): State<Runtime>, SessionCaller(p): SessionCaller) -> R {
    let mut tx = rt.pool.begin().await?;
    let perms = accessible_profiles(&mut tx, &p).await?;
    let managed: Vec<Uuid> = perms
        .iter()
        .filter(|x| household::can_manage(x))
        .map(|x| x.profile.id)
        .collect();
    let members: Vec<Member> = sqlx::query_as(
        "select u.id, u.display_name, u.locale, \
         (select count(*) from passkey k where k.user_id = u.id) as passkeys, \
         u.oidc_subject is not null as oidc, u.password_hash is not null as password, \
         exists (select 1 from auth_challenge c where c.user_id = u.id and c.purpose = 'invite' \
           and c.used_at is null and c.expires_at > now()) as invite_pending, \
         (select p.slug from profile p where p.user_id = u.id) as own_profile \
         from app_user u where u.household_id = $1 and u.disabled_at is null order by u.created_at, u.id",
    )
    .bind(p.household_id)
    .fetch_all(&mut *tx)
    .await?;
    let grants: Vec<GrantRow> = sqlx::query_as(
        "select g.user_id, p.slug, g.role, g.categories, g.expires_at, g.granted_at from profile_grant g \
         join profile p on p.id = g.profile_id where p.household_id = $1 \
         and (g.profile_id = any($2) or g.user_id = $3) order by p.slug, g.user_id",
    )
    .bind(p.household_id)
    .bind(&managed)
    .bind(p.user_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let profiles: Vec<Value> = summaries(&perms, &p, &rt.settings)
        .into_iter()
        .map(|mut row| {
            let id = row["id"].as_str().and_then(|i| Uuid::parse_str(i).ok());
            if let (Some(perm), Some(obj)) = (perms.iter().find(|x| Some(x.profile.id) == id), row.as_object_mut()) {
                obj.insert("can_manage".into(), json!(managed.contains(&perm.profile.id)));
                obj.insert("claimed".into(), json!(perm.profile.user_id.is_some()));
                let locks: Vec<String> = perm.profile.locks().into_iter().collect();
                obj.insert("locks".into(), json!(locks));
                obj.insert(
                    "genome_sensitive_agents".into(),
                    json!(perm.profile.genome_sensitive_agents),
                );
            }
            row
        })
        .collect();
    let members: Vec<Value> = members
        .into_iter()
        .map(|m| {
            json!({
                "user_id": m.id,
                "display_name": m.display_name,
                "locale": m.locale,
                "logins": {"passkeys": m.passkeys, "oidc": m.oidc, "password": m.password},
                "enrolled": m.passkeys > 0 || m.oidc || m.password,
                "invite_pending": m.invite_pending,
                "own_profile": m.own_profile,
                "is_me": Some(m.id) == p.user_id,
            })
        })
        .collect();
    let grants: Vec<Value> = grants
        .into_iter()
        .map(|g| {
            let mut cats = g.categories;
            cats.sort();
            json!({
                "user_id": g.user_id,
                "profile": g.slug,
                "role": g.role,
                "categories": cats,
                "expires_at": g.expires_at.as_ref().map(pydantic_datetime_utc),
                "granted_at": pydantic_datetime_utc(&g.granted_at),
            })
        })
        .collect();
    ok(json!({"members": members, "profiles": profiles, "grants": grants}))
}

// ---------------------------------------------------------- people, invites ----

#[derive(Debug, Deserialize)]
struct RelativeIn {
    display_name: String,
    role: String,
    #[serde(default)]
    relationship: Option<String>,
    #[serde(default)]
    birth_date: Option<String>,
    #[serde(default)]
    sex_at_birth: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    slug: Option<String>,
}

/// A profile without a login (a child, a parent). Its creator is its guardian.
async fn add_relative(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Body(body): Body<RelativeIn>) -> R {
    let mut c = Check::body();
    c.len("display_name", Some(&body.display_name), 1, 80)
        .literal("role", Some(&body.role), &["guardian", "caregiver"])
        .literal(
            "relationship",
            body.relationship.as_deref(),
            &["partner", "child", "parent", "other"],
        );
    let birth = date_field(&mut c, "birth_date", body.birth_date.as_deref());
    c.literal(
        "sex_at_birth",
        body.sex_at_birth.as_deref(),
        &["female", "male", "unknown"],
    )
    .len("country", body.country.as_deref(), 2, 2)
    .len("slug", body.slug.as_deref(), 0, 40)
    .done()?;
    let (Some(household_id), Some(uid)) = (p.household_id, p.user_id) else {
        return Err(ApiError::unauthenticated().into());
    };
    let mut tx = rt.pool.begin().await?;
    let (pid, slug) = household::add_relative(
        &mut tx,
        &household::RelativeIn {
            household_id,
            created_by: uid,
            display_name: &body.display_name,
            role: &body.role,
            relationship: body.relationship.as_deref(),
            birth_date: birth,
            sex_at_birth: body.sex_at_birth.as_deref(),
            country: body.country.as_deref(),
            slug: body.slug.as_deref(),
        },
    )
    .await
    .map_err(fail)?;
    audit(
        &mut tx,
        &p,
        Audit {
            args_summary: Some(format!("create;role={}", body.role)),
            ..Audit::new("grant_change", "POST /api/household/profiles", Some(pid))
        },
    )
    .await?;
    tx.commit().await?;
    Ok(answer(StatusCode::CREATED, json!({"id": pid, "slug": slug}), &[]))
}

#[derive(Debug, Deserialize)]
struct InviteIn {
    display_name: String,
    #[serde(default = "es")]
    locale: String,
    #[serde(default)]
    claim_profile: Option<String>,
}

fn es() -> String {
    "es".into()
}

/// A login for another person, with a one-time link valid 7 days. With `claim_profile` (a profile
/// nobody has claimed yet) that person becomes its owner when they enrol.
async fn invite(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Body(body): Body<InviteIn>) -> R {
    Check::body()
        .len("display_name", Some(&body.display_name), 1, 80)
        .literal("locale", Some(&body.locale), &["es", "en"])
        .done()?;
    let (Some(household_id), Some(uid)) = (p.household_id, p.user_id) else {
        return Err(ApiError::unauthenticated().into());
    };
    let mut tx = rt.pool.begin().await?;
    let perms = accessible_profiles(&mut tx, &p).await?;
    let mut claim_id = None;
    if let Some(claim) = body.claim_profile.as_deref().filter(|c| !c.is_empty()) {
        let perm = Caller(p.clone()).require(&mut tx, claim, None, None).await?;
        if !(household::can_manage(&perm) || perm.role == "caregiver") {
            return Err(detail(
                StatusCode::FORBIDDEN,
                "forbidden",
                "You do not manage this profile.",
            ));
        }
        if !household::claim_invite_allowed(&mut tx, perm.profile.id, uid).await? {
            return Err(detail(
                StatusCode::FORBIDDEN,
                "forbidden",
                "A guardian manages this profile: they send the invite.",
            ));
        }
        claim_id = Some(perm.profile.id);
    } else if !perms.iter().any(|x| x.role == "owner") {
        return Err(detail(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only an owner can invite a member.",
        ));
    }
    let (new_user, code) =
        household::invite_member(&mut tx, household_id, uid, &body.display_name, &body.locale, claim_id)
            .await
            .map_err(fail)?;
    audit(
        &mut tx,
        &p,
        Audit {
            resource_type: Some("user"),
            resource_ids: Some(vec![new_user.to_string()]),
            ..Audit::new("invite", "POST /api/household/invites", claim_id)
        },
    )
    .await?;
    tx.commit().await?;
    Ok(answer(
        StatusCode::CREATED,
        json!({
            "user_id": new_user,
            "code": code,
            "url": format!("{}/enroll?code={code}", rt.settings.base_url),
            "expires_in_days": household::INVITE_TTL_DAYS,
        }),
        &[],
    ))
}

// --------------------------------------------------------------------- grants ----

#[derive(Debug, Deserialize)]
struct GrantIn {
    user_id: String,
    profile: String,
    role: String,
    #[serde(default)]
    categories: Option<Vec<String>>,
    #[serde(default)]
    expires_at: Option<String>,
}

/// A pydantic `datetime`: aware as given, naive read as UTC (the database session runs in UTC).
fn datetime_field(c: &mut Check, field: &str, value: Option<&str>) -> Option<DateTime<Utc>> {
    let v = value?;
    if let Ok(t) = parse_datetime(v) {
        return Some(t.with_timezone(&Utc));
    }
    let t = v.trim().replacen(' ', "T", 1);
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(&t, fmt) {
            return Some(n.and_utc());
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(&t, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|n| n.and_utc());
    }
    c.push(
        "datetime_from_date_parsing",
        field,
        "Input should be a valid datetime or date, invalid character in year",
        json!(v),
    );
    None
}

async fn target_user(conn: &mut PgConnection, p: &Principal, user_id: Uuid) -> R<()> {
    let hit: Option<i32> =
        sqlx::query_scalar("select 1 from app_user where id = $1 and household_id = $2 and disabled_at is null")
            .bind(user_id)
            .bind(p.household_id)
            .fetch_optional(conn)
            .await?;
    if hit.is_none() {
        return Err(detail(
            StatusCode::NOT_FOUND,
            "unknown",
            "No such member in this household.",
        ));
    }
    Ok(())
}

async fn is_owner_grant(conn: &mut PgConnection, user_id: Uuid, profile_id: Uuid) -> R<bool> {
    let role: Option<String> =
        sqlx::query_scalar("select role from profile_grant where user_id = $1 and profile_id = $2")
            .bind(user_id)
            .bind(profile_id)
            .fetch_optional(conn)
            .await?;
    Ok(role.as_deref() == Some("owner"))
}

fn owner_grant() -> Fail {
    detail(
        StatusCode::CONFLICT,
        "owner_grant",
        "The owner's own access cannot change.",
    )
}

/// Python's `sorted(set)` repr: `['a', 'b']`.
fn py_list(items: &Scopes) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("'{s}'")).collect();
    format!("[{}]", quoted.join(", "))
}

async fn put_grant(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Body(body): Body<GrantIn>) -> R {
    let mut c = Check::body();
    let user_id = uuid_param(&mut c, "user_id", Some(&body.user_id));
    c.literal("role", Some(&body.role), &["guardian", "caregiver", "viewer"]);
    let expires_at = datetime_field(&mut c, "expires_at", body.expires_at.as_deref());
    c.done()?;
    let Some(user_id) = user_id else {
        return Err(ApiError::not_found().into());
    };
    let mut tx = rt.pool.begin().await?;
    step_up(&mut tx, &p).await?;
    let perm = manage(&mut tx, &p, &body.profile).await?;
    target_user(&mut tx, &p, user_id).await?;
    if is_owner_grant(&mut tx, user_id, perm.profile.id).await? {
        return Err(owner_grant());
    }
    let cats: Option<Scopes> = body.categories.as_ref().map(scope_set);
    if let Some(cats) = &cats {
        let unknown: Scopes = cats.difference(&all_scopes()).cloned().collect();
        if !unknown.is_empty() {
            return Err(detail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_input",
                &format!("Unknown categories {}", py_list(&unknown)),
            ));
        }
    }
    set_grant(
        &mut tx,
        user_id,
        perm.profile.id,
        &body.role,
        cats.as_ref(),
        p.user_id,
        expires_at,
    )
    .await?;
    let cats_text = cats.as_ref().map_or_else(
        || "default".to_string(),
        |c| c.iter().cloned().collect::<Vec<_>>().join(","),
    );
    audit(
        &mut tx,
        &p,
        Audit {
            resource_type: Some("user"),
            resource_ids: Some(vec![user_id.to_string()]),
            args_summary: Some(format!("role={};categories={cats_text}", body.role)),
            ..Audit::new("grant_change", "PUT /api/grants", Some(perm.profile.id))
        },
    )
    .await?;
    tx.commit().await?;
    ok(json!({"ok": true}))
}

fn required(c: &mut Check, field: &str, value: Option<&String>) -> Option<String> {
    match value {
        Some(v) => Some(v.clone()),
        None => {
            c.push("missing", field, "Field required", Value::Null);
            None
        }
    }
}

async fn delete_grant(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Query(q): Params) -> R {
    let mut c = Check::query();
    let user_id = uuid_param(&mut c, "user_id", q.get("user_id"));
    let profile = required(&mut c, "profile", q.get("profile"));
    c.done()?;
    let (Some(user_id), Some(profile)) = (user_id, profile) else {
        return Err(ApiError::not_found().into());
    };
    let mut tx = rt.pool.begin().await?;
    step_up(&mut tx, &p).await?;
    let perm = manage(&mut tx, &p, &profile).await?;
    if is_owner_grant(&mut tx, user_id, perm.profile.id).await? {
        return Err(owner_grant());
    }
    if !revoke_grant(&mut tx, user_id, perm.profile.id).await? {
        return Err(detail(StatusCode::NOT_FOUND, "unknown", "No such grant."));
    }
    audit(
        &mut tx,
        &p,
        Audit {
            resource_type: Some("user"),
            resource_ids: Some(vec![user_id.to_string()]),
            args_summary: Some("revoke".into()),
            ..Audit::new("grant_change", "DELETE /api/grants", Some(perm.profile.id))
        },
    )
    .await?;
    tx.commit().await?;
    ok(json!({"ok": true}))
}

// ------------------------------------------------------ locks, sensitive switches ----

#[derive(Debug, Deserialize)]
struct LocksIn {
    locks: Vec<String>,
}

/// Categories switched off for everyone but the owner. Removing a lock needs step-up.
async fn put_locks(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Path(profile): Path<String>,
    Body(body): Body<LocksIn>,
) -> R {
    let mut tx = rt.pool.begin().await?;
    let perm = manage(&mut tx, &p, &profile).await?;
    let new = scope_set(&body.locks);
    let unknown: Scopes = new.difference(&all_scopes()).cloned().collect();
    if !unknown.is_empty() {
        return Err(detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_input",
            &format!("Unknown scopes {}", py_list(&unknown)),
        ));
    }
    if perm.profile.locks().difference(&new).next().is_some() {
        step_up(&mut tx, &p).await?;
    }
    set_locks(&mut tx, perm.profile.id, &new).await?;
    let joined = new.iter().cloned().collect::<Vec<_>>().join(",");
    audit(
        &mut tx,
        &p,
        Audit {
            args_summary: Some(joined),
            ..Audit::new("lock_change", "PUT /api/profiles/locks", Some(perm.profile.id))
        },
    )
    .await?;
    tx.commit().await?;
    ok(json!({"ok": true, "locks": new}))
}

#[derive(Debug, Deserialize)]
struct SensitiveIn {
    #[serde(default)]
    ui: Option<bool>,
    #[serde(default)]
    agents: Option<bool>,
}

fn py_opt_bool(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "True",
        Some(false) => "False",
        None => "None",
    }
}

/// The two consents for sensitive genome findings: show them in the app, share them with agents.
async fn put_sensitive(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Path(profile): Path<String>,
    Body(body): Body<SensitiveIn>,
) -> R {
    let mut tx = rt.pool.begin().await?;
    let perm = manage(&mut tx, &p, &profile).await?;
    let turning_on = (body.ui == Some(true) && !perm.profile.genome_sensitive_ui)
        || (body.agents == Some(true) && !perm.profile.genome_sensitive_agents);
    if turning_on {
        if perm.profile.is_minor(local_today(), rt.settings.age_of_majority) {
            return Err(detail(
                StatusCode::CONFLICT,
                "minor",
                "Sensitive findings stay withheld until the age of majority.",
            ));
        }
        step_up(&mut tx, &p).await?;
    }
    set_genome_switches(&mut tx, perm.profile.id, body.ui, body.agents).await?;
    audit(
        &mut tx,
        &p,
        Audit {
            args_summary: Some(format!(
                "ui={};agents={}",
                py_opt_bool(body.ui),
                py_opt_bool(body.agents)
            )),
            ..Audit::new("consent_change", "PUT /api/profiles/sensitive", Some(perm.profile.id))
        },
    )
    .await?;
    let after = effective_permission(&mut tx, &p, perm.profile.id).await?.profile;
    tx.commit().await?;
    ok(json!({"ok": true, "ui": after.genome_sensitive_ui, "agents": after.genome_sensitive_agents}))
}

// ------------------------------------------------------------------- deletion ----

/// Own transaction, so the blob files go only after the rows are gone for good.
async fn delete_profile_now(rt: &Runtime, p: &Principal, profile_id: Uuid, slug: &str) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let result: Deleted = household::delete_profile(&mut tx, profile_id).await.map_err(fail)?;
    audit(
        &mut tx,
        p,
        Audit {
            resource_type: Some("profile"),
            resource_ids: Some(vec![profile_id.to_string()]),
            rows_out: Some(i32::try_from(result.total_rows()).unwrap_or(i32::MAX)),
            args_summary: Some(format!("blobs={}", result.blobs.len())),
            ..Audit::new("purge", "DELETE /api/profiles", Some(profile_id))
        },
    )
    .await?;
    tx.commit().await?;
    for sha in &result.blobs {
        if let Err(e) = rt.blobs.delete(sha) {
            tracing::warn!(error = %e, "blob delete failed");
        }
    }
    Ok(json!({"deleted": slug, "rows": result.rows_json(), "blobs": result.blobs.len()}))
}

/// Hard delete of the profile's rows and files. `confirm` must repeat the slug. Older backups still
/// hold the data until they expire.
async fn delete_profile(
    State(rt): State<Runtime>,
    SessionCaller(p): SessionCaller,
    Path(profile): Path<String>,
    Query(q): Params,
) -> R {
    let mut c = Check::query();
    let confirm = required(&mut c, "confirm", q.get("confirm"));
    c.done()?;
    let confirm = confirm.unwrap_or_default();
    let perm = {
        let mut tx = rt.pool.begin().await?;
        step_up(&mut tx, &p).await?;
        let perm = manage(&mut tx, &p, &profile).await?;
        if confirm != perm.profile.slug {
            return Err(detail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "confirm_mismatch",
                "Type the profile's short name.",
            ));
        }
        tx.commit().await?;
        perm
    };
    ok(delete_profile_now(&rt, &p, perm.profile.id, &perm.profile.slug).await?)
}

/// A query bool as pydantic reads it.
fn bool_param(c: &mut Check, field: &str, value: Option<&String>) -> bool {
    let Some(v) = value else { return false };
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "t" | "y" => true,
        "false" | "0" | "no" | "off" | "f" | "n" => false,
        _ => {
            c.push(
                "bool_parsing",
                field,
                "Input should be a valid boolean, unable to interpret input",
                json!(v),
            );
            false
        }
    }
}

/// Close my login; with `delete_profile=true` also hard-delete my own profile. `confirm` repeats my
/// name.
async fn delete_my_account(State(rt): State<Runtime>, SessionCaller(p): SessionCaller, Query(q): Params) -> R {
    let mut c = Check::query();
    let confirm = required(&mut c, "confirm", q.get("confirm"));
    let with_profile = bool_param(&mut c, "delete_profile", q.get("delete_profile"));
    c.done()?;
    let confirm = confirm.unwrap_or_default();
    let uid = p.user_id.ok_or_else(ApiError::unauthenticated)?;
    let own = {
        let mut tx = rt.pool.begin().await?;
        step_up(&mut tx, &p).await?;
        let name: Option<String> = sqlx::query_scalar("select display_name from app_user where id = $1")
            .bind(uid)
            .fetch_optional(&mut *tx)
            .await?;
        if Some(confirm.trim()) != name.as_deref() {
            return Err(detail(
                StatusCode::UNPROCESSABLE_ENTITY,
                "confirm_mismatch",
                "Type your name as shown.",
            ));
        }
        let own: Option<(Uuid, String)> = sqlx::query_as("select id, slug from profile where user_id = $1")
            .bind(uid)
            .fetch_optional(&mut *tx)
            .await?;
        if with_profile && let Some((_, slug)) = &own {
            manage(&mut tx, &p, slug).await?;
        }
        let except = own.as_ref().filter(|_| with_profile).map(|(id, _)| *id);
        let stranded = household::orphaned_profiles(&mut tx, uid, except).await?;
        if !stranded.is_empty() {
            return Err(fail(household::last_manager(stranded)));
        }
        tx.commit().await?;
        own
    };
    let mut deleted = Value::Null;
    if with_profile && let Some((id, slug)) = &own {
        deleted = delete_profile_now(&rt, &p, *id, slug).await?;
    }
    let mut tx = rt.pool.begin().await?;
    household::delete_account(&mut tx, uid).await.map_err(fail)?;
    audit(
        &mut tx,
        &p,
        Audit {
            resource_type: Some("user"),
            resource_ids: Some(vec![uid.to_string()]),
            ..Audit::new("account_delete", "DELETE /api/account", None)
        },
    )
    .await?;
    tx.commit().await?;
    Ok(answer(
        StatusCode::OK,
        json!({"ok": true, "profile_deleted": deleted}),
        &[delete_cookie(sessions::COOKIE)],
    ))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/household", get(get_household))
        .route("/household/profiles", post(add_relative))
        .route("/household/invites", post(invite))
        .route("/grants", put(put_grant).delete(delete_grant))
        .route("/profiles/{p}/locks", put(put_locks))
        .route("/profiles/{p}/sensitive", put(put_sensitive))
        .route("/profiles/{p}", delete(delete_profile))
        .route("/account", delete(delete_my_account))
}
