//! Shared by the lane G REST tests (`#[path = "auth_support.rs"] mod support;`): an app with the real
//! auth stack on https://testserver, and a client that keeps cookies as a browser does
//! (0.1.1's `tests/security/conftest.py`).
#![allow(dead_code)]

use std::collections::BTreeMap;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use ownhealth_api::routes::auth::OidcOverride;
use ownhealth_auth::oidc::OidcConfig;
use ownhealth_auth::sessions;
use ownhealth_core::Runtime;
use ownhealth_core::config::Settings;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub const BASE: &str = "https://testserver";
pub const LAN: &str = "http://192.168.1.20:8080";

pub struct Env {
    pub db: TestDb,
    pub dir: tempfile::TempDir,
    pub settings: Settings,
    pub rt: Runtime,
    pub fam: Option<Family>,
}

impl Env {
    /// An empty instance (first run), or the Rivera family committed.
    pub async fn new(family: bool) -> anyhow::Result<Env> {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let mut settings = db.settings(dir.path())?;
        settings.dev_auth = false;
        settings.base_url = BASE.into();
        let rt = Runtime::with_pool(settings.clone(), db.pool.clone())?;
        let fam = if family {
            let mut c = db.pool.acquire().await?;
            Some(create_rivera_family(&mut c, "Rivera").await?)
        } else {
            None
        };
        Ok(Env {
            db,
            dir,
            settings,
            rt,
            fam,
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.db.pool
    }

    pub fn fam(&self) -> &Family {
        match &self.fam {
            Some(f) => f,
            None => panic!("no family in this test"),
        }
    }

    pub fn user(&self, slug: &str) -> Uuid {
        self.fam().user(slug)
    }

    pub fn profile(&self, slug: &str) -> Uuid {
        self.fam().profile(slug)
    }

    /// An anonymous client on this instance.
    pub fn anon(&self) -> Client {
        self.client_with(&self.settings.base_url.clone(), None)
    }

    /// The same instance served under another base URL (a LAN address).
    pub fn anon_at(&self, base: &str) -> Client {
        self.client_with(base, None)
    }

    pub fn client_with(&self, base: &str, oidc: Option<OidcConfig>) -> Client {
        let mut settings = self.settings.clone();
        settings.base_url = base.into();
        let rt = Runtime {
            settings: std::sync::Arc::new(settings),
            pool: self.rt.pool.clone(),
            blobs: self.rt.blobs.clone(),
        };
        let mut app = Router::new().nest("/api", ownhealth_api::router());
        if let Some(cfg) = oidc {
            app = app.layer(axum::Extension(OidcOverride(Some(cfg))));
        }
        let host = base.split_once("://").map_or(base, |(_, h)| h).to_string();
        Client {
            app: app.with_state(rt),
            host,
            cookies: BTreeMap::new(),
        }
    }

    /// `client_for(user_id, step_up=..., method=...)`: logged in through a real session cookie.
    pub async fn login(&self, user: Uuid, step_up: bool, method: &str) -> anyhow::Result<Client> {
        let mut client = self.anon();
        self.log_in(&mut client, user, step_up, method).await?;
        Ok(client)
    }

    pub async fn as_user(&self, user: Uuid) -> anyhow::Result<Client> {
        self.login(user, true, "passkey").await
    }

    pub async fn stale(&self, user: Uuid) -> anyhow::Result<Client> {
        self.login(user, false, "passkey").await
    }

    pub async fn log_in(&self, client: &mut Client, user: Uuid, step_up: bool, method: &str) -> anyhow::Result<()> {
        let mut c = self.db.pool.acquire().await?;
        let secret =
            sessions::create_session_with(&mut c, user, None, Some("cargo test"), method, Some(step_up)).await?;
        client.cookies.insert(sessions::COOKIE.into(), secret);
        Ok(())
    }

    pub async fn scalar_i64(&self, sql: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_one(&self.db.pool)
            .await?)
    }
}

#[derive(Clone)]
pub struct Client {
    pub app: Router,
    pub host: String,
    pub cookies: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: Vec<u8>,
    pub json: Value,
}

impl Resp {
    pub fn code(&self) -> &str {
        self.json["detail"]["code"].as_str().unwrap_or_default()
    }

    pub fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    pub fn location(&self) -> String {
        self.header("location")
    }

    pub fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect()
    }
}

impl Client {
    pub async fn send(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Resp> {
        let mut req = Request::builder().method(method).uri(path).header("host", &self.host);
        if !self.cookies.is_empty() {
            let jar: Vec<String> = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
            req = req.header("cookie", jar.join("; "));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(b)?))?,
            None => req.body(Body::empty())?,
        };
        self.dispatch(req).await
    }

    /// A request with a body of any type (a multipart upload).
    pub async fn send_raw(
        &mut self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> anyhow::Result<Resp> {
        let mut req = Request::builder().method(method).uri(path).header("host", &self.host);
        if !self.cookies.is_empty() {
            let jar: Vec<String> = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
            req = req.header("cookie", jar.join("; "));
        }
        let req = req.header("content-type", content_type).body(Body::from(body))?;
        self.dispatch(req).await
    }

    async fn dispatch(&mut self, req: Request<Body>) -> anyhow::Result<Resp> {
        let res = self.app.clone().oneshot(req).await?;
        let status = res.status();
        let headers = res.headers().clone();
        for c in headers.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()) {
            let first = c.split(';').next().unwrap_or_default();
            if let Some((k, v)) = first.split_once('=') {
                if c.contains("Max-Age=0") {
                    self.cookies.remove(k);
                } else {
                    self.cookies.insert(k.into(), v.into());
                }
            }
        }
        let bytes = to_bytes(res.into_body(), 64 << 20).await?.to_vec();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Resp {
            status,
            headers,
            bytes,
            json,
        })
    }

    pub async fn get(&mut self, path: &str) -> anyhow::Result<Resp> {
        self.send(Method::GET, path, None, &[]).await
    }

    pub async fn post(&mut self, path: &str, body: &Value) -> anyhow::Result<Resp> {
        self.send(Method::POST, path, Some(body), &[]).await
    }

    /// A POST with no body (begin routes).
    pub async fn post0(&mut self, path: &str) -> anyhow::Result<Resp> {
        self.send(Method::POST, path, None, &[]).await
    }

    pub async fn put(&mut self, path: &str, body: &Value) -> anyhow::Result<Resp> {
        self.send(Method::PUT, path, Some(body), &[]).await
    }

    pub async fn delete(&mut self, path: &str) -> anyhow::Result<Resp> {
        self.send(Method::DELETE, path, None, &[]).await
    }

    /// The logged-in user per `/api/auth/state` (Python's tests read `/api/me`, lane I's).
    pub async fn me(&mut self) -> anyhow::Result<Value> {
        Ok(self.get("/api/auth/state").await?.json["user"].clone())
    }

    pub async fn step_up_fresh(&mut self) -> anyhow::Result<bool> {
        Ok(self.get("/api/auth/state").await?.json["step_up_fresh"] == Value::Bool(true))
    }
}
