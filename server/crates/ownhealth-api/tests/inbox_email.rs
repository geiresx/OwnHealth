//! Lane A (v0.2): email-in over REST (quarantine, release, senders, PDF passwords) and the ROADMAP
//! exit criterion end to end: a password-protected LabNorte report from an allowlisted sender, polled
//! from an IMAP server by the scheduler's `source.poll` job, unlocked, imported by `import.pdf` and in
//! the review queue in under two minutes. Synthetic data only.

#[path = "../../ownhealth-inbox/tests/email_support/fake_imap.rs"]
mod fake_imap;
#[path = "../../ownhealth-inbox/tests/email_support/mail.rs"]
mod mail;
#[path = "auth_support.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use fake_imap::{FakeImap, Mode};
use mail::{LAB, Mail, base64};
use ownhealth_core::Runtime;
use ownhealth_core::jobs::{JobContext, run_one};
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::sources::{self, NewSource, SCHEDULER_SECONDS};
use ownhealth_inbox::email::{self, SenderRuleIn};
use serde_json::{Value, json};
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

const IMAP_USER: &str = "salud.rivera";
const IMAP_PASS: &str = "app-pass-9Wd4-synthetic";
/// The receiving server of the shared injection fixture `inj-email-body.eml`.
const TRUSTED: &str = "mx.rivera.example";

async fn env() -> anyhow::Result<Env> {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(base64(&[5u8; 32]));
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    Ok(env)
}

fn sb(env: &Env) -> anyhow::Result<SecretBox> {
    Ok(SecretBox::from_settings(&env.settings)?)
}

/// A mailbox on `port` with LabNorte allowed for Alex.
async fn mailbox(env: &Env, port: u16) -> anyhow::Result<Uuid> {
    let mut c = env.pool().acquire().await?;
    let h = env.fam().household_id;
    let secret = secrets::store_secret(
        &mut c,
        &sb(env)?,
        h,
        "imap_password",
        None,
        &SecretString::new(IMAP_PASS),
    )
    .await?;
    let id = sources::create_source(
        &mut c,
        &NewSource {
            config: json!({"host": "127.0.0.1", "port": port, "tls": "none", "username": IMAP_USER,
                           "trusted_authserv_id": TRUSTED}),
            secret_id: Some(secret),
            poll_seconds: Some(email::DEFAULT_POLL_SECONDS),
            ..NewSource::new(h, "email", "Correo de salud")
        },
    )
    .await?;
    email::add_rule(
        &mut c,
        id,
        &SenderRuleIn {
            pattern: "labnorte.example".into(),
            profile_id: env.profile("alex"),
            doc_type_hint: None,
            trust_unauthenticated: false,
        },
        env.user("alex"),
    )
    .await?;
    Ok(id)
}

fn fixture(path: &str) -> anyhow::Result<Vec<u8>> {
    Ok(std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../fixtures")
            .join(path),
    )?)
}

fn job_ctx(env: &Env) -> JobContext {
    JobContext {
        pool: env.rt.pool.clone(),
        settings: Arc::clone(&env.rt.settings),
        blobs: env.rt.blobs.clone(),
        worker_id: "test".into(),
    }
}

/// Run queued jobs until none is left.
async fn drain(ctx: &JobContext) -> R {
    for _ in 0..50 {
        if run_one(ctx, "core", None).await?.is_none() {
            return Ok(());
        }
    }
    anyhow::bail!("the job queue did not drain in 50 runs")
}

async fn hold(env: &Env, source: Uuid, raw: &[u8]) -> anyhow::Result<Uuid> {
    let mut c = env.pool().acquire().await?;
    let s = sources::get_source(&mut c, source)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no source"))?;
    let out = email::ingest_message(&mut c, &env.rt.blobs, None, &s, raw, None).await?;
    out.quarantine_id.ok_or_else(|| anyhow::anyhow!("not held: {out:?}"))
}

#[tokio::test]
async fn a_locked_lab_report_from_an_allowed_sender_reaches_the_review_queue_in_under_two_minutes() -> R {
    ownhealth_ingest_pdf::register();
    ownhealth_inbox::register();
    sources::register();
    let env = env().await?;
    let imap = FakeImap::start(IMAP_USER, IMAP_PASS, Mode::Plain).await?;
    let source = mailbox(&env, imap.addr.port()).await?;
    let mut c = env.pool().acquire().await?;
    let rule = email::list_rules(&mut c, source).await?[0].id;
    email::add_pdf_password(
        &mut c,
        &sb(&env)?,
        rule,
        &SecretString::new("12345678Z"),
        Some("DNI de Alex"),
    )
    .await?;

    // PDFium scans the system fonts the first time a process opens a PDF that uses a standard font
    // (about 20 s on a Mac, next to nothing in the container). A server pays it once, at its first
    // PDF, so it stays out of the timed window: open one PDF first.
    let warm = fixture("labslips/injection/inj-paperless-note.pdf")?;
    tokio::task::spawn_blocking(move || ownhealth_ingest_pdf::extract::extract(&warm).map(|_| ())).await??;

    // The worst case: the source was polled a moment before the mail arrived.
    sqlx::query("update source set last_run_at = now() where id = $1")
        .bind(source)
        .execute(&mut *c)
        .await?;
    let locked = fixture("A-email/labnorte-locked-12345678Z.pdf")?;
    let mail = Mail::new("ln-al7@labnorte.example", LAB)
        .auth(format!(
            "{TRUSTED}; dkim=pass header.d=labnorte.example; spf=pass smtp.mailfrom={LAB}"
        ))
        .attach("LN-20261102-AL7.pdf", "application/pdf", locked)
        .build();
    imap.deliver(mail);
    imap.deliver(fixture("labslips/injection/inj-email-body.eml")?);
    imap.deliver(fixture("A-email/injection/inj-email-body-unknown-sender.eml")?);

    // Test clock: the scheduler (every SCHEDULER_SECONDS) picks the source up exactly one poll
    // interval after its last run, never before.
    let back = |secs: i32| {
        sqlx::query("update source set last_run_at = now() - make_interval(secs => $2) where id = $1")
            .bind(source)
            .bind(secs)
    };
    back(email::DEFAULT_POLL_SECONDS - 1).execute(&mut *c).await?;
    assert!(sources::schedule_due(&mut c).await?.is_empty());
    back(email::DEFAULT_POLL_SECONDS).execute(&mut *c).await?;
    let started = Instant::now();
    assert_eq!(sources::schedule_due(&mut c).await?.len(), 1);
    drain(&job_ctx(&env)).await?;
    let work = started.elapsed();
    let worst = Duration::from_secs(email::DEFAULT_POLL_SECONDS as u64 + SCHEDULER_SECONDS) + work;
    eprintln!("email exit criterion: worst case {worst:?} (work {work:?})");
    assert!(worst < Duration::from_secs(120), "worst case {worst:?} (work {work:?})");

    // the run: three messages, two imported, one held; the mailbox emptied into Done
    let (status, new, quarantined): (String, i32, i32) = sqlx::query_as(
        "select status, items_new, items_quarantined from source_run where source_id = $1 and finished_at is not null",
    )
    .bind(source)
    .fetch_one(&mut *c)
    .await?;
    assert_eq!((status.as_str(), new, quarantined), ("ok", 2, 1));
    assert_eq!((imap.count("INBOX"), imap.count(email::DEFAULT_DONE_FOLDER)), (0, 3));

    // the unlocked copy was read: its four rows are accepted or waiting in the review queue
    let copy: Uuid = sqlx::query_scalar("select id from document where derived_from is not null and profile_id = $1")
        .bind(env.profile("alex"))
        .fetch_one(&mut *c)
        .await?;
    let states: Vec<String> = sqlx::query_scalar(
        "select cr.state from candidate_row cr join extraction_run er on er.id = cr.extraction_run_id \
         where er.document_id = $1 order by cr.page_no",
    )
    .bind(copy)
    .fetch_all(&mut *c)
    .await?;
    assert_eq!(states.len(), 4, "{states:?}");
    assert!(
        states.iter().all(|s| s == "auto_accepted" || s == "in_review"),
        "{states:?}"
    );
    // the shared injection fixture: the clean attachment imports, the body never becomes a value
    let planted: i64 = sqlx::query_scalar(
        "select count(*) from candidate_row where cells::text like '%\"45\"%' or cells::text like '%CANARY%'",
    )
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(planted, 0);
    let held: i64 = sqlx::query_scalar("select count(*) from quarantine_item where status = 'held'")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(held, 1);
    Ok(())
}

#[tokio::test]
async fn quarantine_routes_list_release_and_delete() -> R {
    let env = env().await?;
    let source = mailbox(&env, 1).await?;
    let pdf = fixture("labslips/injection/inj-metadata.pdf")?;
    let raw = Mail::new("rel@clinica-norte.example", "citas@clinica-norte.example")
        .attach("informe.pdf", "application/pdf", pdf)
        .build();
    let q1 = hold(&env, source, &raw).await?;
    let q2 = hold(&env, source, &fixture("A-email/injection/inj-email-subject.eml")?).await?;

    let r = env.anon().get("/api/inboxes/quarantine").await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.get("/api/inboxes/quarantine").await?;
    assert_eq!(r.status, StatusCode::OK);
    let items = r.json.as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 2);
    let injected = items
        .iter()
        .find(|i| i["id"] == json!(q2))
        .ok_or_else(|| anyhow::anyhow!("q2 missing"))?;
    // free text from a stranger goes out only inside the untrusted envelope
    assert_eq!(injected["subject"]["trust"], "untrusted_text");
    assert_eq!(injected["subject"]["origin"], "email_subject");
    let flat = serde_json::to_string(&items)?;
    for item in &items {
        let mut v = item.clone();
        v["subject"] = Value::Null;
        if let Some(a) = v["attachments"].as_array_mut() {
            for f in a {
                assert!(f["filename"].is_null() || f["filename"]["trust"] == "untrusted_text");
                f["filename"] = Value::Null;
            }
        }
        assert!(!serde_json::to_string(&v)?.contains("CANARY"), "{v}");
    }
    assert!(flat.contains("CANARY-EMAIL-SUBJECT-3"));

    // releasing needs step-up and the right to manage the profile
    let path = format!("/api/inboxes/quarantine/{q1}/release");
    let mut stale = env.stale(env.user("alex")).await?;
    let r = stale.post(&path, &json!({"profile_id": env.profile("lucia")})).await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    let r = alex.post(&path, &json!({"profile_id": env.profile("carmen")})).await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    let r = alex
        .post(
            &path,
            &json!({"profile_id": env.profile("lucia"), "allow_sender": true}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["document_ids"].as_array().map(Vec::len), Some(1));
    let r = alex.post(&path, &json!({"profile_id": env.profile("lucia")})).await?;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let senders = alex.get(&format!("/api/inboxes/{source}/senders")).await?;
    assert!(senders.json.as_array().is_some_and(|a| {
        a.iter()
            .any(|s| s["pattern"] == "citas@clinica-norte.example" && s["profile_id"] == json!(env.profile("lucia")))
    }));
    let audits = env
        .scalar_i64("select count(*) from audit_event where action = 'quarantine.release_and_allow'")
        .await?;
    assert_eq!(audits, 1);

    // delete, and the lists by status
    let r = alex.delete(&format!("/api/inboxes/quarantine/{q2}")).await?;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(alex.get("/api/inboxes/quarantine").await?.json, json!([]));
    let all = alex.get("/api/inboxes/quarantine?status=all").await?.json;
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    let r = alex.delete(&format!("/api/inboxes/quarantine/{q2}")).await?;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = alex.get("/api/inboxes/quarantine?status=bogus").await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    Ok(())
}

#[tokio::test]
async fn sender_and_password_routes_never_return_a_password() -> R {
    let env = env().await?;
    let source = mailbox(&env, 1).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let senders = format!("/api/inboxes/{source}/senders");
    let r = alex
        .post(
            &senders,
            &json!({"pattern": "Citas@Clinica-Norte.example", "profile_id": env.profile("alex")}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    let rule = r.json["id"].as_str().unwrap_or_default().to_string();
    let r = alex
        .post(
            &senders,
            &json!({"pattern": "citas@clinica-norte.example", "profile_id": env.profile("alex")}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = alex
        .post(
            &senders,
            &json!({"pattern": "no-domain", "profile_id": env.profile("alex")}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut stale = env.stale(env.user("alex")).await?;
    let r = stale
        .post(
            &senders,
            &json!({"pattern": "x.example", "profile_id": env.profile("alex")}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let r = alex
        .post(
            &format!("/api/senders/{rule}/pdf-passwords"),
            &json!({"password": "12345678Z", "label": "DNI de Alex"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    let pw = r.json["id"].as_str().unwrap_or_default().to_string();
    let listed = alex.get(&senders).await?;
    let text = serde_json::to_string(&listed.json)?;
    assert!(text.contains("DNI de Alex") && !text.contains("12345678Z"), "{text}");

    // another household sees none of it
    let mut other = {
        let mut c = env.pool().acquire().await?;
        let fam = ownhealth_core::demo::create_rivera_family(&mut c, "Ortega").await?;
        drop(c);
        env.as_user(fam.user("alex")).await?
    };
    assert_eq!(other.get(&senders).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(
        other.delete(&format!("/api/pdf-passwords/{pw}")).await?.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other.delete(&format!("/api/senders/{rule}")).await?.status,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        alex.delete(&format!("/api/pdf-passwords/{pw}")).await?.status,
        StatusCode::OK
    );
    assert_eq!(
        alex.delete(&format!("/api/senders/{rule}")).await?.status,
        StatusCode::OK
    );
    assert_eq!(
        env.scalar_i64("select count(*) from secret where purpose = 'pdf_password'")
            .await?,
        0
    );
    assert_eq!(
        alex.get("/api/inboxes/not-a-uuid/senders").await?.status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}
