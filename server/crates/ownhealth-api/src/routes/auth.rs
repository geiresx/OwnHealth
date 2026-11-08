//! Lane G: `api/auth.py`. Login, first-run setup, invites, step-up, passkeys, recovery codes, OIDC
//! (security-privacy.md 5), under `/auth/*`.
//!
//! The browser runs the WebAuthn ceremonies: `.../begin` returns the options JSON for
//! `navigator.credentials.create()` / `.get()`, `.../finish` takes the credential JSON back
//! (`PublicKeyCredential.toJSON()`). A successful login sets the `oh_session` cookie.
//!
//! The helpers at the top (`Fail`, `Body`, `Meta`, `StepUpCaller`, the 422 shapes) are shared by the
//! other lane G routers: tokens, family, audit, export.

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use axum_extra::extract::CookieJar;
use chrono::{NaiveDate, Utc};
use ownhealth_auth::oidc::{self, OidcConfig};
use ownhealth_auth::passkeys::{self, Rp};
use ownhealth_auth::sessions::{self, COOKIE, STEP_UP_SECONDS};
use ownhealth_auth::{AuthError, csrf, household, password, principal, recovery, token_urlsafe, truncate_chars};
use ownhealth_core::access::{Principal, PrincipalKind};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::config::Settings;
use ownhealth_core::{Error, Runtime};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::Caller;

// ------------------------------------------------------------ shared helpers ----

/// An error answer of a lane G route: FastAPI's `{"detail": {...}}`, or its 422 validation list
/// `{"detail": [{"type", "loc", "msg", "input", ...}]}`.
#[derive(Debug)]
pub enum Fail {
    Api(ApiError),
    Invalid(Vec<Value>),
}

pub type R<T = Response> = Result<T, Fail>;

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        match self {
            Fail::Api(e) => e.into_response(),
            Fail::Invalid(errors) => {
                (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": errors}))).into_response()
            }
        }
    }
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Fail {
        Fail::Api(e)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Api(e.into())
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Fail {
        Fail::Api(e.into())
    }
}

impl From<csrf::CsrfFailed> for Fail {
    fn from(e: csrf::CsrfFailed) -> Fail {
        Fail::Api(e.into())
    }
}

/// Python's `_fail(exc)`: 400 with the error's code, 409 for the conflicts.
impl From<AuthError> for Fail {
    fn from(e: AuthError) -> Fail {
        fail(e, StatusCode::BAD_REQUEST)
    }
}

/// `_fail(exc, status)`: the refusal's code and message (and extra keys); 409 for `already_set_up`,
/// `already_claimed`, `passkey_exists` and `last_manager` whatever the status asked.
pub fn fail(e: AuthError, status: StatusCode) -> Fail {
    match e {
        AuthError::Refused { code, message, extra } => {
            let status = if matches!(
                code.as_str(),
                "already_set_up" | "already_claimed" | "passkey_exists" | "last_manager"
            ) {
                StatusCode::CONFLICT
            } else {
                status
            };
            let mut err = ApiError::new(status, &code, message);
            err.extra = Box::new(extra);
            Fail::Api(err)
        }
        AuthError::Core(e) => e.into(),
    }
}

/// `HTTPException(status, detail={"code": code, "message": message})`.
pub fn detail(status: StatusCode, code: &str, message: &str) -> Fail {
    Fail::Api(ApiError::new(status, code, message))
}

/// One pydantic error entry.
pub fn error_entry(kind: &str, loc: Vec<Value>, msg: &str, input: Value, ctx: Option<Value>) -> Value {
    let mut e = Map::new();
    e.insert("type".into(), json!(kind));
    e.insert("loc".into(), Value::Array(loc));
    e.insert("msg".into(), json!(msg));
    e.insert("input".into(), input);
    if let Some(ctx) = ctx {
        e.insert("ctx".into(), ctx);
    }
    Value::Object(e)
}

/// pydantic's wording of a Literal's choices: `'a', 'b' or 'c'`.
pub fn literal_expected(allowed: &[&str]) -> String {
    let quoted: Vec<String> = allowed.iter().map(|a| format!("'{a}'")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => quoted.join(""),
    }
}

/// Field checks with pydantic's error shapes; `done()` answers 422 when any failed.
pub struct Check {
    place: &'static str,
    errors: Vec<Value>,
}

impl Check {
    pub fn body() -> Check {
        Check {
            place: "body",
            errors: Vec::new(),
        }
    }

    pub fn query() -> Check {
        Check {
            place: "query",
            errors: Vec::new(),
        }
    }

    pub fn path() -> Check {
        Check {
            place: "path",
            errors: Vec::new(),
        }
    }

    fn loc(&self, field: &str) -> Vec<Value> {
        vec![json!(self.place), json!(field)]
    }

    pub fn len(&mut self, field: &str, value: Option<&str>, min: usize, max: usize) -> &mut Check {
        if let Some(v) = value {
            let n = v.chars().count();
            if n < min {
                let s = if min == 1 { "" } else { "s" };
                self.errors.push(error_entry(
                    "string_too_short",
                    self.loc(field),
                    &format!("String should have at least {min} character{s}"),
                    json!(v),
                    Some(json!({"min_length": min})),
                ));
            } else if n > max {
                self.errors.push(error_entry(
                    "string_too_long",
                    self.loc(field),
                    &format!("String should have at most {max} characters"),
                    json!(v),
                    Some(json!({"max_length": max})),
                ));
            }
        }
        self
    }

    pub fn literal(&mut self, field: &str, value: Option<&str>, allowed: &[&str]) -> &mut Check {
        if let Some(v) = value
            && !allowed.contains(&v)
        {
            let expected = literal_expected(allowed);
            self.errors.push(error_entry(
                "literal_error",
                self.loc(field),
                &format!("Input should be {expected}"),
                json!(v),
                Some(json!({"expected": expected})),
            ));
        }
        self
    }

    /// `input` is the value as sent (pydantic reports `"0"` for a string, `0` for a number).
    pub fn range(&mut self, field: &str, value: i64, input: Value, ge: i64, le: i64) -> &mut Check {
        if value < ge {
            self.errors.push(error_entry(
                "greater_than_equal",
                self.loc(field),
                &format!("Input should be greater than or equal to {ge}"),
                input,
                Some(json!({"ge": ge})),
            ));
        } else if value > le {
            self.errors.push(error_entry(
                "less_than_equal",
                self.loc(field),
                &format!("Input should be less than or equal to {le}"),
                input,
                Some(json!({"le": le})),
            ));
        }
        self
    }

    pub fn items(&mut self, field: &str, n: usize, min: usize, max: usize) -> &mut Check {
        let s = |k: usize| if k == 1 { "item" } else { "items" };
        if n < min {
            self.errors.push(error_entry(
                "too_short",
                self.loc(field),
                &format!("List should have at least {min} {} after validation, not {n}", s(min)),
                Value::Null,
                Some(json!({"field_type": "List", "min_length": min, "actual_length": n})),
            ));
        } else if n > max {
            self.errors.push(error_entry(
                "too_long",
                self.loc(field),
                &format!("List should have at most {max} {} after validation, not {n}", s(max)),
                Value::Null,
                Some(json!({"field_type": "List", "max_length": max, "actual_length": n})),
            ));
        }
        self
    }

    pub fn push(&mut self, kind: &str, field: &str, msg: &str, input: Value) -> &mut Check {
        self.errors.push(error_entry(kind, self.loc(field), msg, input, None));
        self
    }

    pub fn done(&mut self) -> R<()> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(Fail::Invalid(std::mem::take(&mut self.errors)))
        }
    }
}

/// A JSON request body. Unparseable JSON, a missing body or a field of the wrong type answers 422
/// with pydantic's error shapes, as FastAPI did.
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = Fail;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state).await.map_err(|_| {
            Fail::Invalid(vec![error_entry(
                "missing",
                vec![json!("body")],
                "Field required",
                Value::Null,
                None,
            )])
        })?;
        if bytes.is_empty() {
            return Err(Fail::Invalid(vec![error_entry(
                "missing",
                vec![json!("body")],
                "Field required",
                Value::Null,
                None,
            )]));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            Fail::Invalid(vec![error_entry(
                "json_invalid",
                vec![json!("body"), json!(e.column())],
                "JSON decode error",
                json!({}),
                Some(json!({"error": e.to_string()})),
            )])
        })?;
        if !value.is_object() {
            return Err(Fail::Invalid(vec![error_entry(
                "model_attributes_type",
                vec![json!("body")],
                "Input should be a valid dictionary or object to extract fields from",
                value,
                None,
            )]));
        }
        serde_json::from_value(value.clone())
            .map(Body)
            .map_err(|e| Fail::Invalid(vec![serde_entry(&e, value)]))
    }
}

/// A serde error as the nearest pydantic entry: `missing` for a missing field, else a type error.
fn serde_entry(e: &serde_json::Error, input: Value) -> Value {
    let text = e.to_string();
    if let Some(rest) = text.strip_prefix("missing field `")
        && let Some(field) = rest.split('`').next()
    {
        return error_entry(
            "missing",
            vec![json!("body"), json!(field)],
            "Field required",
            input,
            None,
        );
    }
    error_entry("value_error", vec![json!("body")], &text, input, None)
}

/// The request facts a login route needs: method, headers, cookies, the peer IP and user agent.
pub struct Meta {
    pub method: Method,
    pub headers: HeaderMap,
    pub cookies: CookieJar,
    /// The peer address when it is an IP (Python's `client_ip`).
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl<S: Send + Sync> FromRequestParts<S> for Meta {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(a)| a.ip().to_string());
        let user_agent = parts
            .headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        Ok(Meta {
            method: parts.method.clone(),
            headers: parts.headers.clone(),
            cookies: CookieJar::from_headers(&parts.headers),
            ip,
            user_agent,
        })
    }
}

impl Meta {
    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies.get(name).map(|c| c.value().to_string())
    }
}

/// `dependencies=[Public]`: the same-origin check of the public login routes (no principal yet).
pub struct Public;

impl FromRequestParts<Runtime> for Public {
    type Rejection = Fail;

    async fn from_request_parts(parts: &mut Parts, rt: &Runtime) -> Result<Self, Self::Rejection> {
        csrf::check_origin(&parts.method, &parts.headers, &rt.settings.base_url)?;
        Ok(Public)
    }
}

/// `auth.deps.StepUp`: a person in the browser who logged in within the last 5 minutes.
pub struct StepUpCaller(pub Principal);

impl FromRequestParts<Runtime> for StepUpCaller {
    type Rejection = Fail;

    async fn from_request_parts(parts: &mut Parts, rt: &Runtime) -> Result<Self, Self::Rejection> {
        let Caller(p) = Caller::from_request_parts(parts, rt).await?;
        let mut conn = rt.pool.acquire().await?;
        sessions::require_step_up(&mut conn, &p, STEP_UP_SECONDS).await?;
        Ok(StepUpCaller(p))
    }
}

/// The JSON answer with optional `Set-Cookie` headers.
pub fn answer(status: StatusCode, body: Value, cookies: &[String]) -> Response {
    let mut res = (status, Json(body)).into_response();
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    res
}

pub fn ok(body: Value) -> R {
    Ok(answer(StatusCode::OK, body, &[]))
}

/// Starlette's `set_cookie(..., httponly=True, samesite="lax")`.
pub fn set_cookie(name: &str, value: &str, max_age: i64, path: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{name}={value}; HttpOnly; Max-Age={max_age}; Path={path}; SameSite=lax{secure}")
}

/// Starlette's `delete_cookie(name, path="/")`.
pub fn delete_cookie(name: &str) -> String {
    let now = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");
    format!("{name}=\"\"; expires={now}; Max-Age=0; Path=/; SameSite=lax")
}

fn session_cookie(secret: &str, settings: &Settings) -> String {
    set_cookie(
        COOKIE,
        secret,
        sessions::ABSOLUTE_HOURS * 3600,
        "/",
        settings.base_url.starts_with("https://"),
    )
}

/// A query string as a map (repeated keys: the last wins, as FastAPI reads a scalar).
pub type Params = Query<HashMap<String, String>>;

/// A required uuid parameter, or the 422 entry FastAPI gives.
pub fn uuid_param(check: &mut Check, field: &str, value: Option<&String>) -> Option<Uuid> {
    match value {
        None => {
            check.push("missing", field, "Field required", Value::Null);
            None
        }
        Some(v) => match Uuid::parse_str(v) {
            Ok(u) => Some(u),
            Err(e) => {
                check.push(
                    "uuid_parsing",
                    field,
                    &format!("Input should be a valid UUID, {e}"),
                    json!(v),
                );
                None
            }
        },
    }
}

/// A pydantic `date` field: `YYYY-MM-DD`.
pub fn date_field(check: &mut Check, field: &str, value: Option<&str>) -> Option<NaiveDate> {
    let v = value?;
    match NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        Ok(d) => Some(d),
        Err(_) => {
            check.push(
                "date_from_datetime_parsing",
                field,
                "Input should be a valid date or datetime, invalid character in year",
                json!(v),
            );
            None
        }
    }
}

// ------------------------------------------------------------------ auth bits ----

fn rp(settings: &Settings) -> Rp {
    Rp::from_base_url(&settings.base_url)
}

fn require_passkeys(settings: &Settings) -> R<()> {
    if !password::passkeys_possible(&settings.base_url) {
        return Err(detail(
            StatusCode::BAD_REQUEST,
            "passkeys_unavailable",
            "Passkeys need https (or localhost). Use the HTTPS address, or the password fallback.",
        ));
    }
    Ok(())
}

fn require_no_passkeys(settings: &Settings) -> R<()> {
    if password::passkeys_possible(&settings.base_url) {
        return Err(detail(
            StatusCode::FORBIDDEN,
            "passkeys_available",
            "This address supports passkeys: use one.",
        ));
    }
    Ok(())
}

/// Tests inject a fake provider with this request extension; otherwise the environment decides.
#[derive(Debug, Clone)]
pub struct OidcOverride(pub Option<OidcConfig>);

fn oidc_config(o: &Option<Extension<OidcOverride>>) -> Option<OidcConfig> {
    match o {
        Some(Extension(OidcOverride(cfg))) => cfg.clone(),
        None => OidcConfig::from_env(),
    }
}

/// Start a session, audit the login and return the cookie to set.
async fn login(conn: &mut PgConnection, meta: &Meta, settings: &Settings, user_id: Uuid, method: &str) -> R<String> {
    let secret = sessions::create_session(
        &mut *conn,
        user_id,
        meta.ip.as_deref(),
        meta.user_agent.as_deref(),
        method,
    )
    .await?;
    let ua = truncate_chars(meta.user_agent.as_deref().unwrap_or_default(), 300);
    write_audit(
        conn,
        &AuditEvent {
            actor_id: Some(user_id),
            tool: Some(format!("auth:{method}")),
            ip: meta.ip.clone(),
            user_agent: (!ua.is_empty()).then(|| ua.to_string()),
            ..AuditEvent::new("user", "ui", "login")
        },
    )
    .await?;
    Ok(session_cookie(&secret, settings))
}

/// A new way in (a passkey, an OIDC link) needs a fresh login: a session someone took over must not
/// plant one. A recovery-code login counts only right after it logged in, so the person can replace a
/// lost passkey; it never counts as step-up for anything else.
pub async fn require_fresh_to_add_login(conn: &mut PgConnection, p: &Principal) -> R<()> {
    if let Some(sid) = sessions::session_id(p)
        && sessions::session_method(&mut *conn, p).await?.as_deref() == Some("recovery")
    {
        let just_in: Option<i32> = sqlx::query_scalar(
            "select 1 from user_session where id = $1 and revoked_at is null \
             and created_at > now() - make_interval(secs => $2)",
        )
        .bind(sid)
        .bind(STEP_UP_SECONDS as f64)
        .fetch_optional(&mut *conn)
        .await?;
        if just_in.is_some() {
            return Ok(());
        }
    }
    sessions::require_step_up(conn, p, STEP_UP_SECONDS).await?;
    Ok(())
}

fn user_of(p: &Principal) -> R<Uuid> {
    p.user_id.ok_or_else(|| Fail::Api(ApiError::unauthenticated()))
}

async fn display_name(conn: &mut PgConnection, user_id: Uuid) -> R<String> {
    Ok(sqlx::query_scalar("select display_name from app_user where id = $1")
        .bind(user_id)
        .fetch_one(conn)
        .await?)
}

async fn flag(conn: &mut PgConnection, sql: &str, user_id: Uuid) -> R<bool> {
    let v: Option<Option<bool>> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(user_id)
        .fetch_optional(conn)
        .await?;
    Ok(v.flatten().unwrap_or(false))
}

// ------------------------------------------------------------------- state ----

/// What the login page needs: first run or not, which methods work here, who is logged in.
async fn state(State(rt): State<Runtime>, meta: Meta, o: Option<Extension<OidcOverride>>) -> R {
    let mut tx = rt.pool.begin().await?;
    let p = principal::resolve(&mut tx, &rt.settings, None, meta.cookie(COOKIE).as_deref()).await?;
    let cfg = oidc_config(&o);
    let (mut user, mut fresh, mut method) = (Value::Null, false, Value::Null);
    if let Some(p) = &p {
        method = json!(sessions::session_method(&mut tx, p).await?);
        if let Some(uid) = p.user_id {
            let (id, name, locale): (Uuid, String, String) =
                sqlx::query_as("select id, display_name, locale from app_user where id = $1")
                    .bind(uid)
                    .fetch_one(&mut *tx)
                    .await?;
            let passkeys: i64 = sqlx::query_scalar("select count(*) from passkey where user_id = $1")
                .bind(uid)
                .fetch_one(&mut *tx)
                .await?;
            let left = recovery::remaining(&mut tx, uid).await?;
            let oidc_linked = flag(
                &mut tx,
                "select oidc_subject is not null from app_user where id = $1",
                uid,
            )
            .await?;
            user = json!({
                "id": id,
                "display_name": name,
                "locale": locale,
                "passkeys": passkeys,
                "recovery_codes_left": left,
                "oidc_linked": oidc_linked,
            });
            fresh = sessions::require_step_up(&mut tx, p, STEP_UP_SECONDS).await.is_ok();
        }
    }
    let possible = password::passkeys_possible(&rt.settings.base_url);
    let setup_needed = !household::users_exist(&mut tx).await?;
    tx.commit().await?;
    ok(json!({
        "setup_needed": setup_needed,
        "passkeys_possible": possible,
        "password_allowed": !possible,
        "oidc": {"enabled": cfg.is_some(), "label": cfg.as_ref().map(|c| c.label.clone())},
        "user": user,
        "method": method,
        "auth_kind": p.as_ref().map(|p| p.kind),
        "step_up_fresh": fresh,
        "step_up_seconds": STEP_UP_SECONDS,
    }))
}

// ---------------------------------------------------------------- first run ----

#[derive(Debug, Clone, Deserialize)]
pub struct SetupIn {
    #[serde(default)]
    household_name: String,
    display_name: String,
    #[serde(default = "es")]
    locale: String,
    #[serde(default)]
    birth_date: Option<String>,
    #[serde(default)]
    sex_at_birth: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

fn es() -> String {
    "es".into()
}

impl SetupIn {
    fn check(&self, c: &mut Check) -> Option<NaiveDate> {
        c.len("household_name", Some(&self.household_name), 0, 80)
            .len("display_name", Some(&self.display_name), 1, 80);
        date_field(c, "birth_date", self.birth_date.as_deref())
    }

    /// `body.model_dump(mode="json")`, kept in the setup challenge.
    fn dump(&self) -> Value {
        json!({
            "household_name": self.household_name,
            "display_name": self.display_name,
            "locale": self.locale,
            "birth_date": self.birth_date,
            "sex_at_birth": self.sex_at_birth,
            "country": self.country,
        })
    }

    fn owner<'a>(&'a self, birth: Option<NaiveDate>) -> household::OwnerIn<'a> {
        household::OwnerIn {
            household_name: &self.household_name,
            display_name: &self.display_name,
            locale: &self.locale,
            birth_date: birth,
            sex_at_birth: self.sex_at_birth.as_deref(),
            country: self.country.as_deref(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CredentialIn {
    credential: Map<String, Value>,
    #[serde(default)]
    label: Option<String>,
}

impl CredentialIn {
    fn check(&self, c: &mut Check) {
        c.len("label", self.label.as_deref(), 0, 80);
    }

    fn credential(&self) -> Value {
        Value::Object(self.credential.clone())
    }
}

async fn setup_begin(State(rt): State<Runtime>, _: Public, Body(body): Body<SetupIn>) -> R {
    let mut c = Check::body();
    body.check(&mut c);
    c.done()?;
    require_passkeys(&rt.settings)?;
    let mut tx = rt.pool.begin().await?;
    if household::users_exist(&mut tx).await? {
        return Err(detail(
            StatusCode::CONFLICT,
            "already_set_up",
            "This instance already has an owner.",
        ));
    }
    let mut data = Map::new();
    data.insert("setup".into(), body.dump());
    let options = passkeys::begin_registration(
        &mut tx,
        Uuid::new_v4(),
        &rp(&rt.settings),
        &body.display_name,
        "setup",
        data,
    )
    .await?;
    tx.commit().await?;
    ok(options)
}

async fn setup_finish(State(rt): State<Runtime>, _: Public, meta: Meta, Body(body): Body<CredentialIn>) -> R {
    let mut c = Check::body();
    body.check(&mut c);
    c.done()?;
    let mut tx = rt.pool.begin().await?;
    let reg = passkeys::verify_registration(&mut tx, &body.credential(), &rp(&rt.settings), &["setup"]).await?;
    let setup: SetupIn = reg
        .challenge
        .data
        .get("setup")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| {
            fail(
                AuthError::refused(
                    "challenge_invalid",
                    "This request belongs to another step. Start again.",
                ),
                StatusCode::BAD_REQUEST,
            )
        })?;
    let birth = setup
        .birth_date
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let user_id = reg.challenge.text("user_id").and_then(|u| Uuid::parse_str(u).ok());
    let owner = household::setup_owner(
        &mut tx,
        &household::OwnerIn {
            user_id,
            ..setup.owner(birth)
        },
    )
    .await?;
    passkeys::store_passkey(&mut tx, owner.user_id, &reg, body.label.as_deref()).await?;
    let codes = recovery::new_codes(&mut tx, owner.user_id).await?;
    let cookie = login(&mut tx, &meta, &rt.settings, owner.user_id, "passkey").await?;
    tx.commit().await?;
    Ok(answer(
        StatusCode::OK,
        json!({"user_id": owner.user_id, "profile": owner.slug, "recovery_codes": codes}),
        &[cookie],
    ))
}

#[derive(Debug, Clone, Deserialize)]
pub struct PasswordSetupIn {
    #[serde(flatten)]
    setup: SetupIn,
    password: String,
}

/// First run on a plain-HTTP LAN address, where passkeys cannot run.
async fn setup_password(State(rt): State<Runtime>, _: Public, meta: Meta, Body(body): Body<PasswordSetupIn>) -> R {
    let mut c = Check::body();
    let birth = body.setup.check(&mut c);
    c.len("password", Some(&body.password), password::MIN_LENGTH, 256)
        .done()?;
    require_no_passkeys(&rt.settings)?;
    let mut tx = rt.pool.begin().await?;
    let owner = household::setup_owner(&mut tx, &body.setup.owner(birth)).await?;
    password::set_password(&mut tx, owner.user_id, &body.password).await?;
    let codes = recovery::new_codes(&mut tx, owner.user_id).await?;
    let cookie = login(&mut tx, &meta, &rt.settings, owner.user_id, "password").await?;
    tx.commit().await?;
    Ok(answer(
        StatusCode::OK,
        json!({"user_id": owner.user_id, "profile": owner.slug, "recovery_codes": codes}),
        &[cookie],
    ))
}

// -------------------------------------------------------------------- login ----

async fn login_begin(State(rt): State<Runtime>, _: Public) -> R {
    require_passkeys(&rt.settings)?;
    let mut tx = rt.pool.begin().await?;
    let options = passkeys::begin_login(&mut tx, &rp(&rt.settings), None, None).await?;
    tx.commit().await?;
    ok(options)
}

async fn login_finish(State(rt): State<Runtime>, _: Public, meta: Meta, Body(body): Body<CredentialIn>) -> R {
    let mut c = Check::body();
    body.check(&mut c);
    c.done()?;
    let mut tx = rt.pool.begin().await?;
    let user_id = passkeys::finish_login(&mut tx, &body.credential(), &rp(&rt.settings), "login")
        .await
        .map_err(|e| fail(e, StatusCode::UNAUTHORIZED))?;
    let cookie = login(&mut tx, &meta, &rt.settings, user_id, "passkey").await?;
    tx.commit().await?;
    Ok(answer(StatusCode::OK, json!({"user_id": user_id}), &[cookie]))
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecoveryIn {
    code: String,
}

/// Log in with one recovery code. It is used up; the session has no step-up until a new login.
async fn recovery_login(State(rt): State<Runtime>, _: Public, meta: Meta, Body(body): Body<RecoveryIn>) -> R {
    Check::body().len("code", Some(&body.code), 1, 40).done()?;
    let mut tx = rt.pool.begin().await?;
    let Some(user_id) = recovery::use_code(&mut tx, &body.code).await? else {
        return Err(detail(
            StatusCode::UNAUTHORIZED,
            "recovery_invalid",
            "Unknown or used recovery code.",
        ));
    };
    let cookie = login(&mut tx, &meta, &rt.settings, user_id, "recovery").await?;
    let left = recovery::remaining(&mut tx, user_id).await?;
    tx.commit().await?;
    Ok(answer(
        StatusCode::OK,
        json!({"user_id": user_id, "recovery_codes_left": left}),
        &[cookie],
    ))
}

#[derive(Debug, Clone, Deserialize)]
pub struct PasswordLoginIn {
    name: String,
    password: String,
}

async fn password_login(State(rt): State<Runtime>, _: Public, meta: Meta, Body(body): Body<PasswordLoginIn>) -> R {
    Check::body()
        .len("name", Some(&body.name), 1, 80)
        .len("password", Some(&body.password), 1, 256)
        .done()?;
    require_no_passkeys(&rt.settings)?;
    let mut tx = rt.pool.begin().await?;
    let rows: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "select id, password_hash from app_user where lower(display_name) = lower($1) \
         and password_hash is not null and disabled_at is null",
    )
    .bind(body.name.trim())
    .fetch_all(&mut *tx)
    .await?;
    let ok_ = match &rows[..] {
        [(_, hash)] => password::check_password_async(&body.password, hash.clone()).await?,
        _ => false,
    };
    if rows.is_empty() {
        // the same cost on a miss
        let h = password::hash_password_async("timing").await?;
        password::check_password_async(&body.password, Some(h)).await?;
    }
    if !ok_ {
        return Err(detail(
            StatusCode::UNAUTHORIZED,
            "login_failed",
            "Wrong name or password.",
        ));
    }
    let user_id = rows[0].0;
    let cookie = login(&mut tx, &meta, &rt.settings, user_id, "password").await?;
    tx.commit().await?;
    Ok(answer(StatusCode::OK, json!({"user_id": user_id}), &[cookie]))
}

async fn logout(State(rt): State<Runtime>, _: Public, meta: Meta) -> R {
    let mut tx = rt.pool.begin().await?;
    if let Some(secret) = meta.cookie(COOKIE).filter(|s| !s.is_empty()) {
        sessions::revoke_session(&mut tx, &secret).await?;
    }
    tx.commit().await?;
    Ok(answer(StatusCode::OK, json!({"ok": true}), &[delete_cookie(COOKIE)]))
}

// ------------------------------------------------------------------ step-up ----

/// Which ceremony confirms it is you: a passkey (options returned), the password, or OIDC.
async fn step_up_begin(State(rt): State<Runtime>, Caller(p): Caller) -> R {
    let uid = user_of(&p)?;
    let mut tx = rt.pool.begin().await?;
    let has_passkey = flag(&mut tx, "select exists (select 1 from passkey where user_id = $1)", uid).await?;
    let out = if has_passkey && password::passkeys_possible(&rt.settings.base_url) {
        let options = passkeys::begin_login(&mut tx, &rp(&rt.settings), Some(uid), sessions::session_id(&p)).await?;
        json!({"method": "passkey", "options": options})
    } else if flag(
        &mut tx,
        "select password_hash is not null from app_user where id = $1",
        uid,
    )
    .await?
    {
        json!({"method": "password"})
    } else if flag(
        &mut tx,
        "select oidc_subject is not null from app_user where id = $1",
        uid,
    )
    .await?
    {
        json!({"method": "oidc", "url": "/api/auth/oidc/login?mode=step_up"})
    } else {
        return Err(detail(StatusCode::CONFLICT, "no_method", "Add a passkey first."));
    };
    tx.commit().await?;
    ok(out)
}

#[derive(Debug, Clone, Deserialize)]
pub struct StepUpIn {
    #[serde(default)]
    credential: Option<Map<String, Value>>,
    #[serde(default)]
    password: Option<String>,
}

async fn step_up_finish(State(rt): State<Runtime>, Caller(p): Caller, Body(body): Body<StepUpIn>) -> R {
    Check::body().len("password", body.password.as_deref(), 0, 256).done()?;
    let uid = user_of(&p)?;
    let Some(sid) = sessions::session_id(&p) else {
        return Err(detail(
            StatusCode::FORBIDDEN,
            "step_up_required",
            "Log in in the browser first.",
        ));
    };
    let mut tx = rt.pool.begin().await?;
    if let Some(credential) = body.credential {
        let user_id = passkeys::finish_login(&mut tx, &Value::Object(credential), &rp(&rt.settings), "step_up")
            .await
            .map_err(|e| fail(e, StatusCode::UNAUTHORIZED))?;
        if user_id != uid {
            // a challenge another user started, answered with their passkey
            return Err(detail(
                StatusCode::UNAUTHORIZED,
                "passkey_invalid",
                "Another user's passkey.",
            ));
        }
    } else if let Some(pw) = body.password {
        if password::passkeys_possible(&rt.settings.base_url)
            && flag(&mut tx, "select exists (select 1 from passkey where user_id = $1)", uid).await?
        {
            return Err(detail(StatusCode::FORBIDDEN, "passkeys_available", "Use your passkey."));
        }
        if !password::verify_user_password(&mut tx, uid, &pw).await? {
            return Err(detail(StatusCode::UNAUTHORIZED, "login_failed", "Wrong password."));
        }
    } else {
        return Err(detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_input",
            "Send a passkey credential or the password.",
        ));
    }
    sessions::mark_step_up(&mut tx, sid).await?;
    tx.commit().await?;
    ok(json!({"ok": true, "valid_seconds": STEP_UP_SECONDS}))
}

// ---------------------------------------------------- passkeys, recovery codes ----

async fn list_my_passkeys(State(rt): State<Runtime>, Caller(p): Caller) -> R {
    let uid = user_of(&p)?;
    let mut conn = rt.pool.acquire().await?;
    let items = passkeys::list_passkeys(&mut conn, uid).await?;
    ok(json!({"items": items}))
}

/// Add a passkey (a second device, or a new one after a recovery-code login). Needs a fresh login.
async fn add_passkey_begin(State(rt): State<Runtime>, Caller(p): Caller) -> R {
    require_passkeys(&rt.settings)?;
    let uid = user_of(&p)?;
    let mut tx = rt.pool.begin().await?;
    require_fresh_to_add_login(&mut tx, &p).await?;
    let name = display_name(&mut tx, uid).await?;
    let options = passkeys::begin_registration(&mut tx, uid, &rp(&rt.settings), &name, "register", Map::new()).await?;
    tx.commit().await?;
    ok(options)
}

async fn add_passkey_finish(State(rt): State<Runtime>, Caller(p): Caller, Body(body): Body<CredentialIn>) -> R {
    let mut c = Check::body();
    body.check(&mut c);
    c.done()?;
    let uid = user_of(&p)?;
    let mut tx = rt.pool.begin().await?;
    passkeys::finish_registration(
        &mut tx,
        uid,
        &body.credential(),
        &rp(&rt.settings),
        body.label.as_deref(),
    )
    .await?;
    let items = passkeys::list_passkeys(&mut tx, uid).await?;
    tx.commit().await?;
    ok(json!({"items": items}))
}

async fn remove_passkey(
    State(rt): State<Runtime>,
    StepUpCaller(p): StepUpCaller,
    Path(credential_id): Path<String>,
) -> R {
    let uid = user_of(&p)?;
    let mut tx = rt.pool.begin().await?;
    let other_way_in = flag(
        &mut tx,
        "select (select count(*) from passkey where user_id = $1) > 1 or password_hash is not null \
         or oidc_subject is not null from app_user where id = $1",
        uid,
    )
    .await?;
    if !other_way_in {
        return Err(detail(
            StatusCode::CONFLICT,
            "last_passkey",
            "This is your only way in. Add another first.",
        ));
    }
    if !passkeys::delete_passkey(&mut tx, uid, &credential_id).await? {
        return Err(detail(StatusCode::NOT_FOUND, "unknown", "Unknown passkey."));
    }
    tx.commit().await?;
    ok(json!({"ok": true}))
}

/// Ten new codes; the old ones stop working. Shown once.
async fn regenerate_recovery_codes(State(rt): State<Runtime>, StepUpCaller(p): StepUpCaller) -> R {
    let uid = user_of(&p)?;
    let mut tx = rt.pool.begin().await?;
    let codes = recovery::new_codes(&mut tx, uid).await?;
    tx.commit().await?;
    ok(json!({"recovery_codes": codes}))
}

// ------------------------------------------------------------------- invites ----

async fn invite_info(State(rt): State<Runtime>, Path(code): Path<String>, o: Option<Extension<OidcOverride>>) -> R {
    let mut tx = rt.pool.begin().await?;
    let Some(ch) = household::peek_invite(&mut tx, &code).await? else {
        return Err(detail(
            StatusCode::NOT_FOUND,
            "invite_invalid",
            "This invite expired or was used.",
        ));
    };
    let (name, hh): (String, String) = sqlx::query_as(
        "select u.display_name, h.name from app_user u join household h on h.id = u.household_id where u.id = $1",
    )
    .bind(ch.user_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let possible = password::passkeys_possible(&rt.settings.base_url);
    ok(json!({
        "display_name": name,
        "household": hh,
        "claims_profile": ch.data.get("claim_profile_id").is_some_and(|v| !v.is_null()),
        "passkeys_possible": possible,
        "password_allowed": !possible,
        "oidc": oidc_config(&o).is_some(),
    }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollIn {
    code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollFinishIn {
    #[serde(flatten)]
    credential: CredentialIn,
    code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnrollPasswordIn {
    code: String,
    password: String,
}

fn invite_invalid() -> Fail {
    detail(
        StatusCode::BAD_REQUEST,
        "invite_invalid",
        "This invite expired or was used.",
    )
}

async fn enroll_begin(State(rt): State<Runtime>, _: Public, Body(body): Body<EnrollIn>) -> R {
    Check::body().len("code", Some(&body.code), 10, 80).done()?;
    require_passkeys(&rt.settings)?;
    let mut tx = rt.pool.begin().await?;
    let ch = household::peek_invite(&mut tx, &body.code)
        .await?
        .ok_or_else(invite_invalid)?;
    let uid = ch.user_id.ok_or_else(invite_invalid)?;
    let name = display_name(&mut tx, uid).await?;
    let options = passkeys::begin_registration(&mut tx, uid, &rp(&rt.settings), &name, "enroll", Map::new()).await?;
    tx.commit().await?;
    ok(options)
}

async fn enrolled(
    conn: &mut PgConnection,
    meta: &Meta,
    settings: &Settings,
    user_id: Uuid,
    method: &str,
) -> R<(Value, String)> {
    let codes = recovery::new_codes(&mut *conn, user_id).await?;
    let cookie = login(conn, meta, settings, user_id, method).await?;
    Ok((json!({"user_id": user_id, "recovery_codes": codes}), cookie))
}

async fn enroll_finish(State(rt): State<Runtime>, _: Public, meta: Meta, Body(body): Body<EnrollFinishIn>) -> R {
    let mut c = Check::body();
    body.credential.check(&mut c);
    c.len("code", Some(&body.code), 10, 80).done()?;
    let mut tx = rt.pool.begin().await?;
    let reg =
        passkeys::verify_registration(&mut tx, &body.credential.credential(), &rp(&rt.settings), &["enroll"]).await?;
    let user_id = household::redeem_invite(&mut tx, &body.code).await?;
    if reg.challenge.text("user_id") != Some(user_id.to_string().as_str()) {
        return Err(AuthError::refused("invite_invalid", "This passkey was made for another invite.").into());
    }
    passkeys::store_passkey(&mut tx, user_id, &reg, body.credential.label.as_deref()).await?;
    let (out, cookie) = enrolled(&mut tx, &meta, &rt.settings, user_id, "passkey").await?;
    tx.commit().await?;
    Ok(answer(StatusCode::OK, out, &[cookie]))
}

async fn enroll_password(State(rt): State<Runtime>, _: Public, meta: Meta, Body(body): Body<EnrollPasswordIn>) -> R {
    Check::body()
        .len("code", Some(&body.code), 10, 80)
        .len("password", Some(&body.password), password::MIN_LENGTH, 256)
        .done()?;
    require_no_passkeys(&rt.settings)?;
    let mut tx = rt.pool.begin().await?;
    let user_id = household::redeem_invite(&mut tx, &body.code).await?;
    let name = display_name(&mut tx, user_id).await?;
    let taken: bool = sqlx::query_scalar(
        "select exists (select 1 from app_user where lower(display_name) = lower($1) and id <> $2 \
         and password_hash is not null)",
    )
    .bind(&name)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(detail(
            StatusCode::CONFLICT,
            "name_taken",
            "Another login uses this name.",
        ));
    }
    password::set_password(&mut tx, user_id, &body.password).await?;
    let (out, cookie) = enrolled(&mut tx, &meta, &rt.settings, user_id, "password").await?;
    tx.commit().await?;
    Ok(answer(StatusCode::OK, out, &[cookie]))
}

// ---------------------------------------------------------------------- OIDC ----

const OIDC_COOKIE: &str = "oh_oidc";

fn redirect_uri(settings: &Settings) -> String {
    format!("{}/api/auth/oidc/callback", settings.base_url)
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut res = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        res.headers_mut().insert(header::LOCATION, v);
    }
    for c in cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    res
}

/// Python's `urllib.parse.quote` (safe `/`).
fn quote(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn back(path: &str, error: Option<&str>) -> Response {
    match error {
        Some(e) => redirect(&format!("{path}?auth_error={}", quote(e)), &[]),
        None => redirect(path, &[]),
    }
}

async fn session_principal(conn: &mut PgConnection, settings: &Settings, meta: &Meta) -> R<Option<Principal>> {
    Ok(principal::resolve(conn, settings, None, meta.cookie(COOKIE).as_deref()).await?)
}

/// Redirect to the provider. mode: login | link (logged in) | enroll (with ?invite=) | step_up.
async fn oidc_login(State(rt): State<Runtime>, meta: Meta, Query(q): Params, o: Option<Extension<OidcOverride>>) -> R {
    let Some(cfg) = oidc_config(&o) else {
        return Err(detail(
            StatusCode::NOT_FOUND,
            "oidc_disabled",
            "OIDC is not configured.",
        ));
    };
    let mode = q.get("mode").map_or("login", String::as_str);
    let mut tx = rt.pool.begin().await?;
    let (mut user_id, mut session_id, mut data) = (None, None, Map::new());
    if matches!(mode, "link" | "step_up") {
        let p = session_principal(&mut tx, &rt.settings, &meta)
            .await?
            .filter(|p| p.kind == PrincipalKind::Session)
            .ok_or_else(|| Fail::Api(ApiError::unauthenticated()))?;
        if mode == "link" {
            require_fresh_to_add_login(&mut tx, &p).await?;
        }
        (user_id, session_id) = (p.user_id, sessions::session_id(&p));
    } else if mode == "enroll" {
        let invite = q.get("invite").cloned().unwrap_or_default();
        if household::peek_invite(&mut tx, &invite).await?.is_none() {
            return Err(invite_invalid());
        }
        data.insert("invite".into(), json!(invite));
    }
    // the callback counts only in this browser: it must bring back the binder cookie set here
    let binder = token_urlsafe(32);
    data.insert("binder".into(), json!(oidc::binder_hash(&binder)));
    let uri = redirect_uri(&rt.settings);
    let url = oidc::begin(
        &mut tx,
        &cfg,
        oidc::Begin {
            mode,
            redirect_uri: &uri,
            user_id,
            session_id,
            data,
        },
    )
    .await?;
    tx.commit().await?;
    let cookie = set_cookie(
        OIDC_COOKIE,
        &binder,
        oidc::STATE_TTL_SECONDS,
        "/api/auth/oidc",
        rt.settings.base_url.starts_with("https://"),
    );
    Ok(redirect(&url, &[cookie]))
}

async fn oidc_callback(
    State(rt): State<Runtime>,
    meta: Meta,
    Query(q): Params,
    o: Option<Extension<OidcOverride>>,
) -> R {
    let Some(cfg) = oidc_config(&o) else {
        return Err(detail(
            StatusCode::NOT_FOUND,
            "oidc_disabled",
            "OIDC is not configured.",
        ));
    };
    let (state, code) = (
        q.get("state").cloned().unwrap_or_default(),
        q.get("code").cloned().unwrap_or_default(),
    );
    let error = q.get("error").filter(|e| !e.is_empty());
    if error.is_some() || state.is_empty() || code.is_empty() {
        let e = truncate_chars(error.map_or("failed", String::as_str), 40);
        return Ok(back("/login", Some(&format!("oidc_{e}"))));
    }
    // Every answer from here commits: a state is used up even when the sign-in fails.
    let mut tx = rt.pool.begin().await?;
    let res = match oidc::finish(
        &mut tx,
        &cfg,
        &state,
        &code,
        &redirect_uri(&rt.settings),
        &meta.cookie(OIDC_COOKIE).unwrap_or_default(),
    )
    .await
    {
        Ok(r) => r,
        Err(AuthError::Refused { code, .. }) => {
            tx.commit().await?;
            return Ok(back("/login", Some(&code)));
        }
        Err(AuthError::Core(e)) => return Err(e.into()),
    };
    let out = oidc_outcome(&mut tx, &rt.settings, &meta, &res).await?;
    tx.commit().await?;
    Ok(out)
}

async fn oidc_outcome(tx: &mut PgConnection, settings: &Settings, meta: &Meta, res: &oidc::OidcResult) -> R {
    let ch = &res.challenge;
    if matches!(res.mode.as_str(), "link" | "step_up") {
        // and in the session that started it
        let p = session_principal(&mut *tx, settings, meta).await?;
        if p.is_none() || ch.session_id.is_none() || p.as_ref().and_then(sessions::session_id) != ch.session_id {
            return Ok(back("/", Some("oidc_wrong_user")));
        }
    }
    let owner: Option<Uuid> =
        sqlx::query_scalar("select id from app_user where oidc_subject = $1 and disabled_at is null")
            .bind(&res.subject)
            .fetch_optional(&mut *tx)
            .await?;
    match res.mode.as_str() {
        "login" => {
            let mut owner = owner;
            if owner.is_none() && !household::users_exist(&mut *tx).await? {
                let o = household::setup_owner(
                    &mut *tx,
                    &household::OwnerIn {
                        household_name: "",
                        display_name: res.name.as_deref().unwrap_or("Owner"),
                        locale: "es",
                        oidc_subject: Some(&res.subject),
                        ..Default::default()
                    },
                )
                .await?;
                owner = Some(o.user_id);
            }
            let Some(owner) = owner else {
                return Ok(back("/login", Some("oidc_not_linked")));
            };
            let cookie = login(tx, meta, settings, owner, "oidc").await?;
            Ok(redirect("/", &[cookie]))
        }
        "enroll" => {
            if owner.is_some() {
                return Ok(back("/login", Some("oidc_already_linked")));
            }
            let user_id = match household::redeem_invite(&mut *tx, ch.text("invite").unwrap_or_default()).await {
                Ok(u) => u,
                Err(AuthError::Refused { code, .. }) => return Ok(back("/login", Some(&code))),
                Err(AuthError::Core(e)) => return Err(e.into()),
            };
            sqlx::query("update app_user set oidc_subject = $1 where id = $2")
                .bind(&res.subject)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
            let cookie = login(tx, meta, settings, user_id, "oidc").await?;
            Ok(redirect("/", &[cookie]))
        }
        "link" => {
            if owner.is_some() && owner != ch.user_id {
                return Ok(back("/settings", Some("oidc_already_linked")));
            }
            sqlx::query("update app_user set oidc_subject = $1 where id = $2")
                .bind(&res.subject)
                .bind(ch.user_id)
                .execute(&mut *tx)
                .await?;
            Ok(back("/settings", None))
        }
        _ => {
            // step_up: the same person at the provider, and a login the provider just asked for
            let (Some(owner), Some(sid)) = (owner, ch.session_id) else {
                return Ok(back("/", Some("oidc_wrong_user")));
            };
            if Some(owner) != ch.user_id {
                return Ok(back("/", Some("oidc_wrong_user")));
            }
            match res.auth_time {
                Some(t) if Utc::now().timestamp() - t <= STEP_UP_SECONDS => {}
                _ => return Ok(back("/", Some("oidc_not_fresh"))),
            }
            sessions::mark_step_up(tx, sid).await?;
            Ok(back("/", None))
        }
    }
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/auth/state", get(state))
        .route("/auth/setup/begin", post(setup_begin))
        .route("/auth/setup/finish", post(setup_finish))
        .route("/auth/setup/password", post(setup_password))
        .route("/auth/login/begin", post(login_begin))
        .route("/auth/login/finish", post(login_finish))
        .route("/auth/recovery", post(recovery_login))
        .route("/auth/password/login", post(password_login))
        .route("/auth/logout", post(logout))
        .route("/auth/step-up/begin", post(step_up_begin))
        .route("/auth/step-up/finish", post(step_up_finish))
        .route("/auth/passkeys", get(list_my_passkeys))
        .route("/auth/passkeys/begin", post(add_passkey_begin))
        .route("/auth/passkeys/finish", post(add_passkey_finish))
        .route("/auth/passkeys/{credential_id}", delete(remove_passkey))
        .route("/auth/recovery-codes", post(regenerate_recovery_codes))
        .route("/auth/invite/{code}", get(invite_info))
        .route("/auth/enroll/begin", post(enroll_begin))
        .route("/auth/enroll/finish", post(enroll_finish))
        .route("/auth/enroll/password", post(enroll_password))
        .route("/auth/oidc/login", get(oidc_login))
        .route("/auth/oidc/callback", get(oidc_callback))
}
