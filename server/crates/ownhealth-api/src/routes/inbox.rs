//! Lane A (v0.2): email routing over REST (`ownhealth_inbox::email`): the allowlist, PDF password
//! rules and the quarantine. A person who manages a profile (owner, or guardian of an unclaimed
//! profile) of the household. Releasing a message and adding a sender widen what flows in, so both
//! need step-up and the right to manage the target profile; deleting does not.
//!
//! | Route | What |
//! |---|---|
//! | `GET /inboxes/quarantine?status=held` | `[{id, source_id, sender, subject: untrusted, attachments, reason, status, received_at}]`; `status=all` lists every status |
//! | `POST /inboxes/quarantine/{id}/release` | `{profile_id, allow_sender: bool}` -> `{document_ids, problems: [{code, message}]}`; a `too_large` message is refused |
//! | `DELETE /inboxes/quarantine/{id}` | delete a held message |
//! | `GET /inboxes/{id}/senders`, `POST /inboxes/{id}/senders` | sender rules of a mailbox (each with its password labels); `{pattern, profile_id, doc_type_hint?, trust_unauthenticated?}` |
//! | `DELETE /senders/{id}` | one rule, and its passwords |
//! | `POST /senders/{id}/pdf-passwords` | `{password, label?}` (sealed; never returned) |
//! | `DELETE /pdf-passwords/{id}` | one password rule |

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use ownhealth_auth::household::{can_manage, require_manage};
use ownhealth_auth::sessions;
use ownhealth_core::access::{Principal, accessible_profiles};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::quarantine::{self, QuarantineItem};
use ownhealth_core::secrets::{SecretBox, SecretString};
use ownhealth_core::sources::{Source, get_source};
use ownhealth_core::{Error, Runtime};
use ownhealth_inbox::email::{self, SenderRuleIn};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::auth::{Body, R, answer, ok};
use crate::Caller;

fn parse_id(id: &str) -> R<Uuid> {
    Ok(Uuid::parse_str(id).map_err(|_| Error::NotFound("Unknown id.".into()))?)
}

fn not_found(what: &str) -> Error {
    Error::NotFound(format!("Unknown {what}."))
}

/// The caller manages at least one profile of the household (inbox settings are household-wide).
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

/// The caller may route mail into this profile.
async fn manages_profile(conn: &mut PgConnection, p: &Principal, profile_id: Uuid) -> R<()> {
    let perm = Caller(p.clone())
        .require(conn, &profile_id.to_string(), None, None)
        .await?;
    require_manage(perm)?;
    Ok(())
}

async fn mailbox(conn: &mut PgConnection, household: Uuid, id: Uuid) -> R<Source> {
    match get_source(conn, id).await? {
        Some(s) if s.household_id == household && s.kind == "email" => Ok(s),
        _ => Err(not_found("mailbox").into()),
    }
}

async fn held_item(conn: &mut PgConnection, household: Uuid, id: Uuid) -> R<QuarantineItem> {
    match quarantine::get_item(conn, id).await? {
        Some(q) if q.household_id == household => Ok(q),
        _ => Err(not_found("quarantine item").into()),
    }
}

/// The household of a sender rule, and its profile.
async fn rule_owner(conn: &mut PgConnection, rule_id: Uuid) -> R<(Uuid, Uuid)> {
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "select s.household_id, r.profile_id from sender_rule r join source s on s.id = r.source_id where r.id = $1",
    )
    .bind(rule_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.ok_or_else(|| not_found("sender rule"))?)
}

async fn audit(conn: &mut PgConnection, p: &Principal, action: &str, profile: Option<Uuid>, ids: Vec<String>) -> R<()> {
    write_audit(
        conn,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some("inbox".into()),
            profile_id: profile,
            resource_type: Some("quarantine_item".into()),
            resource_ids: Some(ids),
            ..AuditEvent::new("user", "ui", action)
        },
    )
    .await?;
    Ok(())
}

fn untrusted(text: Option<&str>, origin: &str) -> Value {
    match text {
        Some(t) => json!({"trust": "untrusted_text", "origin": origin, "text": t}),
        None => Value::Null,
    }
}

fn item_out(q: &QuarantineItem) -> Value {
    let attachments: Vec<Value> = q
        .attachments
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| {
                    json!({"filename": untrusted(f.get("filename").and_then(Value::as_str), "filename"),
                           "mime": f.get("mime"), "size": f.get("size")})
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": q.id, "source_id": q.source_id, "message_id": q.message_id, "sender": q.sender,
        "subject": untrusted(q.subject.as_deref(), "email_subject"),
        "attachments": attachments, "reason": q.reason, "status": q.status, "received_at": q.received_at,
        "decided_at": q.decided_at, "released_document_ids": q.released_document_ids,
    })
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    limit: Option<i64>,
}

async fn list_quarantine(State(rt): State<Runtime>, Caller(p): Caller, Query(q): Query<ListQuery>) -> R {
    let mut conn = rt.pool.acquire().await?;
    let household = manager(&mut conn, &p).await?;
    let status = match q.status.as_deref().unwrap_or("held") {
        "all" => None,
        s @ ("held" | "released" | "deleted") => Some(s),
        _ => return Err(Error::Invalid("status is held, released, deleted or all".into()).into()),
    };
    let items = quarantine::list_items(&mut conn, household, status, q.limit.unwrap_or(100)).await?;
    ok(Value::Array(items.iter().map(item_out).collect()))
}

#[derive(Deserialize)]
struct ReleaseIn {
    profile_id: Uuid,
    #[serde(default)]
    allow_sender: bool,
}

async fn release(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>, Body(b): Body<ReleaseIn>) -> R {
    let id = parse_id(&id)?;
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    held_item(&mut tx, household, id).await?;
    manages_profile(&mut tx, &p, b.profile_id).await?;
    sessions::require_step_up(&mut tx, &p, sessions::STEP_UP_SECONDS).await?;
    let user = p
        .user_id
        .ok_or_else(|| Error::access_denied("forbidden", "A person releases mail."))?;
    let sb = SecretBox::from_settings(&rt.settings).ok();
    let out = email::release(&mut tx, &rt.blobs, sb.as_ref(), id, user, b.profile_id, b.allow_sender).await?;
    let action = if b.allow_sender {
        "quarantine.release_and_allow"
    } else {
        "quarantine.release"
    };
    audit(&mut tx, &p, action, Some(b.profile_id), vec![id.to_string()]).await?;
    tx.commit().await?;
    let problems = email::problems(&out);
    ok(json!({"document_ids": out.document_ids, "problems": problems}))
}

async fn delete_held(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let id = parse_id(&id)?;
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    held_item(&mut tx, household, id).await?;
    let user = p
        .user_id
        .ok_or_else(|| Error::access_denied("forbidden", "A person deletes mail."))?;
    quarantine::mark_deleted(&mut tx, id, user).await?;
    audit(&mut tx, &p, "quarantine.delete", None, vec![id.to_string()]).await?;
    tx.commit().await?;
    ok(json!({"deleted": true}))
}

async fn list_senders(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let id = parse_id(&id)?;
    let mut conn = rt.pool.acquire().await?;
    let household = manager(&mut conn, &p).await?;
    let source = mailbox(&mut conn, household, id).await?;
    let mut out = Vec::new();
    for r in email::list_rules(&mut conn, source.id).await? {
        let passwords = email::list_pdf_passwords(&mut conn, r.id).await?;
        let mut v = serde_json::to_value(&r).map_err(Error::from)?;
        v["pdf_passwords"] = serde_json::to_value(&passwords).map_err(Error::from)?;
        out.push(v);
    }
    ok(Value::Array(out))
}

async fn add_sender(
    State(rt): State<Runtime>,
    Caller(p): Caller,
    Path(id): Path<String>,
    Body(b): Body<SenderRuleIn>,
) -> R {
    let id = parse_id(&id)?;
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    let source = mailbox(&mut tx, household, id).await?;
    manages_profile(&mut tx, &p, b.profile_id).await?;
    sessions::require_step_up(&mut tx, &p, sessions::STEP_UP_SECONDS).await?;
    let user = p
        .user_id
        .ok_or_else(|| Error::access_denied("forbidden", "A person adds senders."))?;
    let rule = email::add_rule(&mut tx, source.id, &b, user).await?;
    write_audit(
        &mut tx,
        &AuditEvent {
            actor_id: p.user_id,
            tool: Some("inbox".into()),
            profile_id: Some(b.profile_id),
            resource_type: Some("sender_rule".into()),
            resource_ids: Some(vec![rule.to_string()]),
            ..AuditEvent::new("user", "ui", "sender_rule.add")
        },
    )
    .await?;
    tx.commit().await?;
    Ok(answer(StatusCode::CREATED, json!({"id": rule}), &[]))
}

async fn delete_sender(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let id = parse_id(&id)?;
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    if rule_owner(&mut tx, id).await?.0 != household {
        return Err(not_found("sender rule").into());
    }
    email::delete_rule(&mut tx, id).await?;
    tx.commit().await?;
    ok(json!({"deleted": true}))
}

#[derive(Deserialize)]
struct PasswordIn {
    password: String,
    #[serde(default)]
    label: Option<String>,
}

async fn add_password(
    State(rt): State<Runtime>,
    Caller(p): Caller,
    Path(id): Path<String>,
    Body(b): Body<PasswordIn>,
) -> R {
    let id = parse_id(&id)?;
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    let (owner, profile) = rule_owner(&mut tx, id).await?;
    if owner != household {
        return Err(not_found("sender rule").into());
    }
    manages_profile(&mut tx, &p, profile).await?;
    let sb = SecretBox::from_settings(&rt.settings)?;
    let password = SecretString::new(b.password);
    let rule = email::add_pdf_password(&mut tx, &sb, id, &password, b.label.as_deref()).await?;
    tx.commit().await?;
    Ok(answer(StatusCode::CREATED, json!({"id": rule}), &[]))
}

async fn delete_password(State(rt): State<Runtime>, Caller(p): Caller, Path(id): Path<String>) -> R {
    let id = parse_id(&id)?;
    let mut tx = rt.pool.begin().await?;
    let household = manager(&mut tx, &p).await?;
    let owner: Option<Uuid> = sqlx::query_scalar(
        "select s.household_id from pdf_password_rule pr join sender_rule r on r.id = pr.sender_rule_id \
         join source s on s.id = r.source_id where pr.id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if owner != Some(household) {
        return Err(not_found("password rule").into());
    }
    email::delete_pdf_password(&mut tx, id).await?;
    tx.commit().await?;
    ok(json!({"deleted": true}))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/inboxes/quarantine", get(list_quarantine))
        .route("/inboxes/quarantine/{id}", delete(delete_held))
        .route("/inboxes/quarantine/{id}/release", post(release))
        .route("/inboxes/{id}/senders", get(list_senders).post(add_sender))
        .route("/senders/{id}", delete(delete_sender))
        .route("/senders/{id}/pdf-passwords", post(add_password))
        .route("/pdf-passwords/{id}", delete(delete_password))
}
