//! Lane B (v0.2): the Paperless-ngx webhook and poller, the PWA share target and the iOS Shortcut
//! over REST, and three-level dedup across the inboxes. Paperless is a local axum fake on 127.0.0.1;
//! nothing reaches the internet. Synthetic data only (the Rivera family, LabNorte).

mod lane_i;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use http::{Request, StatusCode, header};
use lane_i::{App, R};
use ownhealth_core::Runtime;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use ownhealth_core::jobs::{self, JobContext};
use ownhealth_core::secrets::{SecretBox, SecretString, store_secret};
use ownhealth_core::sources::{self, NewSource, Source};
use ownhealth_core::testing::TestDb;
use ownhealth_inbox::{email, folder, paperless};
use serde_json::{Value, json};
use uuid::Uuid;

/// 32 zero bytes: a test-only secrets key.
const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const PL_TOKEN: &str = "pl-test-token";
const HEALTH_TAG: i64 = 3;

async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    settings.secrets_key = Some(KEY.into());
    settings.inbox_dir = Some(dir.path().join("inbox"));
    let rt = Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    drop(c);
    Ok(App { db, dir, rt, family })
}

/// A LabNorte report for Alex. The last row is a marker no catalog knows, so the report always has
/// at least one row a person must look at.
fn report(report_id: &str) -> Vec<u8> {
    lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id,
        pages: vec![vec![
            LabRow::new("Glucosa", "92", "mg/dL", "70 - 110"),
            LabRow::new("Colesterol LDL", "97", "mg/dL", "< 130"),
            LabRow::new("Índice Norte-Q", "3,1", "U", "1 - 5"),
        ]],
    })
    .0
}

fn note_fixture() -> anyhow::Result<Value> {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../fixtures/paperless/note-injection.json"
    );
    Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

// ------------------------------------------------------- fake Paperless ----

#[derive(Clone, Default)]
struct Fake {
    docs: Arc<Mutex<Vec<Value>>>,
    files: Arc<Mutex<HashMap<i64, Vec<u8>>>>,
    downloads: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
    /// `/download/` answers 500, as a Paperless with a broken storage does.
    broken_downloads: Arc<AtomicBool>,
    list_queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl Fake {
    fn add(&self, meta: Value, pdf: Vec<u8>) {
        let id = meta["id"].as_i64().unwrap_or_default();
        self.docs.lock().expect("lock").push(meta);
        self.files.lock().expect("lock").insert(id, pdf);
    }
}

/// The status the fake answers instead of the resource: down, or a wrong API token.
fn authorized(fake: &Fake, h: &HeaderMap) -> Result<(), StatusCode> {
    if fake.down.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if h.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Token {PL_TOKEN}")) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

async fn fake_meta(State(f): State<Fake>, h: HeaderMap, Path(id): Path<i64>) -> axum::response::Response {
    if let Err(r) = authorized(&f, &h) {
        return r.into_response();
    }
    let docs = f.docs.lock().expect("lock").clone();
    match docs.into_iter().find(|d| d["id"] == json!(id)) {
        Some(d) => axum::Json(d).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fake_download(
    State(f): State<Fake>,
    h: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    if let Err(r) = authorized(&f, &h) {
        return r.into_response();
    }
    // only the original is ever asked for
    if q.get("original").map(String::as_str) != Some("true") {
        return StatusCode::IM_A_TEAPOT.into_response();
    }
    if f.broken_downloads.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    f.downloads.fetch_add(1, Ordering::SeqCst);
    match f.files.lock().expect("lock").get(&id) {
        Some(b) => ([(header::CONTENT_TYPE, "application/pdf")], b.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fake_tags(
    State(f): State<Fake>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    if let Err(r) = authorized(&f, &h) {
        return r.into_response();
    }
    let results = if q.get("name__iexact").map(|s| s.to_lowercase()) == Some("health".into()) {
        json!([{"id": HEALTH_TAG, "name": "health"}])
    } else {
        json!([])
    };
    axum::Json(json!({"count": results.as_array().map_or(0, Vec::len), "results": results})).into_response()
}

async fn fake_list(
    State(f): State<Fake>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> axum::response::Response {
    if let Err(r) = authorized(&f, &h) {
        return r.into_response();
    }
    f.list_queries.lock().expect("lock").push(q.clone());
    let tag: i64 = q.get("tags__id__all").and_then(|t| t.parse().ok()).unwrap_or(-1);
    let mut docs: Vec<Value> = f
        .docs
        .lock()
        .expect("lock")
        .iter()
        .filter(|d| d["tags"].as_array().is_some_and(|t| t.contains(&json!(tag))))
        .filter(|d| {
            q.get("modified__gt")
                .is_none_or(|gt| d["modified"].as_str().unwrap_or("") > gt.as_str())
        })
        .cloned()
        .collect();
    docs.sort_by(|a, b| a["modified"].as_str().cmp(&b["modified"].as_str()));
    let size: usize = q.get("page_size").and_then(|s| s.parse().ok()).unwrap_or(25);
    let page: usize = q.get("page").and_then(|s| s.parse().ok()).unwrap_or(1);
    let chunk: Vec<Value> = docs.iter().skip((page - 1) * size).take(size).cloned().collect();
    let next = if page * size < docs.len() {
        json!("more")
    } else {
        Value::Null
    };
    axum::Json(json!({"count": docs.len(), "next": next, "results": chunk})).into_response()
}

/// Start the fake; returns its base URL.
async fn serve(fake: Fake) -> anyhow::Result<String> {
    let app = axum::Router::new()
        .route("/api/documents/", get(fake_list))
        .route("/api/documents/{id}/", get(fake_meta))
        .route("/api/documents/{id}/download/", get(fake_download))
        .route("/api/tags/", get(fake_tags))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{addr}"))
}

fn meta(id: i64, tags: &[i64], modified: &str) -> Value {
    json!({"id": id, "title": format!("Documento {id}"), "tags": tags, "modified": modified,
           "original_file_name": format!("labnorte-{id}.pdf"), "notes": []})
}

/// A Paperless source with its API token sealed and a push token issued. Returns (source, push token).
async fn paperless_source(a: &App, config: Value, api_token: &str) -> anyhow::Result<(Source, String)> {
    let mut c = a.conn().await?;
    let sb = SecretBox::from_settings(&a.rt.settings)?;
    let secret = store_secret(
        &mut c,
        &sb,
        a.family.household_id,
        "paperless_token",
        Some("Paperless"),
        &SecretString::new(api_token),
    )
    .await?;
    let id = sources::create_source(
        &mut c,
        &NewSource {
            profile_id: Some(a.family.profile("alex")),
            config,
            secret_id: Some(secret),
            poll_seconds: Some(paperless::DEFAULT_POLL_SECONDS),
            ..NewSource::new(a.family.household_id, "paperless", "Paperless de casa")
        },
    )
    .await?;
    let token = sources::issue_token(&mut c, id).await?;
    Ok((sources::get_source(&mut c, id).await?.expect("source"), token))
}

async fn push_source(a: &App, kind: &str, profile: Option<&str>) -> anyhow::Result<(Source, String)> {
    let mut c = a.conn().await?;
    let id = sources::create_source(
        &mut c,
        &NewSource {
            profile_id: profile.map(|p| a.family.profile(p)),
            ..NewSource::new(a.family.household_id, kind, "iPhone de Alex")
        },
    )
    .await?;
    let token = sources::issue_token(&mut c, id).await?;
    Ok((sources::get_source(&mut c, id).await?.expect("source"), token))
}

async fn webhook(a: &App, token: Option<&str>, body: &str) -> anyhow::Result<(StatusCode, Value)> {
    let mut b = Request::post("/ingest/paperless").header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header("x-ownhealth-secret", t);
    }
    a.send(b.body(Body::from(body.to_string()))?).await
}

fn job_ctx(a: &App) -> JobContext {
    JobContext::new(a.rt.pool.clone(), a.rt.settings.clone(), a.rt.blobs.clone())
}

/// Run every queued `import.pdf` job, as the worker would.
async fn run_imports(a: &App) -> anyhow::Result<usize> {
    ownhealth_ingest_pdf::register();
    let kinds = ["import.pdf".to_string()];
    let mut n = 0;
    while jobs::run_one(&job_ctx(a), "core", Some(&kinds)).await?.is_some() {
        n += 1;
    }
    Ok(n)
}

async fn last_run(a: &App, source: Uuid) -> anyhow::Result<Value> {
    Ok(
        sqlx::query_scalar("select to_jsonb(r) from source_run r where source_id = $1 order by id desc limit 1")
            .bind(source)
            .fetch_one(&a.rt.pool)
            .await?,
    )
}

// --------------------------------------------------------------- tests ----

#[tokio::test]
async fn a_health_document_shows_in_the_queue_with_its_paperless_link_within_a_minute() -> R {
    let a = app().await?;
    let fake = Fake::default();
    fake.add(note_fixture()?, report("LN-PL-7"));
    let base = serve(fake.clone()).await?;
    let (source, token) = paperless_source(
        &a,
        json!({"base_url": format!("{base}/"), "health_tag": HEALTH_TAG, "allow_private": true}),
        PL_TOKEN,
    )
    .await?;

    // PDFium scans the system fonts the first time a process opens a PDF that uses a standard font
    // (about 20 s on a Mac, next to nothing in the container). A server pays it once, at its first
    // PDF, so it stays out of the timed window: open one PDF first.
    let warm = report("LN-PL-WARM");
    tokio::task::spawn_blocking(move || ownhealth_ingest_pdf::extract::extract(&warm).map(|_| ())).await??;

    let start = Instant::now();
    let doc_url = json!({"doc_url": format!("{base}/documents/7/details")}).to_string();
    let (s, v) = webhook(&a, Some(&token), &doc_url).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["outcome"].as_str(), v["kind"].as_str()),
        (Some("queued"), Some("pdf")),
        "{v}"
    );
    let doc = v["document_id"].as_str().unwrap_or_default().to_string();
    assert_eq!(run_imports(&a).await?, 1);
    let (s, queue) = a.get("alex", &format!("/review?document_id={doc}")).await?;
    assert_eq!(s, StatusCode::OK, "{queue}");
    assert!(
        queue["items"].as_array().is_some_and(|i| !i.is_empty()),
        "the document is in the review queue: {queue}"
    );
    let (s, d) = a.get("alex", &format!("/documents/{doc}")).await?;
    assert_eq!(s, StatusCode::OK, "{d}");
    let elapsed = start.elapsed();
    assert_eq!(
        d["flags"]["source_link"],
        json!({"kind": "paperless", "id": 7, "url": format!("{base}/documents/7/details")}),
        "{d}"
    );
    println!("paperless exit criterion: webhook to queue {elapsed:?}");
    assert!(elapsed < Duration::from_secs(60), "webhook to queue took {elapsed:?}");

    // the Paperless id is the source link; only the original was downloaded
    let (channel, source_ref, sid): (String, Option<String>, Option<Uuid>) =
        sqlx::query_as("select channel, source_ref, source_id from document_source where document_id = $1::uuid")
            .bind(&doc)
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!(
        (channel.as_str(), source_ref.as_deref(), sid),
        ("paperless", Some("7"), Some(source.id))
    );
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
    let run = last_run(&a, source.id).await?;
    assert_eq!(
        (&run["trigger"], &run["status"], &run["items_new"]),
        (&json!("push"), &json!("ok"), &json!(1))
    );

    // Paperless fires again (a re-run workflow): nothing is downloaded or stored twice
    let (s, v) = webhook(&a, Some(&token), &doc_url).await?;
    assert_eq!(
        (s, v["outcome"].as_str()),
        (StatusCode::OK, Some("already_seen")),
        "{v}"
    );
    assert_eq!(v["document_id"].as_str(), Some(doc.as_str()));
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(a.count("select count(*) from document").await?, 1);

    // the title, content, notes and custom fields of the Paperless document never enter OwnHealth
    let leaked = a
        .count(
            "select count(*) from (\
               select to_jsonb(t)::text x from document t union all \
               select to_jsonb(t)::text from document_source t union all \
               select to_jsonb(t)::text from document_page t union all \
               select to_jsonb(t)::text from source_item t union all \
               select to_jsonb(t)::text from source_run t union all \
               select to_jsonb(t)::text from source t union all \
               select to_jsonb(t)::text from job t union all \
               select to_jsonb(t)::text from review_item t union all \
               select to_jsonb(t)::text from audit_event t) s where x like '%CANARY-PL%'",
        )
        .await?;
    assert_eq!(leaked, 0);
    Ok(())
}

#[tokio::test]
async fn the_webhook_refuses_what_it_should_and_says_why_on_the_source() -> R {
    let a = app().await?;
    let fake = Fake::default();
    fake.add(meta(8, &[9], "2026-09-14T10:00:00+02:00"), report("LN-PL-8"));
    fake.add(meta(9, &[HEALTH_TAG], "2026-09-14T10:00:00+02:00"), report("LN-PL-9"));
    let base = serve(fake.clone()).await?;
    let cfg = json!({"base_url": base, "health_tag": HEALTH_TAG, "allow_private": true});
    let (source, token) = paperless_source(&a, cfg.clone(), PL_TOKEN).await?;
    let body = |id: i64| json!({"doc_url": format!("{base}/documents/{id}/details")}).to_string();

    // no token, a token of another kind of source
    assert_eq!(webhook(&a, None, &body(9)).await?.0, StatusCode::UNAUTHORIZED);
    let (_, shortcut_token) = push_source(&a, "shortcut", Some("alex")).await?;
    let (s, v) = webhook(&a, Some(&shortcut_token), &body(9)).await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("wrong_source"))
    );
    // a body that is not JSON, a doc_url without an id (PAPERLESS_URL unset gives null)
    assert_eq!(webhook(&a, Some(&token), "nope").await?.0, StatusCode::BAD_REQUEST);
    for b in [
        json!({"doc_url": null}),
        json!({}),
        json!({"doc_url": "https://x/documents/abc/"}),
    ] {
        assert_eq!(
            webhook(&a, Some(&token), &b.to_string()).await?.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    // not tagged health: refused, nothing downloaded, the run says so
    let (s, v) = webhook(&a, Some(&token), &body(8)).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 0);
    let run = last_run(&a, source.id).await?;
    assert_eq!(run["status"], "error");
    assert!(run["error"].as_str().is_some_and(|e| e.contains("health tag")), "{run}");

    // Paperless down: the source health shows it, and the next good push clears it
    fake.down.store(true, Ordering::SeqCst);
    let (s, _) = webhook(&a, Some(&token), &body(9)).await?;
    assert!(s.is_server_error(), "{s}");
    let mut c = a.conn().await?;
    let h = sources::source_health(&mut c, a.family.household_id).await?;
    let me = h.iter().find(|x| x.id == source.id).expect("health");
    assert!(me.consecutive_failures >= 2, "{me:?}");
    assert!(me.last_error.as_deref().is_some_and(|e| e.contains("503")), "{me:?}");
    fake.down.store(false, Ordering::SeqCst);
    let (s, v) = webhook(&a, Some(&token), &body(9)).await?;
    assert_eq!((s, v["outcome"].as_str()), (StatusCode::OK, Some("queued")), "{v}");
    let h = sources::source_health(&mut c, a.family.household_id).await?;
    assert_eq!(
        h.iter().find(|x| x.id == source.id).map(|x| x.consecutive_failures),
        Some(0)
    );

    // a wrong API token, and a Paperless on the LAN without "on my network": refused
    let (_, bad) = paperless_source(&a, cfg, "wrong-token").await?;
    assert!(webhook(&a, Some(&bad), &body(9)).await?.0.is_server_error());
    let (lan, lan_token) = paperless_source(&a, json!({"base_url": base, "health_tag": HEALTH_TAG}), PL_TOKEN).await?;
    assert!(webhook(&a, Some(&lan_token), &body(9)).await?.0.is_server_error());
    let run = last_run(&a, lan.id).await?;
    assert!(run["error"].as_str().is_some_and(|e| e.contains("private")), "{run}");
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn the_poller_backfills_by_tag_routes_by_tag_and_moves_its_cursor() -> R {
    let a = app().await?;
    let fake = Fake::default();
    fake.add(meta(7, &[HEALTH_TAG], "2026-09-14T09:00:00+02:00"), report("LN-PL-P7"));
    fake.add(meta(8, &[9], "2026-09-14T09:30:00+02:00"), report("LN-PL-P8"));
    fake.add(
        meta(10, &[HEALTH_TAG, 12], "2026-09-15T09:00:00+02:00"),
        report("LN-PL-P10"),
    );
    let base = serve(fake.clone()).await?;
    let lucia = a.family.profile("lucia");
    let (source, _) = paperless_source(
        &a,
        json!({"base_url": base, "health_tag": "Health", "allow_private": true,
               "profile_by_tag": {"12": lucia}}),
        PL_TOKEN,
    )
    .await?;

    let o = paperless::poll_paperless(job_ctx(&a), source.clone()).await?;
    assert_eq!((o.seen, o.new, o.problems.len()), (2, 2, 0), "{o:?}");
    assert_eq!(o.cursor, Some(json!({"modified": "2026-09-15T09:00:00+02:00"})));
    let profiles: Vec<(String, Uuid)> = sqlx::query_as(
        "select s.source_ref, d.profile_id from document_source s join document d on d.id = s.document_id order by 1",
    )
    .fetch_all(&a.rt.pool)
    .await?;
    assert_eq!(
        profiles,
        vec![("10".into(), lucia), ("7".into(), a.family.profile("alex"))]
    );
    {
        let q = fake.list_queries.lock().expect("lock");
        assert_eq!(q[0].get("tags__id__all").map(String::as_str), Some("3"));
        assert!(!q[0].contains_key("modified__gt"));
    }

    // with the cursor saved, the next poll asks only for what changed since
    fake.add(
        meta(11, &[HEALTH_TAG], "2026-09-16T08:00:00+02:00"),
        report("LN-PL-P11"),
    );
    let mut c = a.conn().await?;
    sources::save_cursor(&mut c, source.id, o.cursor.as_ref().expect("cursor")).await?;
    let source = sources::get_source(&mut c, source.id).await?.expect("source");
    let o = paperless::poll_paperless(job_ctx(&a), source).await?;
    assert_eq!((o.seen, o.new), (1, 1), "{o:?}");
    let q = fake.list_queries.lock().expect("lock");
    assert_eq!(
        q.last().and_then(|q| q.get("modified__gt")).map(String::as_str),
        Some("2026-09-15T09:00:00+02:00")
    );
    assert_eq!(fake.downloads.load(Ordering::SeqCst), 3);
    Ok(())
}

/// A poll in which every pull fails is a failed sync, through the real job path: the source counts
/// failed runs, keeps no last success and keeps its cursor, so the documents are tried again.
#[tokio::test]
async fn a_poll_where_every_pull_fails_is_not_a_successful_sync() -> R {
    let a = app().await?;
    let fake = Fake::default();
    fake.add(meta(7, &[HEALTH_TAG], "2026-09-14T09:00:00+02:00"), report("LN-PL-F7"));
    fake.add(meta(8, &[HEALTH_TAG], "2026-09-14T09:30:00+02:00"), report("LN-PL-F8"));
    fake.broken_downloads.store(true, Ordering::SeqCst);
    let base = serve(fake.clone()).await?;
    let (source, _) = paperless_source(
        &a,
        json!({"base_url": base, "health_tag": "health", "allow_private": true}),
        PL_TOKEN,
    )
    .await?;
    sources::register();
    paperless::register();
    for expected in 1..=2 {
        let mut c = a.conn().await?;
        sources::enqueue_poll(&mut c, source.id).await?;
        jobs::drain(&job_ctx(&a), "core", 10, None).await?;
        let s = sources::get_source(&mut c, source.id).await?.expect("source");
        assert_eq!(s.consecutive_failures, expected, "run {expected}");
        assert!(s.last_success_at.is_none(), "run {expected}: {:?}", s.last_success_at);
        assert!(
            s.last_error.as_deref().is_some_and(|e| e.starts_with("pull_failed")),
            "{:?}",
            s.last_error
        );
        assert!(s.cursor.is_none(), "{:?}", s.cursor);
        assert_eq!(last_run(&a, source.id).await?["status"], json!("error"));
    }
    let docs: i64 = sqlx::query_scalar("select count(*) from document_source where channel = 'paperless'")
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(docs, 0);

    // Paperless is fixed: the next poll brings both documents in and the source is healthy again
    fake.broken_downloads.store(false, Ordering::SeqCst);
    let mut c = a.conn().await?;
    sources::enqueue_poll(&mut c, source.id).await?;
    jobs::drain(&job_ctx(&a), "core", 10, None).await?;
    let s = sources::get_source(&mut c, source.id).await?.expect("source");
    assert_eq!((s.consecutive_failures, s.last_success_at.is_some()), (0, true));
    assert_eq!(last_run(&a, source.id).await?["status"], json!("ok"));
    Ok(())
}

// ---------------------------------------------------------- phone pushes ----

const BOUNDARY: &str = "ownhealth-push-boundary";

fn multipart(fields: &[(&str, &str)], files: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    for (field, filename, bytes) in files {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn push(uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> anyhow::Result<Request<Body>> {
    let mut b = Request::post(uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    Ok(b.body(Body::from(body))?)
}

#[tokio::test]
async fn the_share_target_takes_files_from_a_session_only() -> R {
    let a = app().await?;
    let alex = a.cookie("alex").await?;
    let (one, two) = (report("LN-SH-1"), report("LN-SH-2"));
    let body = multipart(
        &[("profile", "lucia")],
        &[("files", "a.pdf", &one), ("files", "b.pdf", &two)],
    );
    let (s, v) = a.send(push("/ingest/share", &[("cookie", &alex)], body)?).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2);
    assert!(
        items.iter().all(|i| i["outcome"] == "queued" && i["kind"] == "pdf"),
        "{v}"
    );
    assert_eq!(items[0]["filename"], "a.pdf");
    let rows: Vec<(String, Uuid)> =
        sqlx::query_as("select s.channel, d.profile_id from document_source s join document d on d.id = s.document_id")
            .fetch_all(&a.rt.pool)
            .await?;
    assert_eq!(rows, vec![("share".into(), a.family.profile("lucia")); 2]);
    assert_eq!(
        a.count("select count(*) from audit_event where tool = 'POST /api/ingest/share'")
            .await?,
        1
    );

    // opened by the phone as a page, without a profile: the person's own, then the review queue
    let body = multipart(&[], &[("files", "c.pdf", &report("LN-SH-3"))]);
    let (s, h, _) = a
        .send_raw(push(
            "/ingest/share",
            &[("cookie", &alex), ("sec-fetch-mode", "navigate")],
            body,
        )?)
        .await?;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        h.get(header::LOCATION).and_then(|l| l.to_str().ok()),
        Some("/import-review")
    );
    assert_eq!(
        a.count(&format!(
            "select count(*) from document where filename = 'c.pdf' and profile_id = '{}'",
            a.family.profile("alex")
        ))
        .await?,
        1
    );

    // refused: no session, an access token, another site, a profile the caller may not import to,
    // and a push with no file
    let f = || multipart(&[("profile", "alex")], &[("files", "d.pdf", b"%PDF-1.4\n")]);
    assert_eq!(
        a.send(push("/ingest/share", &[], f())?).await?.0,
        StatusCode::UNAUTHORIZED
    );
    let pat = format!("Bearer {}", a.pat().await?);
    assert_eq!(
        a.send(push("/ingest/share", &[("authorization", &pat)], f())?).await?.0,
        StatusCode::FORBIDDEN
    );
    let (s, v) = a
        .send(push(
            "/ingest/share",
            &[("cookie", &alex), ("sec-fetch-site", "cross-site")],
            f(),
        )?)
        .await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("csrf_failed"))
    );
    let sam = a.cookie("sam").await?;
    let body = multipart(&[("profile", "carmen")], &[("files", "e.pdf", b"%PDF-1.4\n")]);
    let s = a.send(push("/ingest/share", &[("cookie", &sam)], body)?).await?.0;
    assert!(matches!(s, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "{s}");
    let (s, v) = a
        .send(push(
            "/ingest/share",
            &[("cookie", &alex)],
            multipart(&[("profile", "alex")], &[]),
        )?)
        .await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("no_files"))
    );
    assert_eq!(a.count("select count(*) from document").await?, 3);
    Ok(())
}

#[tokio::test]
async fn the_shortcut_pushes_with_a_token_bound_to_one_profile() -> R {
    let a = app().await?;
    let (source, token) = push_source(&a, "shortcut", Some("lucia")).await?;
    let bearer = format!("Bearer {token}");
    let pdf = report("LN-SC-1");
    let (s, v) = a
        .send(push(
            "/ingest/shortcut",
            &[("authorization", &bearer)],
            multipart(&[], &[("file", "Foto.pdf", &pdf)]),
        )?)
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["outcome"], "queued", "{v}");
    let (channel, sid, profile): (String, Option<Uuid>, Uuid) = sqlx::query_as(
        "select s.channel, s.source_id, d.profile_id from document_source s join document d on d.id = s.document_id",
    )
    .fetch_one(&a.rt.pool)
    .await?;
    assert_eq!(
        (channel.as_str(), sid, profile),
        ("shortcut", Some(source.id), a.family.profile("lucia"))
    );
    // the profile field of the form cannot move it elsewhere
    let (_, v) = a
        .send(push(
            "/ingest/shortcut",
            &[("authorization", &bearer)],
            multipart(&[("profile", "carmen")], &[("file", "Otra.pdf", &report("LN-SC-2"))]),
        )?)
        .await?;
    let doc = v["items"][0]["document_id"].as_str().unwrap_or_default();
    let p: Uuid = sqlx::query_scalar("select profile_id from document where id = $1::uuid")
        .bind(doc)
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(p, a.family.profile("lucia"));
    // the same file again is a duplicate, and the source's run says so
    let (_, v) = a
        .send(push(
            "/ingest/shortcut",
            &[("authorization", &bearer)],
            multipart(&[], &[("file", "x.pdf", &pdf)]),
        )?)
        .await?;
    assert_eq!(v["items"][0]["outcome"], "duplicate", "{v}");
    let run = last_run(&a, source.id).await?;
    assert_eq!((&run["status"], &run["items_duplicate"]), (&json!("ok"), &json!(1)));

    // refused: a session, a Paperless token, a Shortcut source with no profile
    let alex = a.cookie("alex").await?;
    let f = || multipart(&[], &[("file", "y.pdf", b"%PDF-1.4\n")]);
    assert_eq!(
        a.send(push("/ingest/shortcut", &[("cookie", &alex)], f())?).await?.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, pl) = push_source(&a, "paperless", Some("alex")).await?;
    let (s, v) = a
        .send(push(
            "/ingest/shortcut",
            &[("authorization", &format!("Bearer {pl}"))],
            f(),
        )?)
        .await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("wrong_source"))
    );
    let (loose, loose_token) = push_source(&a, "shortcut", None).await?;
    let (s, _) = a
        .send(push(
            "/ingest/shortcut",
            &[("authorization", &format!("Bearer {loose_token}"))],
            f(),
        )?)
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(last_run(&a, loose.id).await?["status"], "error");
    Ok(())
}

// --------------------------------------------- dedup across the inboxes ----

fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for (i, c) in data.chunks(3).enumerate() {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..4 {
            out.push(if k <= c.len() {
                T[(n >> (18 - 6 * k) & 63) as usize] as char
            } else {
                '='
            });
        }
        if i % 19 == 18 {
            out.push_str("\r\n");
        }
    }
    out
}

fn lab_mail(pdf: &[u8]) -> Vec<u8> {
    format!(
        "From: LabNorte <resultados@labnorte.example>\r\nTo: salud@rivera.example\r\n\
         Subject: Sus resultados\r\nMessage-ID: <ln-77@labnorte.example>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n--m\r\nContent-Type: text/plain\r\n\r\n\
         Adjuntamos su informe.\r\n--m\r\nContent-Type: application/pdf; name=\"informe.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"informe.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
         {}\r\n--m--\r\n",
        b64(pdf)
    )
    .into_bytes()
}

#[tokio::test]
async fn the_same_pdf_by_email_folder_and_paperless_is_one_document() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let pdf = report("LN-DEDUP");

    // 1. email: an allowlisted sender (lane A's path)
    let mut c = a.conn().await?;
    let mail_id = sources::create_source(
        &mut c,
        &NewSource {
            config: json!({"host": "imap.rivera.example", "username": "salud", "trusted_authserv_id": "mx.rivera.example"}),
            ..NewSource::new(a.family.household_id, "email", "Buzón de salud")
        },
    )
    .await?;
    email::add_rule(
        &mut c,
        mail_id,
        &email::SenderRuleIn {
            pattern: "resultados@labnorte.example".into(),
            profile_id: alex,
            doc_type_hint: None,
            trust_unauthenticated: true,
        },
        a.family.user("alex"),
    )
    .await?;
    let mail = sources::get_source(&mut c, mail_id).await?.expect("source");
    let mut tx = a.rt.pool.begin().await?;
    let out = email::ingest_message(&mut tx, &a.rt.blobs, None, &mail, &lab_mail(&pdf), None).await?;
    tx.commit().await?;
    assert!(out.quarantine_id.is_none(), "{out:?}");

    // 2. the watched folder
    let (folder_src, _) = push_source(&a, "folder", None).await?;
    let inbox = a.dir.path().join("inbox");
    std::fs::create_dir_all(inbox.join("alex"))?;
    std::fs::write(inbox.join("alex").join("escaneo.pdf"), &pdf)?;
    let o = folder::poll_root(&a.rt.pool, &a.rt.blobs, &folder_src, &inbox, Duration::ZERO).await?;
    assert_eq!((o.new, o.duplicate), (0, 1), "{o:?}");

    // 3. Paperless
    let fake = Fake::default();
    fake.add(meta(21, &[HEALTH_TAG], "2026-09-14T09:00:00+02:00"), pdf.clone());
    let base = serve(fake).await?;
    let (_, token) = paperless_source(
        &a,
        json!({"base_url": base, "health_tag": HEALTH_TAG, "allow_private": true}),
        PL_TOKEN,
    )
    .await?;
    let (s, v) = webhook(
        &a,
        Some(&token),
        &json!({"doc_url": format!("{base}/documents/21/")}).to_string(),
    )
    .await?;
    assert_eq!((s, v["outcome"].as_str()), (StatusCode::OK, Some("duplicate")), "{v}");

    // level 1: one document, one import, three ways in; the Paperless link is on it too
    assert_eq!(a.count("select count(*) from document").await?, 1);
    let channels: Vec<String> = sqlx::query_scalar("select channel from document_source order by channel")
        .fetch_all(&a.rt.pool)
        .await?;
    assert_eq!(channels, ["email", "folder", "paperless"]);
    assert_eq!(run_imports(&a).await?, 1);
    let doc: Uuid = sqlx::query_scalar("select id from document")
        .fetch_one(&a.rt.pool)
        .await?;
    let link: Value = sqlx::query_scalar("select flags->'source_link' from document where id = $1")
        .bind(doc)
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(link["id"], 21);

    // levels 2 and 3: the same report in other bytes (a phone share) is processed and its rows are
    // recognised as the facts already held: no second observation
    let facts = a.count("select count(*) from observation").await?;
    let mut copy = pdf.clone();
    copy.extend_from_slice(b"\n%reenviado desde el movil\n");
    let body = multipart(&[("profile", "alex")], &[("files", "copia.pdf", &copy)]);
    let cookie = a.cookie("alex").await?;
    let (s, v) = a.send(push("/ingest/share", &[("cookie", &cookie)], body)?).await?;
    assert_eq!(
        (s, v["items"][0]["outcome"].as_str()),
        (StatusCode::OK, Some("queued")),
        "{v}"
    );
    assert_eq!(run_imports(&a).await?, 1);
    assert_eq!(a.count("select count(*) from document").await?, 2);
    assert_eq!(a.count("select count(*) from observation").await?, facts);
    let confirmed = a
        .count("select count(*) from candidate_row where state = 'confirmed' and observation_id is not null")
        .await?;
    assert!(confirmed >= 1, "the copy's rows link to the facts already held");
    Ok(())
}
