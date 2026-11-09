//! Lane B: Paperless-ngx (ingestion.md 3.1). The person's Paperless workflow ("Document Added", tag
//! `health`) posts `{"doc_url": "{{doc_url}}"}` to `POST /api/ingest/paperless` with the source's push
//! token in `X-OwnHealth-Secret`. We parse the id from `doc_url` and trust nothing else in the body:
//! the document's tags and the original come from the Paperless API with the source's API token
//! (secret purpose `paperless_token`, `Authorization: Token ...`) through `core::net::get`
//! (`Egress::home_service`). The Paperless id is `source_ref` and the per-source dedup key, and the
//! document carries `flags.source_link = {kind: "paperless", id, url}`, the page in Paperless, so the
//! review queue can link back. Polling fallback every 15 min:
//! `tags__id__all=<health>&modified__gt=<cursor>`.
//!
//! Only the id, tags, file name and `modified` of a Paperless document are read. Its title, content,
//! notes and custom fields are free text anyone with Paperless access can write; they are never
//! fetched into OwnHealth or logged.
//!
//! Source `config` (kind `paperless`): `{base_url, health_tag: <tag id or name>, allow_private?: bool,
//! profile_by_tag?: {"<tag id>": profile_id}}`; `source.profile_id` is the default profile.
//!
//! Not done, on purpose: writing the `ownhealth-imported` tag back (ingestion.md 3.1 step 4) needs a
//! token that may write to Paperless, and the setup recommends a read-only one; and pulling the archived
//! PDF for scans (step 3), because the local OCR or the person's agent reads them.

use std::collections::HashMap;

use ownhealth_core::Runtime;
use ownhealth_core::arrive::{Arrival, Arrived, arrive, store_bytes};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::config::Settings;
use ownhealth_core::dedup::seen_item;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::net::{self, Egress};
use ownhealth_core::secrets::{SecretBox, SecretString, read_secret};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source, begin_run, finish_run};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub const DEFAULT_POLL_SECONDS: i32 = 900;
pub const IMPORTED_TAG: &str = "ownhealth-imported";
pub const CHANNEL: &str = "paperless";
const PAGE_SIZE: u32 = 25;
/// At most this many list pages per poll (1000 documents); the cursor carries on next time.
const MAX_PAGES: u32 = 40;

#[derive(Debug, Clone, Deserialize)]
struct Config {
    base_url: String,
    health_tag: Value,
    #[serde(default)]
    allow_private: bool,
    #[serde(default)]
    profile_by_tag: HashMap<String, Uuid>,
}

/// The few fields of a Paperless document we read.
#[derive(Debug, Clone, Deserialize)]
struct DocMeta {
    id: i64,
    #[serde(default)]
    tags: Vec<i64>,
    #[serde(default)]
    original_file_name: Option<String>,
    #[serde(default)]
    modified: Option<String>,
}

/// The Paperless document id in a `doc_url` (`https://paperless.example/documents/123/details`, the
/// API form `/api/documents/123/`, or the bare id).
pub fn document_id_from_url(doc_url: &str) -> Option<i64> {
    let s = doc_url.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().ok().filter(|n| *n > 0);
    }
    let url = url::Url::parse(s).ok()?;
    let segs: Vec<&str> = url.path_segments()?.collect();
    let i = segs.iter().rposition(|p| *p == "documents")?;
    let id = segs.get(i + 1)?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse().ok().filter(|n| *n > 0)
}

/// The page of document `id` in the Paperless web UI.
pub fn document_link(base_url: &str, id: i64) -> String {
    format!("{}/documents/{id}/details", base_url.trim_end_matches('/'))
}

fn config(source: &Source) -> Result<Config> {
    parse_config(&source.config)
}

/// Checks a `paperless` source config before it is saved.
pub fn check_config(config: &Value) -> Result<()> {
    parse_config(config).map(|_| ())
}

fn parse_config(config: &Value) -> Result<Config> {
    let cfg: Config = serde_json::from_value(config.clone())
        .map_err(|e| Error::Invalid(format!("the Paperless source config is incomplete: {e}")))?;
    let base =
        url::Url::parse(&cfg.base_url).map_err(|_| Error::Invalid("the Paperless base_url is not a URL".into()))?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(Error::Invalid("the Paperless base_url must be http(s)".into()));
    }
    Ok(cfg)
}

/// A Paperless API session for one source.
struct Client {
    base: String,
    egress: Egress,
    token: SecretString,
}

impl Client {
    async fn new(conn: &mut PgConnection, settings: &Settings, source: &Source, cfg: &Config) -> Result<Client> {
        let secret_id = source
            .secret_id
            .ok_or_else(|| Error::Invalid("the Paperless source has no API token".into()))?;
        let sb = SecretBox::from_settings(settings)?;
        let token = read_secret(conn, &sb, source.household_id, secret_id).await?;
        Ok(Client {
            base: cfg.base_url.trim_end_matches('/').to_string(),
            egress: Egress::home_service(cfg.allow_private),
            token,
        })
    }

    async fn get(&self, url: &str, what: &str) -> Result<Vec<u8>> {
        let auth = format!("Token {}", self.token.expose());
        let r = net::get(
            url,
            &self.egress,
            &[("authorization", &auth), ("accept", "application/json")],
        )
        .await?;
        if !(200..300).contains(&r.status) {
            return Err(Error::Other(anyhow::anyhow!(
                "Paperless answered {} for {what}",
                r.status
            )));
        }
        Ok(r.body)
    }

    async fn json(&self, url: &str, what: &str) -> Result<Value> {
        let body = self.get(url, what).await?;
        serde_json::from_slice(&body).map_err(|_| Error::Other(anyhow::anyhow!("Paperless sent no JSON for {what}")))
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<String> {
        let mut u = url::Url::parse(&format!("{}{path}", self.base))
            .map_err(|_| Error::Invalid("the Paperless base_url is not a URL".into()))?;
        if !query.is_empty() {
            u.query_pairs_mut().extend_pairs(query);
        }
        Ok(u.to_string())
    }

    async fn meta(&self, id: i64) -> Result<DocMeta> {
        let v = self
            .json(
                &self.url(&format!("/api/documents/{id}/"), &[])?,
                &format!("document {id}"),
            )
            .await?;
        serde_json::from_value(v).map_err(|_| Error::Other(anyhow::anyhow!("Paperless document {id} is not readable")))
    }

    async fn original(&self, id: i64) -> Result<Vec<u8>> {
        self.get(
            &self.url(&format!("/api/documents/{id}/download/"), &[("original", "true")])?,
            &format!("the original of document {id}"),
        )
        .await
    }

    /// The health tag's id: the number in the config, or the tag of that name.
    async fn health_tag(&self, cfg: &Config) -> Result<i64> {
        match &cfg.health_tag {
            Value::Number(n) => n
                .as_i64()
                .ok_or_else(|| Error::Invalid("health_tag is not a tag id".into())),
            Value::String(s) if !s.trim().is_empty() && s.trim().bytes().all(|b| b.is_ascii_digit()) => s
                .trim()
                .parse()
                .map_err(|_| Error::Invalid("health_tag is not a tag id".into())),
            Value::String(name) if !name.trim().is_empty() => {
                let v = self
                    .json(
                        &self.url("/api/tags/", &[("name__iexact", name.trim())])?,
                        "the health tag",
                    )
                    .await?;
                v["results"]
                    .as_array()
                    .and_then(|r| r.first())
                    .and_then(|t| t["id"].as_i64())
                    .ok_or_else(|| Error::Invalid("Paperless has no tag with the configured health_tag name".into()))
            }
            _ => Err(Error::Invalid("the Paperless source needs a health_tag".into())),
        }
    }
}

/// The profile a document goes to: the first of its tags with a rule, else the source's profile.
async fn profile_for(conn: &mut PgConnection, source: &Source, cfg: &Config, tags: &[i64]) -> Result<Uuid> {
    let picked = tags
        .iter()
        .find_map(|t| cfg.profile_by_tag.get(&t.to_string()).copied())
        .or(source.profile_id)
        .ok_or_else(|| Error::Invalid("the Paperless source has no profile for this document".into()))?;
    let ok: Option<i32> = sqlx::query_scalar("select 1 from profile where id = $1 and household_id = $2")
        .bind(picked)
        .bind(source.household_id)
        .fetch_optional(conn)
        .await?;
    ok.map(|_| picked)
        .ok_or_else(|| Error::Invalid("the Paperless source points at a profile of another household".into()))
}

/// Set `flags.source_link` on the document the Paperless item became (or was already).
async fn link(conn: &mut PgConnection, document_id: Uuid, cfg: &Config, id: i64) -> Result<()> {
    sqlx::query("update document set flags = flags || jsonb_build_object('source_link', $2::jsonb) where id = $1")
        .bind(document_id)
        .bind(json!({"kind": CHANNEL, "id": id, "url": document_link(&cfg.base_url, id)}))
        .execute(conn)
        .await?;
    Ok(())
}

fn document_of(a: &Arrived) -> Option<Uuid> {
    match a {
        Arrived::AlreadySeen { document_id } => *document_id,
        Arrived::Duplicate { document_id } => Some(*document_id),
        Arrived::Queued { document_id, .. } => *document_id,
        Arrived::Unsupported { .. } => None,
    }
}

/// Pull one tagged document and hand it to `arrive`. A document this source handed us before is not
/// downloaded again.
async fn pull(
    pool: &PgPool,
    blobs: &BlobStore,
    source: &Source,
    cfg: &Config,
    client: &Client,
    meta: &DocMeta,
) -> Result<Arrived> {
    let ext = meta.id.to_string();
    let mut conn = pool.acquire().await?;
    if let Some(item) = seen_item(&mut conn, source.id, &ext).await? {
        return Ok(Arrived::AlreadySeen {
            document_id: item.document_id,
        });
    }
    let profile_id = profile_for(&mut conn, source, cfg, &meta.tags).await?;
    drop(conn);
    let data = client.original(meta.id).await?;
    let (sha, size) = store_bytes(blobs, data).await?;
    let fallback = format!("paperless-{}", meta.id);
    let filename = meta
        .original_file_name
        .as_deref()
        .filter(|f| !f.trim().is_empty())
        .unwrap_or(&fallback);
    let mut tx = pool.begin().await?;
    let arrived = arrive(
        &mut tx,
        blobs,
        &Arrival {
            source_id: Some(source.id),
            source_ref: Some(&ext),
            external_id: Some(&ext),
            filename: Some(filename),
            ..Arrival::new(
                source.household_id,
                profile_id,
                CHANNEL,
                &sha,
                i64::try_from(size).unwrap_or(i64::MAX),
            )
        },
    )
    .await?;
    if let Some(d) = document_of(&arrived) {
        link(&mut tx, d, cfg, meta.id).await?;
    }
    tx.commit().await?;
    Ok(arrived)
}

fn count(o: &mut RunOutcome, a: &Arrived) {
    o.seen += 1;
    match a {
        Arrived::Queued { .. } => o.new += 1,
        Arrived::Duplicate { .. } | Arrived::AlreadySeen { .. } => o.duplicate += 1,
        Arrived::Unsupported { .. } => o.problems.push(RunProblem {
            code: "unsupported".into(),
            message: "Paperless sent a file type OwnHealth does not import".into(),
        }),
    }
}

async fn webhook(rt: &Runtime, source: &Source, body: &Value) -> Result<Arrived> {
    let id = body
        .get("doc_url")
        .and_then(Value::as_str)
        .and_then(document_id_from_url)
        .ok_or_else(|| {
            Error::Invalid("doc_url has no Paperless document id (is PAPERLESS_URL set in Paperless?)".into())
        })?;
    let cfg = config(source)?;
    let client = {
        let mut conn = rt.pool.acquire().await?;
        Client::new(&mut conn, &rt.settings, source, &cfg).await?
    };
    let meta = client.meta(id).await?;
    let tag = client.health_tag(&cfg).await?;
    if !meta.tags.contains(&tag) {
        return Err(Error::Invalid(format!(
            "Paperless document {id} is not tagged with the health tag"
        )));
    }
    pull(&rt.pool, &rt.blobs, source, &cfg, &client, &meta).await
}

/// The webhook body of one "Document Added": pull the original and arrive it. The push is recorded
/// as a run of the source, so its health shows a broken token or an unreachable Paperless.
pub async fn handle_webhook(rt: &Runtime, source: &Source, body: &Value) -> Result<Arrived> {
    if source.kind != CHANNEL {
        return Err(Error::Invalid(
            "this token does not belong to a Paperless source".into(),
        ));
    }
    let run_id = {
        let mut conn = rt.pool.acquire().await?;
        begin_run(&mut conn, source.id, "push", None).await?
    };
    let result = webhook(rt, source, body).await;
    let mut conn = rt.pool.acquire().await?;
    match &result {
        Ok(a) => {
            let mut o = RunOutcome::default();
            count(&mut o, a);
            finish_run(&mut conn, run_id, Ok(&o)).await?;
        }
        Err(e) => finish_run(&mut conn, run_id, Err(&e.to_string())).await?,
    }
    result
}

async fn poll(ctx: &JobContext, source: &Source) -> Result<RunOutcome> {
    let cfg = config(source)?;
    let client = {
        let mut conn = ctx.pool.acquire().await?;
        Client::new(&mut conn, &ctx.settings, source, &cfg).await?
    };
    let tag = client.health_tag(&cfg).await?.to_string();
    let since = source
        .cursor
        .as_ref()
        .and_then(|c| c.get("modified"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut out = RunOutcome::default();
    // the cursor moves up to the last document handled before the first failure, so a failed pull
    // is retried next time (and those after it are skipped as seen)
    let mut cursor = since.clone();
    let mut failed = false;
    let size = PAGE_SIZE.to_string();
    for page in 1..=MAX_PAGES {
        let n = page.to_string();
        let mut q = vec![
            ("tags__id__all", tag.as_str()),
            ("ordering", "modified"),
            ("page_size", size.as_str()),
            ("page", n.as_str()),
            ("truncate_content", "true"),
        ];
        if let Some(s) = &since {
            q.push(("modified__gt", s.as_str()));
        }
        let list = client
            .json(&client.url("/api/documents/", &q)?, "the document list")
            .await?;
        let results: Vec<DocMeta> = list["results"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter_map(|d| serde_json::from_value(d.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        for meta in &results {
            match pull(&ctx.pool, &ctx.blobs, source, &cfg, &client, meta).await {
                Ok(a) => {
                    count(&mut out, &a);
                    if !failed && meta.modified.is_some() {
                        cursor.clone_from(&meta.modified);
                    }
                }
                Err(e) => {
                    out.seen += 1;
                    failed = true;
                    out.problems.push(RunProblem {
                        code: "pull_failed".into(),
                        message: format!("document {}: {e}", meta.id),
                    });
                }
            }
        }
        if list["next"].is_null() || results.is_empty() {
            break;
        }
    }
    out.cursor = cursor.map(|m| json!({"modified": m}));
    Ok(out)
}

/// The `paperless` poller (the fallback when Paperless cannot reach us).
pub async fn poll_paperless(ctx: JobContext, source: Source) -> std::result::Result<RunOutcome, JobError> {
    poll(&ctx, &source).await.map_err(JobError::from)
}

pub fn register() {
    ownhealth_core::sources::register_poller("paperless", |ctx, source| Box::pin(poll_paperless(ctx, source)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_ids_come_from_the_url_path_only() {
        for (url, id) in [
            ("https://paperless.example/documents/123/details", Some(123)),
            ("https://paperless.example/documents/123/", Some(123)),
            ("http://192.168.1.5:8000/paperless/documents/7/details?x=1", Some(7)),
            ("https://paperless.example/api/documents/42/", Some(42)),
            ("88", Some(88)),
            (" 88 ", Some(88)),
            ("0", None),
            ("https://paperless.example/documents/abc/details", None),
            ("https://paperless.example/documents/", None),
            ("https://paperless.example/?documents=5", None),
            ("null", None),
            ("", None),
            ("documents/5", None),
        ] {
            assert_eq!(document_id_from_url(url), id, "{url}");
        }
    }

    #[test]
    fn the_link_points_at_the_paperless_page() {
        assert_eq!(
            document_link("http://paperless.lan:8000/", 7),
            "http://paperless.lan:8000/documents/7/details"
        );
    }
}
