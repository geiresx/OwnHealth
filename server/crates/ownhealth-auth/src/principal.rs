//! Who is calling (`auth/principal.py`): a PAT (Bearer), a browser session (cookie), or the dev
//! user (OWNHEALTH_DEV_AUTH=1).

use ownhealth_core::Result;
use ownhealth_core::access::{Principal, PrincipalKind};
use ownhealth_core::config::Settings;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{sessions, tokens};

/// The first user holding an owner grant. Only for OWNHEALTH_DEV_AUTH=1 (demo, tests, dev).
pub async fn dev_principal(conn: &mut PgConnection) -> Result<Option<Principal>> {
    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "select u.id, u.household_id, u.locale from app_user u join profile_grant g on g.user_id = u.id \
         where g.role = 'owner' and u.disabled_at is null order by u.created_at, u.id limit 1",
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(user, household, locale)| Principal {
        kind: PrincipalKind::Dev,
        lang: locale,
        ..Principal::session(user, household)
    }))
}

/// `authorization`: the Authorization header; `session_secret`: the `oh_session` cookie.
pub async fn resolve(
    conn: &mut PgConnection,
    settings: &Settings,
    authorization: Option<&str>,
    session_secret: Option<&str>,
) -> Result<Option<Principal>> {
    if let Some(auth) = authorization.filter(|a| !a.is_empty()) {
        let (scheme, value) = auth.split_once(' ').unwrap_or((auth, ""));
        if scheme.eq_ignore_ascii_case("bearer") && !value.is_empty() {
            return tokens::verify_token(conn, value.trim()).await;
        }
        return Ok(None);
    }
    if let Some(secret) = session_secret.filter(|s| !s.is_empty())
        && let Some(p) = sessions::resolve_session(&mut *conn, secret).await?
    {
        return Ok(Some(p));
    }
    if settings.dev_auth {
        return dev_principal(conn).await;
    }
    Ok(None)
}
