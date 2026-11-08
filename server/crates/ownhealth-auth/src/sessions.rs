//! Browser sessions and step-up (`auth/sessions.py`, security-privacy.md 5).
//!
//! The cookie `oh_session` carries a random secret; `user_session.secret_sha256` stores its hash.
//! Idle timeout 30 minutes, absolute 12 hours, HttpOnly + SameSite=Lax, Secure when the base URL is
//! https. Step-up: a fresh passkey assertion (or OIDC re-login, or the password where passkeys cannot
//! run) within the last 5 minutes, recorded in `user_session.step_up_at`.

use chrono::{Duration, Utc};
use ownhealth_core::access::{Principal, PrincipalKind};
use ownhealth_core::{Error, Result};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{sha256, token_urlsafe, truncate_chars};

pub const COOKIE: &str = "oh_session";
pub const IDLE_MINUTES: i64 = 30;
pub const ABSOLUTE_HOURS: i64 = 12;
pub const STEP_UP_SECONDS: i64 = 300;
pub const METHODS: [&str; 4] = ["passkey", "recovery", "oidc", "password"];

/// The Principal (kind session, every scope, `session_id` set) for a live session secret, else None.
///
/// Rejects expired, idle and revoked sessions and disabled users; refreshes last_seen_at.
pub async fn resolve_session(conn: &mut PgConnection, secret: &str) -> Result<Option<Principal>> {
    if secret.is_empty() {
        return Ok(None);
    }
    let row: Option<(Uuid, Uuid, Uuid, String)> = sqlx::query_as(
        "update user_session s set last_seen_at = now() from app_user u \
         where s.secret_sha256 = $1 and u.id = s.user_id and s.revoked_at is null \
         and s.expires_at > now() and s.last_seen_at > now() - make_interval(mins => $2) \
         and u.disabled_at is null returning s.id, s.user_id, u.household_id, u.locale",
    )
    .bind(sha256(secret))
    .bind(IDLE_MINUTES as i32)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(sid, user, household, locale)| Principal {
        lang: locale,
        session_id: Some(sid),
        ..Principal::session(user, household)
    }))
}

/// Start a session after a login. Returns the new session secret (the cookie value).
///
/// A passkey, OIDC or password login is itself a fresh authentication, so it counts as step-up;
/// a recovery-code login does not (the person should enrol a new passkey first).
pub async fn create_session(
    conn: &mut PgConnection,
    user_id: Uuid,
    ip: Option<&str>,
    user_agent: Option<&str>,
    method: &str,
) -> Result<String> {
    create_session_with(conn, user_id, ip, user_agent, method, None).await
}

/// `create_session` with the step-up decided by the caller (tests, and Python's `step_up=` argument).
pub async fn create_session_with(
    conn: &mut PgConnection,
    user_id: Uuid,
    ip: Option<&str>,
    user_agent: Option<&str>,
    method: &str,
    step_up: Option<bool>,
) -> Result<String> {
    if !METHODS.contains(&method) {
        return Err(Error::Invalid(format!("unknown login method {method}")));
    }
    let step_up = step_up.unwrap_or(method != "recovery");
    let secret = token_urlsafe(32);
    let now = Utc::now();
    let ua = truncate_chars(user_agent.unwrap_or_default(), 300);
    sqlx::query(
        "insert into user_session (user_id, secret_sha256, expires_at, step_up_at, ip, user_agent, method) \
         values ($1, $2, $3, $4, cast($5 as inet), $6, $7)",
    )
    .bind(user_id)
    .bind(sha256(&secret))
    .bind(now + Duration::hours(ABSOLUTE_HOURS))
    .bind(step_up.then_some(now))
    .bind(ip)
    .bind((!ua.is_empty()).then_some(ua))
    .bind(method)
    .execute(conn)
    .await?;
    Ok(secret)
}

pub async fn revoke_session(conn: &mut PgConnection, secret: &str) -> Result<bool> {
    let n = sqlx::query("update user_session set revoked_at = now() where secret_sha256 = $1 and revoked_at is null")
        .bind(sha256(secret))
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n > 0)
}

pub async fn revoke_user_sessions(conn: &mut PgConnection, user_id: Uuid) -> Result<u64> {
    Ok(
        sqlx::query("update user_session set revoked_at = now() where user_id = $1 and revoked_at is null")
            .bind(user_id)
            .execute(conn)
            .await?
            .rows_affected(),
    )
}

pub async fn mark_step_up(conn: &mut PgConnection, session_id: Uuid) -> Result<()> {
    sqlx::query("update user_session set step_up_at = now() where id = $1")
        .bind(session_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// The session row behind a browser principal; None for tokens and dev auth.
pub fn session_id(principal: &Principal) -> Option<Uuid> {
    if principal.kind == PrincipalKind::Session {
        principal.session_id
    } else {
        None
    }
}

/// How the principal's session started (`passkey`, `recovery`, `oidc`, `password`); None without one.
pub async fn session_method(conn: &mut PgConnection, principal: &Principal) -> Result<Option<String>> {
    let Some(sid) = session_id(principal) else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar("select method from user_session where id = $1")
        .bind(sid)
        .fetch_optional(conn)
        .await?)
}

/// 403 `step_up_required` (an `AccessDenied`) unless the session logged in within `max_age_seconds`.
///
/// Tokens never pass. The dev principal (OWNHEALTH_DEV_AUTH=1, demo and tests only) always passes.
pub async fn require_step_up(conn: &mut PgConnection, principal: &Principal, max_age_seconds: i64) -> Result<()> {
    if principal.kind == PrincipalKind::Dev {
        return Ok(());
    }
    let Some(sid) = session_id(principal) else {
        return Err(Error::access_denied(
            "step_up_required",
            "This needs a browser session with a fresh login.",
        ));
    };
    let fresh: Option<i32> = sqlx::query_scalar(
        "select 1 from user_session where id = $1 and revoked_at is null \
         and step_up_at > now() - make_interval(secs => $2)",
    )
    .bind(sid)
    .bind(max_age_seconds as f64)
    .fetch_optional(conn)
    .await?;
    if fresh.is_none() {
        return Err(Error::access_denied(
            "step_up_required",
            "Confirm it is you: use your passkey again.",
        ));
    }
    Ok(())
}
