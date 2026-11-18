//! A calendar feed of one profile's due items (migration 1131): a secret URL that a calendar app
//! polls with no cookie, so due dates show next to the person's appointments with no notification
//! service to run.
//!
//! ```text
//! GET    /api/ics/{token}.ics                          no cookie: the token is the credential
//! GET    /api/profiles/{p}/calendar-feeds              read:records   the caller's own live feeds
//! POST   /api/profiles/{p}/calendar-feeds              read:records + step-up   {detail?} -> {feed, path}
//! PATCH  /api/profiles/{p}/calendar-feeds/{id}         read:records (+ step-up for named)   {detail}
//! DELETE /api/profiles/{p}/calendar-feeds/{id}         read:records   revoke
//! ```
//!
//! The token is `oh_ics_<id8>_<secret43>` and lives only in the POST answer: the row keeps the prefix
//! and the sha256 of the secret, as a push token does. A fetch looks the prefix up and compares the
//! hash in constant time; an unknown, wrong or revoked token, a disabled creator and a creator without
//! read:records on the profile all get the same 404.
//!
//! A fetch reads the ownhealth.prevention/v1 document as its creator, in the creator's language: one
//! all-day VEVENT per open item (overdue, due, upcoming) on its due date, or today when that date has
//! passed or there is none, and one per snoozed item on the day it is shown again, or on its due date
//! when that comes later. A dismissed item and one up to date are left out. An item marked done is
//! left out while the mark hides it, and comes back when its next round is open, as on the Due screen.
//! UID = sha256(profile id, item key), so an event keeps its UID when its day moves. `generic` titles
//! every event "OwnHealth: 1 item due"; `named` uses the item's title and puts its reason and
//! guideline link in the description. Each fetch writes one audit row (via export, actor share,
//! action calendar_feed.fetch, with the peer address and user agent: behind a reverse proxy the
//! address is the proxy's, as on every audit row), and a feed fetched more than RATE_PER_HOUR times
//! in the last hour waits. A HEAD gets 405: it would deliver no calendar.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, ProfileRef, require, today_utc};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_rules::prevention;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::auth::{Body, Check, Meta, R, answer, ok};
use crate::error::ApiError;
use crate::extract::Caller;

pub const PREFIX: &str = "oh_ics_";
/// Fetches of one feed per rolling hour; calendar apps poll every few minutes at most.
pub const RATE_PER_HOUR: i64 = 60;
const DETAILS: [&str; 2] = ["generic", "named"];
/// The statuses an event is made for.
const OPEN: [&str; 3] = ["overdue", "due", "upcoming"];

/// The title of every event of a generic feed, per language.
fn generic_title(lang: &str) -> &'static str {
    if lang == "es" {
        "OwnHealth: 1 elemento pendiente"
    } else {
        "OwnHealth: 1 item due"
    }
}

/// Every sentence the feed writes itself, for the wording gate (the named titles and reasons are the
/// Due screen's, linted there).
pub fn phrases() -> Value {
    json!({"en": {"generic": generic_title("en")}, "es": {"generic": generic_title("es")}})
}

// ------------------------------------------------------------------ the feed ----

/// The one answer for every token that does not open a feed.
fn gone() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown", "No such calendar feed.")
}

struct Feed {
    id: Uuid,
    profile_id: Uuid,
    created_by: Uuid,
    household_id: Uuid,
    locale: String,
    detail: String,
}

/// The live feed of a token, or None. The same work for every token: one lookup by prefix and one
/// constant-time hash comparison, against a dummy hash when the prefix is unknown. Only a matching
/// secret then locks the feed row, to the end of the transaction, so two fetches at once count the
/// hour's fetches in turn and a wrong secret never waits on a fetch in progress.
async fn lookup(conn: &mut PgConnection, token: &str) -> Result<Option<Feed>, ApiError> {
    let (prefix, secret) = token
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once('_'))
        .unwrap_or(("", ""));
    type Row = (Uuid, Uuid, Uuid, Uuid, String, Vec<u8>, bool);
    let row: Option<Row> = sqlx::query_as(
        "select f.id, f.profile_id, f.created_by, u.household_id, u.locale, f.secret_sha256, \
         f.revoked_at is null and u.disabled_at is null \
         from calendar_feed f join app_user u on u.id = f.created_by where f.prefix = $1",
    )
    .bind(format!("{PREFIX}{prefix}"))
    .fetch_optional(&mut *conn)
    .await?;
    let stored = row.as_ref().map_or_else(|| vec![0u8; 32], |r| r.5.clone());
    let same: bool = ownhealth_auth::sha256(secret).ct_eq(&stored).into();
    let Some(r) = row.filter(|r| same && r.6 && !secret.is_empty()) else {
        return Ok(None);
    };
    // the feed as it is once its row is ours: a revoke, a disabled login or a change of detail in
    // between counts
    let locked: Option<(bool, String)> = sqlx::query_as(
        "select f.revoked_at is null and u.disabled_at is null, f.detail \
         from calendar_feed f join app_user u on u.id = f.created_by where f.id = $1 for update of f",
    )
    .bind(r.0)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(locked.filter(|l| l.0).map(|(_, detail)| Feed {
        id: r.0,
        profile_id: r.1,
        created_by: r.2,
        household_id: r.3,
        locale: r.4,
        detail,
    }))
}

/// One event: (UID, day, summary, description).
type Event = (String, NaiveDate, String, Option<String>);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The UID of one item of one profile: the same on every fetch, whatever day the item moves to.
pub fn uid(profile_id: Uuid, item_key: &str) -> String {
    format!(
        "{}@ownhealth",
        hex(&ownhealth_auth::sha256(&format!("{profile_id}:{item_key}")))
    )
}

fn day(v: &Value) -> Option<NaiveDate> {
    v.as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
}

/// The events of a rendered prevention document.
fn events(doc: &Value, profile_id: Uuid, today: NaiveDate, named: bool, lang: &str) -> Vec<Event> {
    let mut out = Vec::new();
    for list in ["items", "hidden"] {
        for it in doc[list].as_array().into_iter().flatten() {
            if !OPEN.contains(&it["status"].as_str().unwrap_or_default()) {
                continue;
            }
            // the first day the item can show: today for a listed one (a day already past is out of a
            // calendar's sight), and the day after `until` for a snoozed one
            let first = if list == "items" {
                today
            } else if it["decision"]["kind"] == "snooze" {
                match day(&it["decision"]["until"]).and_then(|u| u.succ_opt()) {
                    Some(d) => d.max(today),
                    None => continue,
                }
            } else {
                continue;
            };
            // on its due date, or that first day when the date is earlier (it has passed, or is where
            // the age window opened for something never recorded) or there is none: a snooze never
            // brings an item forward
            let start = day(&it["due"]).map_or(first, |d| d.max(first));
            let key = it["key"].as_str().unwrap_or_default();
            let (summary, description) = if named {
                let title = it["title"].as_str().unwrap_or(key);
                // the guideline the item comes from: its name when it has one, and its link
                let cite = match (it["source"].as_str(), it["cite"].as_str()) {
                    (Some(source), Some(cite)) => Some(format!("{source}: {cite}")),
                    (_, cite) => cite.map(str::to_string),
                };
                let lines: Vec<String> = [&it["why"], &it["reason"]]
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .chain(cite)
                    .collect();
                (format!("OwnHealth: {title}"), Some(lines.join("\n")))
            } else {
                (generic_title(lang).to_string(), None)
            };
            out.push((uid(profile_id, key), start, summary, description));
        }
    }
    out
}

/// RFC 5545 3.3.11 TEXT.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// One content line, folded at 75 octets (RFC 5545 3.1) without splitting a character.
fn line(out: &mut String, text: &str) {
    let mut width = 0;
    for c in text.chars() {
        if width + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += c.len_utf8();
    }
    out.push_str("\r\n");
}

fn ics_date(d: NaiveDate) -> String {
    d.format("%Y%m%d").to_string()
}

/// The calendar text.
fn calendar(name: &str, events: &[Event], now: DateTime<Utc>) -> String {
    let mut out = String::new();
    for l in [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//OwnHealth//Due items//EN",
        "CALSCALE:GREGORIAN",
        "METHOD:PUBLISH",
        // RFC 7986: how often to poll; due dates move at most once a day
        "REFRESH-INTERVAL;VALUE=DURATION:PT6H",
        "X-PUBLISHED-TTL:PT6H",
    ] {
        line(&mut out, l);
    }
    line(&mut out, &format!("NAME:{}", escape(name)));
    line(&mut out, &format!("X-WR-CALNAME:{}", escape(name)));
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    for (uid, start, summary, description) in events {
        line(&mut out, "BEGIN:VEVENT");
        line(&mut out, &format!("UID:{uid}"));
        line(&mut out, &format!("DTSTAMP:{stamp}"));
        line(&mut out, &format!("DTSTART;VALUE=DATE:{}", ics_date(*start)));
        if let Some(end) = start.succ_opt() {
            line(&mut out, &format!("DTEND;VALUE=DATE:{}", ics_date(end)));
        }
        line(&mut out, &format!("SUMMARY:{}", escape(summary)));
        if let Some(d) = description.as_deref().filter(|d| !d.is_empty()) {
            line(&mut out, &format!("DESCRIPTION:{}", escape(d)));
        }
        line(&mut out, "TRANSP:TRANSPARENT");
        line(&mut out, "END:VEVENT");
    }
    line(&mut out, "END:VCALENDAR");
    out
}

async fn feed(State(rt): State<Runtime>, meta: Meta, Path(file): Path<String>) -> Response {
    match serve(&rt, &file, meta).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn serve(rt: &Runtime, file: &str, meta: Meta) -> Result<Response, ApiError> {
    let token = file.strip_suffix(".ics").unwrap_or_default();
    let mut tx = rt.pool.begin().await?;
    let f = lookup(&mut tx, token).await?.ok_or_else(gone)?;
    // the feed reads as its creator, so it stops when the creator loses access
    let creator = Principal {
        lang: f.locale.clone(),
        ..Principal::session(f.created_by, f.household_id)
    };
    let perm = require(
        &mut tx,
        &creator,
        ProfileRef::Id(f.profile_id),
        Some("read:records"),
        None,
    )
    .await
    .map_err(|_| gone())?;
    let recent: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where profile_id = $1 and tool = 'ics' and via = 'export' \
         and resource_ids = array[$2::text] and ts > now() - interval '1 hour'",
    )
    .bind(f.profile_id)
    .bind(f.id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if recent >= RATE_PER_HOUR {
        let mut res = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("This feed answers {RATE_PER_HOUR} times an hour."),
        )
        .into_response();
        res.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("600"));
        return Ok(res);
    }
    let today = today_utc();
    let doc = prevention::build_document(&mut tx, f.profile_id, today).await?;
    let pack = ownhealth_rules::load_pack().map_err(ownhealth_core::Error::from)?;
    let lang = if f.locale == "es" { "es" } else { "en" };
    let rendered = prevention::render(&doc, &pack, lang, &perm.scopes)?;
    let named = f.detail == "named";
    let list = events(&rendered, f.profile_id, today, named, lang);
    let name = if named {
        format!("OwnHealth · {}", perm.profile.display_name)
    } else {
        "OwnHealth".to_string()
    };
    let text = calendar(&name, &list, Utc::now());
    // a calendar app's fetch, with the peer address and user agent it came from, so the owner of an
    // address that leaked can tell who polls it
    let mut e = AuditEvent::new("share", "export", "calendar_feed.fetch");
    e.actor_id = Some(f.created_by);
    e.ip = meta.ip;
    e.user_agent = meta.user_agent;
    e.tool = Some("ics".into());
    e.profile_id = Some(f.profile_id);
    e.resource_type = Some("calendar_feed".into());
    e.resource_ids = Some(vec![f.id.to_string()]);
    e.args_summary = Some(format!("detail={}", f.detail));
    e.rows_out = Some(i32::try_from(list.len()).unwrap_or(i32::MAX));
    e.bytes_out = Some(i32::try_from(text.len()).unwrap_or(i32::MAX));
    write_audit(&mut tx, &e).await?;
    sqlx::query("update calendar_feed set last_used_at = now() where id = $1")
        .bind(f.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(([(header::CONTENT_TYPE, "text/calendar; charset=utf-8")], text).into_response())
}

// ---------------------------------------------------------------- managing ----

#[derive(Deserialize)]
struct DetailIn {
    #[serde(default = "generic")]
    detail: String,
}

fn generic() -> String {
    "generic".into()
}

fn check_detail(d: &str) -> R<()> {
    Check::body().literal("detail", Some(d), &DETAILS).done()?;
    Ok(())
}

type Listed = (Uuid, String, String, DateTime<Utc>, Option<DateTime<Utc>>);

fn feed_out(r: &Listed) -> Value {
    json!({"id": r.0, "prefix": r.1, "detail": r.2, "created_at": r.3, "last_used_at": r.4})
}

const COLS: &str = "id, prefix, detail, created_at, last_used_at";

async fn audit(
    conn: &mut PgConnection,
    p: &Principal,
    perm: &Permission,
    action: &str,
    id: Uuid,
    args: String,
) -> R<()> {
    let mut e = AuditEvent::new("user", "ui", action);
    e.actor_id = p.user_id;
    e.tool = Some("calendar_feed".into());
    e.profile_id = Some(perm.profile.id);
    e.resource_type = Some("calendar_feed".into());
    e.resource_ids = Some(vec![id.to_string()]);
    e.args_summary = Some(args);
    write_audit(conn, &e).await?;
    Ok(())
}

async fn list(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>) -> R {
    let mut conn = rt.pool.acquire().await?;
    let perm = caller.require(&mut conn, &p, Some("read:records"), None).await?;
    let rows: Vec<Listed> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {COLS} from calendar_feed where profile_id = $1 and created_by = $2 and revoked_at is null order by created_at, id"
    )))
    .bind(perm.profile.id)
    .bind(caller.0.user_id)
    .fetch_all(&mut *conn)
    .await?;
    ok(json!({"items": rows.iter().map(feed_out).collect::<Vec<_>>()}))
}

/// The plaintext is in this answer and nowhere else, ever.
async fn create(State(rt): State<Runtime>, caller: Caller, Path(p): Path<String>, Body(b): Body<DetailIn>) -> R {
    check_detail(&b.detail)?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    sessions::require_step_up(&mut tx, &caller.0, sessions::STEP_UP_SECONDS).await?;
    let user = caller.0.user_id.ok_or_else(ApiError::unauthenticated)?;
    let prefix = format!("{PREFIX}{}", ownhealth_core::ids::short(8));
    let secret = ownhealth_auth::token_urlsafe(32);
    let row: Listed = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "insert into calendar_feed (profile_id, created_by, prefix, secret_sha256, detail) \
         values ($1, $2, $3, $4, $5) returning {COLS}"
    )))
    .bind(perm.profile.id)
    .bind(user)
    .bind(&prefix)
    .bind(ownhealth_auth::sha256(&secret))
    .bind(&b.detail)
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        &caller.0,
        &perm,
        "calendar_feed.create",
        row.0,
        format!("detail={}", b.detail),
    )
    .await?;
    tx.commit().await?;
    Ok(answer(
        StatusCode::CREATED,
        json!({"feed": feed_out(&row), "path": format!("/api/ics/{prefix}_{secret}.ics")}),
        &[],
    ))
}

/// One of the caller's own live feeds of the profile, locked.
async fn own(conn: &mut PgConnection, perm: &Permission, user: Option<Uuid>, id: &str) -> R<Uuid> {
    let id = Uuid::parse_str(id).map_err(|_| ApiError::not_found())?;
    let found: Option<Uuid> = sqlx::query_scalar(
        "select id from calendar_feed where id = $1 and profile_id = $2 and created_by = $3 \
         and revoked_at is null for update",
    )
    .bind(id)
    .bind(perm.profile.id)
    .bind(user)
    .fetch_optional(conn)
    .await?;
    found.ok_or_else(|| gone().into())
}

/// Naming the items widens what the calendar shows, so it needs step-up; going back does not.
async fn set_detail(
    State(rt): State<Runtime>,
    caller: Caller,
    Path((p, id)): Path<(String, String)>,
    Body(b): Body<DetailIn>,
) -> R {
    check_detail(&b.detail)?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    let id = own(&mut tx, &perm, caller.0.user_id, &id).await?;
    if b.detail == "named" {
        sessions::require_step_up(&mut tx, &caller.0, sessions::STEP_UP_SECONDS).await?;
    }
    let row: Listed = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "update calendar_feed set detail = $2 where id = $1 returning {COLS}"
    )))
    .bind(id)
    .bind(&b.detail)
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        &caller.0,
        &perm,
        "calendar_feed.detail",
        id,
        format!("detail={}", b.detail),
    )
    .await?;
    tx.commit().await?;
    ok(feed_out(&row))
}

/// Revoking only takes access away, so it needs no step-up.
async fn revoke(State(rt): State<Runtime>, caller: Caller, Path((p, id)): Path<(String, String)>) -> R {
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:records"), None).await?;
    let id = own(&mut tx, &perm, caller.0.user_id, &id).await?;
    sqlx::query("update calendar_feed set revoked_at = now() where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &caller.0, &perm, "calendar_feed.revoke", id, String::new()).await?;
    tx.commit().await?;
    ok(json!({"id": id, "revoked": true}))
}

/// A HEAD would run the whole fetch, take one of the hour's fetches and write an audit row, and
/// deliver no calendar: it is refused before any lookup, the same for every token.
async fn no_head() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "GET")]).into_response()
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/ics/{file}", get(feed).head(no_head))
        .route("/profiles/{p}/calendar-feeds", get(list).post(create))
        .route(
            "/profiles/{p}/calendar-feeds/{id}",
            axum::routing::patch(set_detail).delete(revoke),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_line_folds_at_75_octets_without_splitting_a_character() {
        let mut out = String::new();
        let text = format!("SUMMARY:{}", "vacunación ".repeat(20));
        line(&mut out, &text);
        assert!(out.split("\r\n").all(|l| l.len() <= 75), "{out}");
        assert_eq!(out.replace("\r\n ", ""), format!("{text}\r\n"));
    }

    /// A snooze hides an item until a day; it never brings an item forward of its due date.
    #[test]
    fn a_snoozed_item_comes_back_after_its_snooze_and_never_before_its_due_date() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default();
        let snooze = json!({"kind": "snooze", "until": "2026-12-01"});
        let doc = json!({"items": [], "hidden": [
            {"key": "later", "status": "upcoming", "due": "2027-03-01", "decision": snooze},
            {"key": "now", "status": "due", "due": "2026-11-17", "decision": snooze},
            {"key": "undated", "status": "due", "decision": snooze},
            {"key": "dismissed", "status": "due", "decision": {"kind": "dismiss"}},
        ]});
        let p = Uuid::nil();
        let got: Vec<(String, NaiveDate)> = events(&doc, p, d("2026-11-17"), false, "en")
            .into_iter()
            .map(|e| (e.0, e.1))
            .collect();
        assert_eq!(
            got,
            vec![
                (uid(p, "later"), d("2027-03-01")),
                (uid(p, "now"), d("2026-12-02")),
                (uid(p, "undated"), d("2026-12-02")),
            ]
        );
    }

    #[test]
    fn text_values_escape_the_characters_rfc_5545_reserves() {
        assert_eq!(escape("a;b,c\\d\ne"), "a\\;b\\,c\\\\d\\ne");
    }
}
