//! The caller of a REST request (`api/_deps.get_principal`, `auth/deps.py`): session cookie or
//! dev auth, with the CSRF check. A PAT is refused here; it works only on /mcp.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;
use ownhealth_auth::sessions::COOKIE;
use ownhealth_auth::{csrf, principal};
use ownhealth_core::access::{Permission, Principal, PrincipalKind, ProfileRef, require};
use ownhealth_core::{Error, Runtime};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiError;

/// The resolved caller of a REST route.
#[derive(Debug, Clone)]
pub struct Caller(pub Principal);

impl Caller {
    /// `core.access.require` for `{p}` (slug, `me` or uuid).
    pub async fn require(
        &self,
        conn: &mut PgConnection,
        p: &str,
        scope: Option<&str>,
        action: Option<&str>,
    ) -> Result<Permission, ApiError> {
        let r = match Uuid::parse_str(p) {
            Ok(id) => ProfileRef::Id(id),
            Err(_) => ProfileRef::Name(p),
        };
        Ok(require(conn, &self.0, r, scope, action).await?)
    }
}

impl FromRequestParts<Runtime> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, rt: &Runtime) -> Result<Self, Self::Rejection> {
        let authorization = parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let jar = CookieJar::from_headers(&parts.headers);
        let secret = jar.get(COOKIE).map(|c| c.value().to_string());
        let mut conn = rt.pool.acquire().await?;
        let p = principal::resolve(&mut conn, &rt.settings, authorization.as_deref(), secret.as_deref())
            .await?
            .ok_or_else(ApiError::unauthenticated)?;
        if p.kind == PrincipalKind::Token {
            // A PAT works only on /mcp, where the review queue, limits and identity mode apply.
            return Err(Error::access_denied("forbidden", "Access tokens work only on /mcp. Use the web app.").into());
        }
        csrf::check(&parts.method, &parts.headers, &p, &rt.settings.base_url)?;
        Ok(Caller(p))
    }
}

/// `auth.deps.SessionOnly`: a person in the browser (sessions and dev auth).
#[derive(Debug, Clone)]
pub struct SessionCaller(pub Principal);

impl FromRequestParts<Runtime> for SessionCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, rt: &Runtime) -> Result<Self, Self::Rejection> {
        let Caller(p) = Caller::from_request_parts(parts, rt).await?;
        Ok(SessionCaller(p))
    }
}
