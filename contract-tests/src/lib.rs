//! Lane J: black-box contract tests. They talk only HTTP to a running backend at
//! `OWNHEALTH_CONTRACT_BASE_URL`, started by `run.sh rust` on a fresh database with
//! OWNHEALTH_DEMO=1 and OWNHEALTH_DEV_AUTH=1 (REST acts as Alex Rivera, the first owner, unless a
//! request carries a session cookie).
//!
//! Every test is `#[ignore]` so a plain `cargo test` of the workspace stays offline; `run.sh`
//! runs them with `--include-ignored`, and fails when the base URL is missing.
//!
//! The one thing that is not HTTP: a few tests set a token's limits (`api_token.rate_per_min`, ...),
//! which a person edits in the database in 0.1.1 (no route). `psql` does that through the test
//! Postgres container, on the database `run.sh` exported as `OWNHEALTH_DATABASE_URL`.
//!
//! Tests share one server and one database. Each test that writes works on data it made itself (a
//! new relative profile, its own tokens, its own uploads), so the tests stay independent and the
//! demo profiles the shape tests read stay as the seed left them.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use reqwest::Method;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

pub mod pdf;
pub mod shape;

pub const BASE_URL_ENV: &str = "OWNHEALTH_CONTRACT_BASE_URL";

/// Every scope a token can carry (core/access.py ALL_SCOPES).
pub const ALL_SCOPES: [&str; 10] = [
    "read:documents_text",
    "read:genome",
    "read:genome_raw",
    "read:labs",
    "read:records",
    "read:sensitive",
    "read:summary",
    "read:vitals",
    "read:wearables",
    "write:propose",
];

/// The backend under test, e.g. `http://127.0.0.1:18080`.
pub fn base_url() -> anyhow::Result<String> {
    let url = std::env::var(BASE_URL_ENV)
        .with_context(|| format!("{BASE_URL_ENV} is not set: run contract-tests/run.sh"))?;
    Ok(url.trim_end_matches('/').to_string())
}

/// A short random suffix for names this test creates.
pub fn uniq() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{:x}{:x}{}",
        std::process::id() % 4096,
        nanos % 65536,
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// `uniq()` in lower-case letters only (names are matched on letter tokens).
pub fn uniq_alpha() -> String {
    uniq()
        .chars()
        .map(|c| match c.to_digit(10) {
            Some(d) => char::from(b'g' + d as u8),
            None => c,
        })
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// "Noa Xyz" as a lab slip prints it: "XYZ, NOA".
pub fn patient_line(display_name: &str) -> String {
    let mut parts = display_name.split_whitespace();
    let given = parts.next().unwrap_or_default();
    let surname: Vec<&str> = parts.collect();
    format!(
        "{}, {}",
        surname.join(" ").to_uppercase(),
        given.to_uppercase()
    )
}

/// One HTTP answer: status, headers, raw body and the body as JSON (Null when it is not JSON).
#[derive(Debug)]
pub struct Res {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub json: Value,
}

impl Res {
    /// `detail.code` of a FastAPI-style error body.
    pub fn code(&self) -> Option<&str> {
        self.json["detail"]["code"].as_str()
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    /// Every `Set-Cookie` header for `name`, whole (value and attributes).
    pub fn set_cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{name}=")))
            .map(str::to_string)
    }

    /// The value of a cookie this answer set.
    pub fn cookie_value(&self, name: &str) -> Option<String> {
        let c = self.set_cookie(name)?;
        let v = c
            .split(';')
            .next()?
            .split_once('=')?
            .1
            .trim_matches('"')
            .to_string();
        Some(v)
    }

    /// Fail with the body when the status is not `want`.
    pub fn expect(self, want: u16) -> anyhow::Result<Res> {
        if self.status != want {
            bail!(
                "expected HTTP {want}, got {}: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            );
        }
        Ok(self)
    }
}

/// A REST client. Without a session cookie the backend's dev auth makes it Alex Rivera.
#[derive(Clone)]
pub struct Client {
    pub base: String,
    pub http: reqwest::Client,
    pub session: Option<String>,
}

impl Client {
    pub fn new() -> anyhow::Result<Client> {
        Ok(Client {
            base: base_url()?,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            session: None,
        })
    }

    /// The same client riding on an `oh_session` cookie.
    pub fn with_session(&self, secret: &str) -> Client {
        Client {
            session: Some(secret.to_string()),
            ..self.clone()
        }
    }

    /// Send a request; `headers` are added as given (e.g. `("origin", "https://evil.example")`).
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Res> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.base));
        if let Some(s) = &self.session {
            req = req.header("cookie", format!("oh_session={s}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(serde_json::to_vec(b)?);
        }
        let res = read(req.send().await?).await?;
        Ok(res)
    }

    /// GET a path and return (status, JSON body or Null).
    pub async fn get_json(&self, path: &str) -> anyhow::Result<(u16, Value)> {
        let r = self.get(path).await?;
        Ok((r.status, r.json))
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<Res> {
        self.send(Method::GET, path, None, &[]).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> anyhow::Result<Res> {
        self.send(Method::POST, path, Some(body), &[]).await
    }

    pub async fn put(&self, path: &str, body: &Value) -> anyhow::Result<Res> {
        self.send(Method::PUT, path, Some(body), &[]).await
    }

    pub async fn delete(&self, path: &str) -> anyhow::Result<Res> {
        self.send(Method::DELETE, path, None, &[]).await
    }

    /// A multipart/form-data POST: text fields, then files `(field, filename, bytes, content type)`.
    pub async fn multipart(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        files: &[(&str, &str, &[u8], &str)],
    ) -> anyhow::Result<Res> {
        let boundary = format!("ohcontract{}", uniq());
        let mut body: Vec<u8> = Vec::new();
        for (k, v) in fields {
            body.extend(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
                )
                .as_bytes(),
            );
        }
        for (field, name, bytes, ctype) in files {
            body.extend(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{name}\"\r\nContent-Type: {ctype}\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend(b"\r\n");
        }
        body.extend(format!("--{boundary}--\r\n").as_bytes());
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        if let Some(s) = &self.session {
            req = req.header("cookie", format!("oh_session={s}"));
        }
        read(req.send().await?).await
    }

    /// Upload files to a profile (`POST /api/imports`), expecting 200.
    pub async fn upload(
        &self,
        profile: &str,
        files: &[(&str, &[u8], &str)],
    ) -> anyhow::Result<Value> {
        let parts: Vec<(&str, &str, &[u8], &str)> = files
            .iter()
            .map(|(n, b, t)| ("files", *n, *b, *t))
            .collect();
        let r = self
            .multipart("/api/imports", &[("profile", profile)], &parts)
            .await?
            .expect(200)?;
        Ok(r.json)
    }

    /// Poll `/api/jobs?ids=` until every job is done or dead; returns the job rows.
    pub async fn wait_jobs(&self, ids: &[i64]) -> anyhow::Result<Vec<Value>> {
        let q: Vec<String> = ids.iter().map(i64::to_string).collect();
        for _ in 0..240 {
            let r = self
                .get(&format!("/api/jobs?ids={}", q.join(",")))
                .await?
                .expect(200)?;
            let jobs = r.json["jobs"].as_array().cloned().unwrap_or_default();
            if jobs.len() == ids.len()
                && jobs
                    .iter()
                    .all(|j| matches!(j["state"].as_str(), Some("done" | "dead")))
            {
                return Ok(jobs);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        bail!("jobs {ids:?} did not finish in 60 s")
    }

    /// Upload and wait for the import jobs; returns (upload answer, finished jobs).
    pub async fn import(
        &self,
        profile: &str,
        files: &[(&str, &[u8], &str)],
    ) -> anyhow::Result<(Value, Vec<Value>)> {
        let up = self.upload(profile, files).await?;
        let ids: Vec<i64> = up["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["job_id"].as_i64())
            .collect();
        let jobs = if ids.is_empty() {
            vec![]
        } else {
            self.wait_jobs(&ids).await?
        };
        Ok((up, jobs))
    }

    /// A new relative (a profile without a login) this client's user is guardian of, with a
    /// unique slug so parallel tests never race for one. Returns the slug.
    pub async fn new_relative(&self) -> anyhow::Result<String> {
        Ok(self.new_patient().await?.0)
    }

    /// Wait until a fresh GET of `path` mentions `needle`. 0.1.1 commits a request's transaction
    /// after its response is sent, so a write used at once can still be invisible.
    pub async fn settle(&self, path: &str, needle: &str) -> anyhow::Result<()> {
        for _ in 0..100 {
            let r = self.get(path).await?;
            if r.status == 200 && String::from_utf8_lossy(&r.body).contains(needle) {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        bail!("GET {path} never showed {needle}")
    }

    /// A relative with this display name and birth date (and a unique slug).
    pub async fn relative_named(
        &self,
        name: &str,
        birth: &str,
        sex: &str,
    ) -> anyhow::Result<String> {
        let r = self
            .post(
                "/api/household/profiles",
                &json!({"display_name": name, "role": "guardian", "relationship": "other", "birth_date": birth,
                        "sex_at_birth": sex, "country": "ES", "slug": format!("t-{}", uniq_alpha())}),
            )
            .await?
            .expect(201)?;
        let slug = r.json["slug"]
            .as_str()
            .map(str::to_string)
            .context("no slug")?;
        self.settle("/api/profiles", &format!("\"{slug}\"")).await?;
        Ok(slug)
    }

    /// `new_relative`, also returning the display name ("Noa <letters>", born 1980-04-02), so a
    /// lab report can name the patient: `pdf::lab_report(&patient_line(name), "02/04/1980", ..)`.
    pub async fn new_patient(&self) -> anyhow::Result<(String, String)> {
        let u = uniq_alpha();
        let name = format!("Noa {}", capitalize(&u));
        let r = self
            .post(
                "/api/household/profiles",
                &json!({"display_name": name, "role": "guardian", "relationship": "other",
                        "birth_date": "1980-04-02", "sex_at_birth": "female", "country": "ES", "slug": format!("t-{u}")}),
            )
            .await?
            .expect(201)?;
        let slug = r.json["slug"]
            .as_str()
            .map(str::to_string)
            .context("no slug")?;
        self.settle("/api/profiles", &format!("\"{slug}\"")).await?;
        Ok((slug, name))
    }

    /// A PAT made by this client's user (`POST /api/tokens`); returns (token JSON, plaintext).
    pub async fn token(&self, body: Value) -> anyhow::Result<(Value, String)> {
        let mut b = json!({"label": format!("contract {}", uniq()), "profiles": ["alex"]});
        if let (Some(m), Some(extra)) = (b.as_object_mut(), body.as_object()) {
            for (k, v) in extra {
                m.insert(k.clone(), v.clone());
            }
        }
        let r = self.post("/api/tokens", &b).await?.expect(201)?;
        let plain = r.json["plaintext"]
            .as_str()
            .context("no plaintext")?
            .to_string();
        let id = r.json["token"]["id"].as_str().context("no id")?;
        self.settle("/api/tokens", id).await?;
        Ok((r.json["token"].clone(), plain))
    }

    /// A token with these scopes on these profiles, as an MCP client speaking 2025-11-25.
    pub async fn mcp(&self, scopes: &[&str], profiles: &[&str]) -> anyhow::Result<Mcp> {
        let (_, plain) = self
            .token(json!({"scopes": scopes, "profiles": profiles}))
            .await?;
        Mcp::new(&plain)
    }

    /// The audit rows Alex can see (newest first).
    pub async fn audit(&self, limit: u32) -> anyhow::Result<Vec<Value>> {
        let r = self
            .get(&format!("/api/audit?limit={limit}"))
            .await?
            .expect(200)?;
        Ok(r.json["items"].as_array().cloned().unwrap_or_default())
    }

    /// Invite a member and enrol them with a password (the plain-HTTP path). Returns
    /// (user id, display name, session cookie value, password, recovery codes).
    pub async fn enrolled_member(&self, name: &str) -> anyhow::Result<Member> {
        let inv = self
            .post(
                "/api/household/invites",
                &json!({"display_name": name, "locale": "en"}),
            )
            .await?
            .expect(201)?;
        let code = inv.json["code"].as_str().context("no code")?.to_string();
        self.settle("/api/household", name).await?;
        let password = format!("correct horse {}", uniq());
        let r = self
            .post(
                "/api/auth/enroll/password",
                &json!({"code": code, "password": password}),
            )
            .await?
            .expect(200)?;
        let session = r.cookie_value("oh_session").context("no session cookie")?;
        self.with_session(&session).settle("/api/me", name).await?;
        Ok(Member {
            user_id: r.json["user_id"]
                .as_str()
                .context("no user_id")?
                .to_string(),
            name: name.to_string(),
            password,
            recovery_codes: r.json["recovery_codes"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            client: self.with_session(&session),
            session,
        })
    }
}

/// A member enrolled through an invite, with a live browser session.
pub struct Member {
    pub user_id: String,
    pub name: String,
    pub password: String,
    pub recovery_codes: Vec<Value>,
    pub session: String,
    pub client: Client,
}

impl Member {
    /// Confirm it is them (password step-up), so step-up routes pass for 5 minutes.
    pub async fn step_up(&self) -> anyhow::Result<()> {
        let b = self
            .client
            .post("/api/auth/step-up/begin", &json!({}))
            .await?
            .expect(200)?;
        if b.json["method"] != "password" {
            bail!("step-up method is {}, expected password", b.json["method"]);
        }
        self.client
            .post(
                "/api/auth/step-up/finish",
                &json!({"password": self.password}),
            )
            .await?
            .expect(200)?;
        Ok(())
    }
}

async fn read(res: reqwest::Response) -> anyhow::Result<Res> {
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.bytes().await?.to_vec();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Ok(Res {
        status,
        headers,
        body,
        json,
    })
}

// ------------------------------------------------------------------------------------ MCP ----

pub const LEGACY: &str = "2025-11-25";
/// How the server's instructions start (mcp/server.py INSTRUCTIONS).
pub const INSTRUCTIONS_PREFIX: &str =
    "OwnHealth is a personal health record. Call get_health_status first.";
pub const MODERN: &str = "2026-07-28";

/// A minimal streamable-HTTP MCP client: one POST per JSON-RPC request, stateless as the server.
/// `version` LEGACY sends the MCP-Protocol-Version header; MODERN also sends the per-request
/// `_meta` envelope and the `Mcp-Method` / `Mcp-Name` headers.
pub struct Mcp {
    pub base: String,
    pub http: reqwest::Client,
    pub token: String,
    pub version: &'static str,
    next: AtomicU64,
}

/// One tools/call result.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub is_error: bool,
    /// structuredContent: the envelope, or the error body when is_error
    pub sc: Value,
    /// the text block
    pub text: String,
    pub raw: Value,
}

impl ToolResult {
    pub fn code(&self) -> Option<&str> {
        if self.is_error {
            self.sc["code"].as_str()
        } else {
            None
        }
    }

    pub fn data(&self) -> &Value {
        &self.sc["data"]
    }
}

impl Mcp {
    pub fn new(token: &str) -> anyhow::Result<Mcp> {
        Ok(Mcp {
            base: base_url()?,
            http: reqwest::Client::builder().build()?,
            token: token.to_string(),
            version: LEGACY,
            next: AtomicU64::new(1),
        })
    }

    pub fn modern(mut self) -> Mcp {
        self.version = MODERN;
        self
    }

    /// POST one JSON-RPC message with extra headers; the raw HTTP answer.
    pub async fn post_raw(&self, msg: &Value, headers: &[(&str, &str)]) -> anyhow::Result<Res> {
        let mut req = self
            .http
            .post(format!("{}/mcp", self.base))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.token))
            .header("mcp-protocol-version", self.version);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let mut r = read(req.body(serde_json::to_vec(msg)?).send().await?).await?;
        if r.json.is_null() {
            // an SSE answer: the last `data:` line is the response
            let text = String::from_utf8_lossy(&r.body).to_string();
            if let Some(line) = text.lines().rev().find(|l| l.starts_with("data:")) {
                r.json = serde_json::from_str(line.trim_start_matches("data:").trim())
                    .unwrap_or(Value::Null);
            }
        }
        Ok(r)
    }

    /// One JSON-RPC request; returns the whole response message (`result` or `error`).
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut params = params;
        let mut headers: Vec<(&str, String)> = Vec::new();
        if self.version == MODERN {
            if let Some(p) = params.as_object_mut() {
                let meta = p.entry("_meta").or_insert_with(|| json!({}));
                if let Some(m) = meta.as_object_mut() {
                    m.insert(
                        "io.modelcontextprotocol/protocolVersion".into(),
                        json!(MODERN),
                    );
                    m.insert(
                        "io.modelcontextprotocol/clientCapabilities".into(),
                        json!({}),
                    );
                    m.insert(
                        "io.modelcontextprotocol/clientInfo".into(),
                        json!({"name": "contract-tests", "version": "0"}),
                    );
                }
            }
            headers.push(("mcp-method", method.to_string()));
            let name = match method {
                "tools/call" | "prompts/get" => params["name"].as_str(),
                "resources/read" => params["uri"].as_str(),
                _ => None,
            };
            if let Some(n) = name {
                headers.push(("mcp-name", n.to_string()));
            }
        }
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let hdrs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = self.post_raw(&msg, &hdrs).await?;
        if r.json.is_null() {
            bail!(
                "{method}: HTTP {} with no JSON-RPC body: {}",
                r.status,
                String::from_utf8_lossy(&r.body)
            );
        }
        Ok(r.json)
    }

    /// `result` of a request, or an error naming the JSON-RPC error.
    pub async fn result(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let msg = self.request(method, params).await?;
        if !msg["error"].is_null() {
            bail!("{method} failed: {}", msg["error"]);
        }
        Ok(msg["result"].clone())
    }

    /// The legacy handshake (2025-11-25); returns the initialize result.
    pub async fn initialize(&self) -> anyhow::Result<Value> {
        self.result(
            "initialize",
            json!({"protocolVersion": LEGACY, "capabilities": {}, "clientInfo": {"name": "contract-tests", "version": "0"}}),
        )
        .await
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<Value>> {
        let r = self.result("tools/list", json!({})).await?;
        Ok(r["tools"].as_array().cloned().unwrap_or_default())
    }

    pub async fn tool_names(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .list_tools()
            .await?
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect())
    }

    pub async fn call(&self, name: &str, args: Value) -> anyhow::Result<ToolResult> {
        let r = self
            .result("tools/call", json!({"name": name, "arguments": args}))
            .await?;
        let text = r["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult {
            is_error: r["isError"].as_bool().unwrap_or(false),
            sc: r["structuredContent"].clone(),
            text,
            raw: r,
        })
    }

    /// A successful tools/call: its envelope (fails on isError).
    pub async fn ok(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let r = self.call(name, args).await?;
        if r.is_error {
            bail!("{name} returned an error: {}", r.sc);
        }
        Ok(r.sc)
    }
}

/// The keys every tool envelope carries (mcp/tools/__init__.py OUTPUT_SCHEMA).
pub const ENVELOPE_KEYS: [&str; 14] = [
    "profile",
    "as_of",
    "generated_at",
    "lang",
    "identity_mode",
    "data",
    "evidence",
    "limitations",
    "not_assessed",
    "withheld",
    "db_versions",
    "next_cursor",
    "budget",
    "notice",
];

/// The envelope contract of mcp/envelope.py, checked on every successful tool answer.
pub fn assert_envelope(env: &Value) -> anyhow::Result<()> {
    let keys: Vec<&str> = env
        .as_object()
        .context("envelope is not an object")?
        .keys()
        .map(String::as_str)
        .collect();
    let mut want = ENVELOPE_KEYS.to_vec();
    want.sort_unstable();
    let mut got = keys.clone();
    got.sort_unstable();
    if got != want {
        bail!("envelope keys {got:?}, expected {want:?}");
    }
    let w = &env["withheld"];
    if !(w["sensitive"].is_i64() && w["by_scope"].is_array() && w["minor_rule"].is_i64()) {
        bail!("bad withheld {w}");
    }
    if !(env["budget"]["tokens_est"].is_i64() && env["budget"]["truncated"].is_boolean()) {
        bail!("bad budget {}", env["budget"]);
    }
    if !env["notice"]
        .as_str()
        .is_some_and(|n| n.starts_with("Information from your records, not "))
    {
        bail!("notice is {}", env["notice"]);
    }
    let generated = env["generated_at"].as_str().unwrap_or("");
    if !regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")?.is_match(generated) {
        bail!("generated_at {generated:?} is not second-precision UTC with Z");
    }
    Ok(())
}

/// Every `untrusted_text` wrapper in a JSON value: (origin, text).
pub fn untrusted(v: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(v, &mut |o: &Map<String, Value>| {
        if o.get("trust").and_then(Value::as_str) == Some("untrusted_text") {
            out.push((
                o.get("origin")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                o.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ));
        }
    });
    out
}

/// Every string under a key named `src` / in `src_ids` / matching a src id, anywhere.
pub fn src_ids(v: &Value) -> Vec<String> {
    let re = regex::Regex::new(r"^src_[a-z0-9_]+$").ok();
    let mut out = Vec::new();
    fn go(v: &Value, re: &Option<regex::Regex>, out: &mut Vec<String>) {
        match v {
            Value::String(s) if re.as_ref().is_some_and(|r| r.is_match(s)) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| go(x, re, out)),
            Value::Object(o) => o.values().for_each(|x| go(x, re, out)),
            _ => {}
        }
    }
    go(v, &re, &mut out);
    out
}

pub fn walk(v: &Value, f: &mut dyn FnMut(&Map<String, Value>)) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
        Value::Object(o) => {
            f(o);
            o.values().for_each(|x| walk(x, f));
        }
        _ => {}
    }
}

// ------------------------------------------------------------------------- the database ----

/// Run one SQL statement on the backend's database (through the test Postgres container) and
/// return its output lines (`psql -At`). Only for setup that has no route in 0.1.1.
pub fn psql(sql: &str) -> anyhow::Result<Vec<String>> {
    let url = std::env::var("OWNHEALTH_DATABASE_URL")
        .context("OWNHEALTH_DATABASE_URL is not set: run contract-tests/run.sh")?;
    let db = url
        .rsplit('/')
        .next()
        .context("no database in the URL")?
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    let out = std::process::Command::new("docker")
        .args([
            "exec",
            "ownhealth-testpg",
            "psql",
            "-U",
            "postgres",
            "-d",
            &db,
            "-qAtc",
            sql,
        ])
        .output()
        .context("docker exec psql")?;
    if !out.status.success() {
        bail!("psql failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Sorted strings of a JSON array.
pub fn strings(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect();
    out.sort();
    out
}

/// The synthetic fixtures at the repo root (`fixtures/`).
pub fn fixture(path: &str) -> anyhow::Result<Vec<u8>> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("fixtures")
        .join(path);
    std::fs::read(&p).with_context(|| format!("reading {}", p.display()))
}

/// Files of this crate (`contract-tests/data/...`).
pub fn data(path: &str) -> anyhow::Result<Vec<u8>> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join(path);
    std::fs::read(&p).with_context(|| format!("reading {}", p.display()))
}
