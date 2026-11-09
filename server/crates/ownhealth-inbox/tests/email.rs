//! Lane A: email-in against a real Postgres and an in-process IMAP server (plain, implicit TLS and
//! STARTTLS). Synthetic mail only (LabNorte, the Rivera family).

mod email_support;

use std::path::PathBuf;
use std::sync::Arc;

use email_support::fake_imap::{Acceptor, FakeImap, Io, Mode};
use email_support::mail::{LAB, Mail, TRUSTED, auth_pass, base64};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::config::Settings;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::jobs::JobContext;
use ownhealth_core::quarantine;
use ownhealth_core::secrets::{self, SecretBox, SecretString};
use ownhealth_core::sources::{self, NewSource, Source};
use ownhealth_core::testing::TestDb;
use ownhealth_inbox::email::{self, SenderRuleIn};
use serde_json::{Value, json};
use uuid::Uuid;

type R = anyhow::Result<()>;

const IMAP_USER: &str = "salud.rivera";
const IMAP_PASS: &str = "app-pass-7Kq2-synthetic";

struct Env {
    db: TestDb,
    _dir: tempfile::TempDir,
    ctx: JobContext,
    fam: Family,
    sb: SecretBox,
}

impl Env {
    async fn new() -> anyhow::Result<Env> {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let mut settings: Settings = db.settings(dir.path())?;
        settings.secrets_key = Some(base64(&[9u8; 32]));
        let sb = SecretBox::from_settings(&settings)?;
        let blobs = BlobStore::new(settings.blobs_path())?;
        let ctx = JobContext::new(db.pool.clone(), Arc::new(settings), blobs);
        let mut c = db.pool.acquire().await?;
        let fam = create_rivera_family(&mut c, "Rivera").await?;
        Ok(Env {
            db,
            _dir: dir,
            ctx,
            fam,
            sb,
        })
    }

    /// A mailbox with LabNorte allowed for Alex. `config` overrides the IMAP settings.
    async fn mailbox(&self, config: Value) -> anyhow::Result<Source> {
        let mut c = self.db.pool.acquire().await?;
        let h = self.fam.household_id;
        let secret = secrets::store_secret(
            &mut c,
            &self.sb,
            h,
            "imap_password",
            Some("buzón de salud"),
            &SecretString::new(IMAP_PASS),
        )
        .await?;
        let mut cfg = json!({"host": "127.0.0.1", "port": 143, "tls": "none", "username": IMAP_USER,
                                 "trusted_authserv_id": TRUSTED});
        if let (Some(base), Some(over)) = (cfg.as_object_mut(), config.as_object()) {
            base.extend(over.clone());
        }
        let id = sources::create_source(
            &mut c,
            &NewSource {
                config: cfg,
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
                profile_id: self.fam.profile("alex"),
                doc_type_hint: Some("lab_report".into()),
                trust_unauthenticated: false,
            },
            self.fam.user("alex"),
        )
        .await?;
        sources::get_source(&mut c, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no source"))
    }

    async fn n(&self, sql: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&self.db.pool)
            .await?)
    }

    async fn ingest(&self, source: &Source, raw: &[u8]) -> anyhow::Result<email::MessageOutcome> {
        let mut tx = self.db.pool.begin().await?;
        let out = email::ingest_message(&mut tx, &self.ctx.blobs, Some(&self.sb), source, raw, None).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Rows a parsed message would leave behind. All zero for quarantined mail.
    async fn parsed_rows(&self) -> anyhow::Result<[i64; 7]> {
        Ok([
            self.n("select count(*) from document").await?,
            self.n("select count(*) from document_source").await?,
            self.n("select count(*) from document_page").await?,
            self.n("select count(*) from candidate_row").await?,
            self.n("select count(*) from observation").await?,
            self.n("select count(*) from review_item").await?,
            self.n("select count(*) from job").await?,
        ])
    }
}

fn fixture(name: &str) -> anyhow::Result<Vec<u8>> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/A-email/injection")
        .join(name);
    Ok(std::fs::read(p)?)
}

fn slip() -> Vec<u8> {
    use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
    lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe de resultados de laboratorio",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "02/11/2026",
        report_id: "LN-20261102-AL1",
        pages: vec![vec![
            LabRow::new("Glucosa", "92", "mg/dL", "70 - 110"),
            LabRow::new("Colesterol total", "182", "mg/dL", "< 200"),
        ]],
    })
    .0
}

/// AES-256 (V5/R6), the user password a synthetic DNI.
fn lock(pdf: &[u8], password: &str) -> anyhow::Result<Vec<u8>> {
    use std::collections::BTreeMap;

    use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
    use lopdf::{Document, EncryptionState, EncryptionVersion, Permissions};
    let mut doc = Document::load_mem(pdf)?;
    let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let state = EncryptionState::try_from(EncryptionVersion::V5 {
        encrypt_metadata: true,
        crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
        file_encryption_key: &[3u8; 32],
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password: "owner-labnorte",
        user_password: password,
        permissions: Permissions::all(),
    })?;
    doc.encrypt(&state)?;
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

fn lab_mail(id: &str, pdf: Vec<u8>) -> Vec<u8> {
    Mail::new(id, LAB)
        .auth(auth_pass("labnorte.example"))
        .attach("informe.pdf", "application/pdf", pdf)
        .build()
}

// ------------------------------------------------------------- quarantine ----

/// Every message that is not from an allowlisted, authenticated sender, in every shape we could
/// think of: unknown senders, forged authentication, look-alike domains, two senders, none.
fn not_allowed() -> anyhow::Result<Vec<(Vec<u8>, &'static str)>> {
    let pdf = slip();
    let wrong_server = format!("{TRUSTED}.ofertas-salud.example; dkim=pass header.d=labnorte.example");
    let other_domain = format!("{TRUSTED}; dkim=pass header.d=ofertas-salud.example");
    let fail = format!("{TRUSTED}; dkim=fail header.d=labnorte.example; spf=fail smtp.mailfrom=labnorte.example");
    let mut v = vec![
        (fixture("inj-email-body-unknown-sender.eml")?, "sender_not_allowed"),
        (fixture("inj-email-forged-auth.eml")?, "auth_failed"),
        (fixture("inj-email-forged-auth-below.eml")?, "auth_failed"),
        (fixture("inj-email-subject.eml")?, "sender_not_allowed"),
        (
            Mail::new("q1@labnorte.example", LAB)
                .attach("a.pdf", "application/pdf", pdf.clone())
                .build(),
            "auth_failed",
        ),
        (
            Mail::new("q2@labnorte.example", LAB).auth(wrong_server).build(),
            "auth_failed",
        ),
        (
            Mail::new("q3@labnorte.example", LAB).auth(other_domain.clone()).build(),
            "auth_failed",
        ),
        (Mail::new("q4@labnorte.example", LAB).auth(fail).build(), "auth_failed"),
        (
            Mail::new("q5@ofertas-salud.example", "x@labnorte.example.ofertas-salud.example")
                .auth(other_domain.clone())
                .build(),
            "sender_not_allowed",
        ),
        (
            Mail::new("q6@ofertas-salud.example", "x@mail.labnorte.example").build(),
            "sender_not_allowed",
        ),
        (
            Mail::new("q7@ofertas-salud.example", "labnorte.example@ofertas-salud.example").build(),
            "sender_not_allowed",
        ),
        (
            Mail::new(
                "q8@labnorte.example",
                "resultados@labnorte.example, promo@ofertas-salud.example",
            )
            .auth(auth_pass("labnorte.example"))
            .build(),
            "sender_not_allowed",
        ),
        (
            b"Subject: sin remitente\r\nMessage-ID: <q9@x.example>\r\n\r\nhola\r\n".to_vec(),
            "sender_not_allowed",
        ),
    ];
    for i in 0..10 {
        let id = format!("q1{i}@ofertas-salud.example");
        let m = Mail::new(Box::leak(id.into_boxed_str()), "promo@ofertas-salud.example")
            .auth(other_domain.clone())
            .attach("informe.pdf", "application/pdf", pdf.clone())
            .build();
        v.push((m, "sender_not_allowed"));
    }
    Ok(v)
}

#[tokio::test]
async fn every_message_not_allowed_is_quarantined_and_never_parsed() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let cases = not_allowed()?;
    for (raw, want) in &cases {
        let out = env.ingest(&source, raw).await?;
        let qid = out
            .quarantine_id
            .ok_or_else(|| anyhow::anyhow!("not quarantined: {out:?}"))?;
        assert!(out.document_ids.is_empty());
        let mut c = env.db.pool.acquire().await?;
        let item = quarantine::get_item(&mut c, qid)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no item"))?;
        assert_eq!((item.reason.as_str(), item.status.as_str()), (*want, "held"));
    }
    assert_eq!(env.parsed_rows().await?, [0; 7], "quarantined mail left parsed rows");
    assert_eq!(env.n("select count(*) from quarantine_item").await?, cases.len() as i64);
    assert_eq!(
        env.n("select count(*) from source_item where quarantine_id is not null")
            .await?,
        cases.len() as i64
    );

    // the same message again is recognised by its Message-ID: no second row
    let again = env.ingest(&source, &cases[0].0).await?;
    assert!(again.already_seen && again.quarantine_id.is_none());
    assert_eq!(env.n("select count(*) from quarantine_item").await?, cases.len() as i64);

    // the subject that tried to break out of the list is kept without control characters
    let subject: String = sqlx::query_scalar(
        "select subject from quarantine_item where message_id = 'inj-subject-3@ofertas-salud.example'",
    )
    .fetch_one(&env.db.pool)
    .await?;
    assert!(subject.contains("CANARY-EMAIL-SUBJECT-3") && !subject.contains('\n') && !subject.contains('\r'));
    let attachments: Value = sqlx::query_scalar(
        "select attachments from quarantine_item where message_id = 'inj-body-unknown-1@ofertas-salud.example'",
    )
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(
        attachments,
        json!([{"filename": "informe.pdf", "mime": "application/pdf", "size": 15}])
    );
    Ok(())
}

#[tokio::test]
async fn allowed_mail_arrives_and_its_body_is_not_imported() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let out = env.ingest(&source, &fixture("inj-email-body-allowed.eml")?).await?;
    assert_eq!(out.quarantine_id, None);
    assert_eq!(out.document_ids.len(), 1, "{out:?}");
    let doc = out.document_ids[0];
    let (profile, channel, src, sref): (Uuid, String, Uuid, String) = sqlx::query_as(
        "select d.profile_id, s.channel, s.source_id, s.source_ref from document d \
         join document_source s on s.document_id = d.id where d.id = $1",
    )
    .bind(doc)
    .fetch_one(&env.db.pool)
    .await?;
    assert_eq!(
        (profile, channel.as_str(), src, sref.as_str()),
        (
            env.fam.profile("alex"),
            "email",
            source.id,
            "inj-body-allowed-4@labnorte.example"
        )
    );
    assert_eq!(env.n("select count(*) from job where kind = 'import.pdf'").await?, 1);
    // the body talked to an agent; it is in no row, only in the stored message
    for sql in [
        "select count(*) from document where filename like '%CANARY-EMAIL-BODY%' or flags::text like '%CANARY-EMAIL%'",
        "select count(*) from review_item where payload::text like '%CANARY-EMAIL%'",
        "select count(*) from job where payload::text like '%CANARY-EMAIL%'",
        "select count(*) from quarantine_item",
        "select count(*) from source_run where errors::text like '%CANARY-EMAIL%'",
    ] {
        assert_eq!(env.n(sql).await?, 0, "{sql}");
    }
    // a second copy of the same message changes nothing
    let again = env.ingest(&source, &fixture("inj-email-body-allowed.eml")?).await?;
    assert!(again.already_seen);
    assert_eq!(env.n("select count(*) from document").await?, 1);
    Ok(())
}

#[tokio::test]
async fn limits_skip_parts_without_naming_them() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let mut m = Mail::new("lim-1@labnorte.example", LAB).auth(auth_pass("labnorte.example"));
    m = m.attach("nota.txt", "text/plain", b"hola".to_vec());
    m = m.attach("informe.pdf", "application/pdf", slip());
    m = m.attach("mismo.pdf", "application/octet-stream", slip());
    let out = env.ingest(&source, &m.build()).await?;
    assert_eq!(out.document_ids.len(), 1, "{out:?}");
    assert_eq!(out.duplicates, 1);
    assert_eq!(out.skipped, vec!["attachment 1: unsupported_type".to_string()]);
    Ok(())
}

// ------------------------------------------------------------ PDF passwords ----

#[tokio::test]
async fn a_locked_pdf_is_unlocked_with_its_senders_password_only() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let mut c = env.db.pool.acquire().await?;
    let rule = email::list_rules(&mut c, source.id).await?[0].id;
    email::add_pdf_password(
        &mut c,
        &env.sb,
        rule,
        &SecretString::new("00000000T"),
        Some("DNI de Sam"),
    )
    .await?;
    email::add_pdf_password(
        &mut c,
        &env.sb,
        rule,
        &SecretString::new("12345678Z"),
        Some("DNI de Alex"),
    )
    .await?;
    drop(c);

    let out = env
        .ingest(&source, &lab_mail("pw-1@labnorte.example", lock(&slip(), "12345678Z")?))
        .await?;
    assert_eq!(out.document_ids.len(), 2, "{out:?}");
    let (original, copy) = (out.document_ids[0], out.document_ids[1]);
    let (derived, flags): (Option<Uuid>, Value) =
        sqlx::query_as("select derived_from, (select flags from document where id = $2) from document where id = $1")
            .bind(copy)
            .bind(original)
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(derived, Some(original));
    assert_eq!(flags["encrypted"], json!(true));
    // only the unlocked copy is imported, and no password is in a job, an audit row or a run
    let jobs: Vec<Value> = sqlx::query_scalar("select payload from job where kind = 'import.pdf'")
        .fetch_all(&env.db.pool)
        .await?;
    assert_eq!(
        jobs,
        vec![
            json!({"document_id": copy.to_string(), "profile_id": env.fam.profile("alex").to_string(),
                                 "household_id": env.fam.household_id.to_string()})
        ]
    );
    for sql in [
        "select count(*) from job where payload::text like '%12345678Z%'",
        "select count(*) from audit_event where coalesce(args_summary, '') like '%12345678Z%'",
        "select count(*) from secret where position('12345678Z' in encode(ciphertext, 'escape')) > 0",
        "select count(*) from secret where label like '%12345678Z%'",
    ] {
        assert_eq!(env.n(sql).await?, 0, "{sql}");
    }
    // the passwords of one sender are never tried on another sender's mail
    let mut c = env.db.pool.acquire().await?;
    email::add_rule(
        &mut c,
        source.id,
        &SenderRuleIn {
            pattern: "citas@clinica-norte.example".into(),
            profile_id: env.fam.profile("alex"),
            doc_type_hint: None,
            trust_unauthenticated: false,
        },
        env.fam.user("alex"),
    )
    .await?;
    drop(c);
    let other = Mail::new("pw-2@clinica-norte.example", "citas@clinica-norte.example")
        .auth(auth_pass("clinica-norte.example"))
        .attach("cita.pdf", "application/pdf", lock(&slip(), "12345678Z")?)
        .build();
    let out = env.ingest(&source, &other).await?;
    assert_eq!(
        out.skipped,
        vec![format!("attachment 1: {}", email::ENCRYPTED_UNKNOWN_PASSWORD)]
    );
    Ok(())
}

#[tokio::test]
async fn a_pdf_no_password_opens_goes_to_review_and_is_not_imported() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let out = env
        .ingest(&source, &lab_mail("pw-3@labnorte.example", lock(&slip(), "99999999R")?))
        .await?;
    assert_eq!(out.document_ids.len(), 1);
    let (kind, reasons, doc): (String, Vec<String>, Option<Uuid>) =
        sqlx::query_as("select kind, reasons, document_id from review_item")
            .fetch_one(&env.db.pool)
            .await?;
    assert_eq!(
        (kind.as_str(), reasons, doc),
        (
            "encrypted_document",
            vec![email::ENCRYPTED_UNKNOWN_PASSWORD.to_string()],
            Some(out.document_ids[0])
        )
    );
    assert_eq!(env.n("select count(*) from job").await?, 0);
    Ok(())
}

// ---------------------------------------------------------------- release ----

#[tokio::test]
async fn a_person_releases_a_held_message_once() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let raw = Mail::new("rel-1@clinica-norte.example", "Citas@Clinica-Norte.example")
        .attach("informe.pdf", "application/pdf", slip())
        .build();
    let qid = env
        .ingest(&source, &raw)
        .await?
        .quarantine_id
        .ok_or_else(|| anyhow::anyhow!("not held"))?;
    assert_eq!(env.parsed_rows().await?, [0; 7]);

    let carmen = env.fam.profile("carmen");
    let user = env.fam.user("alex");
    let mut tx = env.db.pool.begin().await?;
    let docs = email::release(&mut tx, &env.ctx.blobs, Some(&env.sb), qid, user, carmen, true)
        .await?
        .document_ids;
    tx.commit().await?;
    assert_eq!(docs.len(), 1);
    let profile: Uuid = sqlx::query_scalar("select profile_id from document where id = $1")
        .bind(docs[0])
        .fetch_one(&env.db.pool)
        .await?;
    assert_eq!(profile, carmen);
    let mut c = env.db.pool.acquire().await?;
    let item = quarantine::get_item(&mut c, qid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("gone"))?;
    assert_eq!((item.status.as_str(), item.decided_by), ("released", Some(user)));
    assert_eq!(item.released_document_ids, Some(docs.clone()));
    let rules = email::list_rules(&mut c, source.id).await?;
    assert!(
        rules
            .iter()
            .any(|r| r.pattern == "citas@clinica-norte.example" && r.profile_id == carmen)
    );

    // a second release is refused, and so is a profile of another household
    let again = email::release(&mut c, &env.ctx.blobs, Some(&env.sb), qid, user, carmen, false).await;
    assert!(matches!(again, Err(ownhealth_core::Error::Review(_))), "{again:?}");
    let raw2 = Mail::new("rel-2@ofertas-salud.example", "promo@ofertas-salud.example").build();
    drop(c);
    let q2 = env
        .ingest(&source, &raw2)
        .await?
        .quarantine_id
        .ok_or_else(|| anyhow::anyhow!("not held"))?;
    let mut c = env.db.pool.acquire().await?;
    let other = create_rivera_family(&mut c, "Ortega").await?;
    let r = email::release(
        &mut c,
        &env.ctx.blobs,
        Some(&env.sb),
        q2,
        user,
        other.profile("alex"),
        false,
    )
    .await;
    assert!(matches!(r, Err(ownhealth_core::Error::Invalid(_))), "{r:?}");
    quarantine::mark_deleted(&mut c, q2, user).await?;
    assert_eq!(env.n("select count(*) from document").await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_too_large_message_is_never_released_as_empty() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let header = b"Message-ID: <big-1@labnorte.example>\r\nFrom: resultados@labnorte.example\r\n\
Subject: Informe escaneado\r\n\r\n";
    let mut tx = env.db.pool.begin().await?;
    let qid = email::hold_too_large(&mut tx, &env.ctx.blobs, &source, header)
        .await?
        .quarantine_id
        .ok_or_else(|| anyhow::anyhow!("not held"))?;
    tx.commit().await?;
    let user = env.fam.user("alex");
    let mut c = env.db.pool.acquire().await?;
    let r = email::release(
        &mut c,
        &env.ctx.blobs,
        Some(&env.sb),
        qid,
        user,
        env.fam.profile("alex"),
        false,
    )
    .await;
    assert!(matches!(r, Err(ownhealth_core::Error::Review(_))), "{r:?}");
    let item = quarantine::get_item(&mut c, qid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("gone"))?;
    assert_eq!(item.status, "held");
    Ok(())
}

#[tokio::test]
async fn allowing_a_sender_needs_a_trusted_receiving_server() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({"trusted_authserv_id": null})).await?;
    let raw = Mail::new("allow-1@clinica-norte.example", "citas@clinica-norte.example")
        .attach("informe.pdf", "application/pdf", slip())
        .build();
    let qid = env
        .ingest(&source, &raw)
        .await?
        .quarantine_id
        .ok_or_else(|| anyhow::anyhow!("not held"))?;
    let (user, carmen) = (env.fam.user("alex"), env.fam.profile("carmen"));
    let mut c = env.db.pool.acquire().await?;
    // the rule it would add requires authentication nobody can give here: refused, nothing changed
    let r = email::release(&mut c, &env.ctx.blobs, Some(&env.sb), qid, user, carmen, true).await;
    assert!(matches!(r, Err(ownhealth_core::Error::Invalid(_))), "{r:?}");
    assert!(email::list_rules(&mut c, source.id).await?.len() == 1);
    // releasing just this one message still works
    let out = email::release(&mut c, &env.ctx.blobs, Some(&env.sb), qid, user, carmen, false).await?;
    assert_eq!(out.document_ids.len(), 1);
    Ok(())
}

#[tokio::test]
async fn rules_and_passwords_are_kept_per_mailbox() -> R {
    let env = Env::new().await?;
    let source = env.mailbox(json!({})).await?;
    let mut c = env.db.pool.acquire().await?;
    let dup = email::add_rule(
        &mut c,
        source.id,
        &SenderRuleIn {
            pattern: "LabNorte.example".into(),
            profile_id: env.fam.profile("lucia"),
            doc_type_hint: None,
            trust_unauthenticated: false,
        },
        env.fam.user("alex"),
    )
    .await;
    assert!(matches!(dup, Err(ownhealth_core::Error::Review(_))), "{dup:?}");
    let other = create_rivera_family(&mut c, "Ortega").await?;
    let foreign = email::add_rule(
        &mut c,
        source.id,
        &SenderRuleIn {
            pattern: "x.example".into(),
            profile_id: other.profile("alex"),
            doc_type_hint: None,
            trust_unauthenticated: false,
        },
        env.fam.user("alex"),
    )
    .await;
    assert!(matches!(foreign, Err(ownhealth_core::Error::Invalid(_))), "{foreign:?}");

    let rule = email::list_rules(&mut c, source.id).await?[0].id;
    let p1 = email::add_pdf_password(&mut c, &env.sb, rule, &SecretString::new("12345678Z"), Some("DNI")).await?;
    email::add_pdf_password(&mut c, &env.sb, rule, &SecretString::new("14051979"), None).await?;
    let listed = email::list_pdf_passwords(&mut c, rule).await?;
    assert_eq!(listed.iter().map(|p| p.position).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(listed[0].label.as_deref(), Some("DNI"));
    assert!(
        email::add_pdf_password(&mut c, &env.sb, rule, &SecretString::new(""), None)
            .await
            .is_err()
    );
    assert!(email::delete_pdf_password(&mut c, p1).await?);
    assert_eq!(
        env.n("select count(*) from secret where purpose = 'pdf_password'")
            .await?,
        1
    );
    assert!(email::delete_rule(&mut c, rule).await?);
    assert_eq!(
        env.n("select count(*) from secret where purpose = 'pdf_password'")
            .await?,
        0
    );
    assert_eq!(env.n("select count(*) from pdf_password_rule").await?, 0);
    Ok(())
}

// ------------------------------------------------------------------- IMAP ----

#[tokio::test]
async fn a_poll_ingests_and_moves_mail_to_the_done_folder() -> R {
    let env = Env::new().await?;
    for (has_move, has_uidplus) in [(true, true), (false, true), (false, false)] {
        let imap = FakeImap::start(IMAP_USER, IMAP_PASS, Mode::Plain).await?;
        imap.lock().has_move = has_move;
        imap.lock().has_uidplus = has_uidplus;
        let source = env.mailbox(json!({"port": imap.addr.port()})).await?;
        let id = |n| format!("poll-{has_move}-{has_uidplus}-{n}@labnorte.example");
        // another client flagged this one \Deleted and chose not to expunge it: never ours to remove
        let theirs = imap.deliver(Mail::new(&id(9), "amigo@correo-salud.example").build());
        if let Some(m) = imap
            .lock()
            .folders
            .get_mut("INBOX")
            .and_then(|f| f.iter_mut().find(|m| m.uid == theirs))
        {
            m.deleted = true;
        }
        imap.deliver(lab_mail(&id(1), slip()));
        imap.deliver(Mail::new("promo-poll@ofertas-salud.example", "promo@ofertas-salud.example").build());
        imap.deliver(lab_mail(&id(1), slip()));
        let run = email::poll_mailbox(env.ctx.clone(), source.clone()).await?;
        // the second mailbox gets the same report bytes: a duplicate of Alex's document
        let (new, dup) = if has_move { (1, 1) } else { (0, 2) };
        assert_eq!(
            (run.seen, run.new, run.quarantined, run.duplicate, run.problems.len()),
            (3, new, 1, dup, 0),
            "{run:?}"
        );
        // without UIDPLUS our copied messages wait, flagged, for the server's own expunge
        let ours_left = if has_move || has_uidplus { 0 } else { 3 };
        assert_eq!(
            (imap.count("INBOX"), imap.count(email::DEFAULT_DONE_FOLDER)),
            (1 + ours_left, 3)
        );
        assert!(imap.lock().folders["INBOX"].iter().any(|m| m.uid == theirs));
        assert!(imap.all_seen(email::DEFAULT_DONE_FOLDER));
        let run = email::poll_mailbox(env.ctx.clone(), source).await?;
        assert_eq!(run.seen, 0);
    }
    Ok(())
}

/// An allowed message whose only part is a photo embedded in the text (iPhone Mail sends a photo of a
/// paper report this way).
fn inline_photo_mail(id: &str) -> Vec<u8> {
    format!(
        "Authentication-Results: {}\r\nMessage-ID: <{id}>\r\nFrom: {LAB}\r\nTo: salud.rivera@correo-salud.example\r\n\
         Subject: Foto del informe\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"r1\"\r\n\r\n\
         --r1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<img src=\"cid:foto1\">\r\n\
         --r1\r\nContent-Type: image/jpeg; name=\"foto.jpg\"\r\nContent-Disposition: inline; filename=\"foto.jpg\"\r\n\
         Content-ID: <foto1>\r\nContent-Transfer-Encoding: base64\r\n\r\n/9j/4AAQSkZJRgABAQAAAQABAAD/2Q==\r\n--r1--\r\n",
        auth_pass("labnorte.example")
    )
    .into_bytes()
}

#[tokio::test]
async fn a_poll_reports_allowed_mail_that_imported_nothing() -> R {
    let env = Env::new().await?;
    let imap = FakeImap::start(IMAP_USER, IMAP_PASS, Mode::Plain).await?;
    let source = env.mailbox(json!({"port": imap.addr.port()})).await?;
    imap.deliver(inline_photo_mail("inline-1@labnorte.example"));
    imap.deliver(
        Mail::new("docx-1@labnorte.example", LAB)
            .auth(auth_pass("labnorte.example"))
            .attach(
                "informe.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                b"PK".to_vec(),
            )
            .build(),
    );
    let run = email::poll_mailbox(env.ctx.clone(), source).await?;
    assert_eq!((run.seen, run.new, run.duplicate), (2, 0, 0), "{run:?}");
    let codes: Vec<&str> = run.problems.iter().map(|p| p.code.as_str()).collect();
    assert_eq!(
        codes,
        [
            "no_importable_attachment",
            "attachment_skipped",
            "no_importable_attachment"
        ],
        "{run:?}"
    );
    assert!(
        run.problems
            .iter()
            .all(|p| !p.message.contains("informe") && !p.message.contains("foto"))
    );
    Ok(())
}

/// Set the mode of `dir` and every folder below it.
#[cfg(unix)]
fn chmod_tree(dir: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            chmod_tree(&e.path(), mode)?;
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
}

/// A poll where every message fails (the blob store cannot be written) is a failed sync through the
/// real job path: failed runs counted, no last success, and the mail stays in the inbox for the next
/// poll.
#[cfg(unix)]
#[tokio::test]
async fn a_poll_where_every_message_fails_is_not_a_successful_sync() -> R {
    let env = Env::new().await?;
    let imap = FakeImap::start(IMAP_USER, IMAP_PASS, Mode::Plain).await?;
    let source = env.mailbox(json!({"port": imap.addr.port()})).await?;
    imap.deliver(lab_mail("fail-1@labnorte.example", slip()));
    imap.deliver(lab_mail("fail-2@labnorte.example", slip()));
    let blobs = env.ctx.settings.blobs_path();
    chmod_tree(&blobs, 0o555)?;
    sources::register();
    email::register();
    let mut checks = Vec::new();
    for _ in 0..2 {
        let mut c = env.db.pool.acquire().await?;
        sources::enqueue_poll(&mut c, source.id).await?;
        ownhealth_core::jobs::drain(&env.ctx, "core", 10, None).await?;
        let s = sources::get_source(&mut c, source.id).await?.expect("source");
        checks.push((
            s.consecutive_failures,
            s.last_success_at.is_some(),
            s.last_error.is_some(),
        ));
    }
    chmod_tree(&blobs, 0o755)?;
    assert_eq!(checks, [(1, false, true), (2, false, true)]);
    assert_eq!(imap.count("INBOX"), 2, "failed mail is left for the next poll");
    assert_eq!(env.n("select count(*) from document").await?, 0);
    Ok(())
}

#[tokio::test]
async fn login_errors_name_the_server_answer_never_the_password() -> R {
    let env = Env::new().await?;
    let imap = FakeImap::start(IMAP_USER, "another-password", Mode::Plain).await?;
    let source = env.mailbox(json!({"port": imap.addr.port()})).await?;
    imap.deliver(lab_mail("never@labnorte.example", slip()));
    let err = email::poll_mailbox(env.ctx.clone(), source)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("logged in"))?;
    assert!(
        err.text().contains("AUTHENTICATIONFAILED") || err.text().contains("login"),
        "{}",
        err.text()
    );
    assert!(!err.text().contains(IMAP_PASS));
    let failed = imap.lock().failed_logins;
    assert_eq!((failed, imap.count("INBOX")), (1, 1));

    // plain IMAP to a public address is refused before connecting
    let public = env.mailbox(json!({"host": "203.0.113.7"})).await?;
    let err = email::poll_mailbox(env.ctx.clone(), public)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("connected"))?;
    assert!(err.text().contains("tls none"), "{}", err.text());
    Ok(())
}

/// A self-signed certificate for `localhost`, made with the openssl command line.
fn certificate(dir: &std::path::Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let ok = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "2",
        ])
        .args(["-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost"])
        .args([
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "extendedKeyUsage=serverAuth",
        ])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()?;
    anyhow::ensure!(ok.status.success(), "openssl: {}", String::from_utf8_lossy(&ok.stderr));
    Ok((cert, key))
}

fn acceptor(cert: &std::path::Path, key: &std::path::Path) -> anyhow::Result<Acceptor> {
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio_rustls::rustls::{self, ServerConfig};
    let certs = vec![CertificateDer::from_pem_file(cert)?];
    let key = PrivateKeyDer::from_pem_file(key)?;
    let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    Ok(Arc::new(move |tcp| {
        let tls = tls.clone();
        Box::pin(async move { Ok(Box::new(tls.accept(tcp).await?) as Box<dyn Io>) })
    }))
}

#[tokio::test]
async fn implicit_tls_and_starttls_verify_the_server() -> R {
    let env = Env::new().await?;
    let dir = tempfile::tempdir()?;
    let (cert, key) = certificate(dir.path())?;
    let acc = acceptor(&cert, &key)?;
    for (mode, tls) in [
        (Mode::Implicit(acc.clone()), "implicit"),
        (Mode::StartTls(acc.clone()), "starttls"),
    ] {
        let imap = FakeImap::start(IMAP_USER, IMAP_PASS, mode).await?;
        imap.deliver(lab_mail(&format!("tls-{tls}@labnorte.example"), slip()));
        let ca = cert.display().to_string();
        let trusted = env
            .mailbox(json!({"host": "localhost", "port": imap.addr.port(), "tls": tls, "ca_file": ca}))
            .await?;
        let run = email::poll_mailbox(env.ctx.clone(), trusted).await?;
        // the same report as the mailbox before: imported once, then a duplicate
        assert_eq!(
            (run.seen, run.new + run.duplicate, run.problems.len()),
            (1, 1, 0),
            "{tls}: {run:?}"
        );
        // nothing but STARTTLS went over the wire in clear
        let lines = imap.lock().plain_lines.clone();
        assert!(
            lines
                .iter()
                .all(|l| !l.contains(IMAP_PASS) && !l.to_uppercase().contains("LOGIN")),
            "{lines:?}"
        );

        // the public roots do not know this certificate: no login happens
        imap.deliver(lab_mail(&format!("tls2-{tls}@labnorte.example"), slip()));
        let untrusted = env
            .mailbox(json!({"host": "localhost", "port": imap.addr.port(), "tls": tls}))
            .await?;
        let err = email::poll_mailbox(env.ctx.clone(), untrusted)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("{tls}: accepted"))?;
        assert!(err.text().contains("tls handshake"), "{tls}: {}", err.text());
        assert_eq!(imap.lock().logins, 1);
    }
    Ok(())
}

#[tokio::test]
async fn the_demo_shows_one_quarantined_message() -> R {
    let env = Env::new().await?;
    email::register();
    let mut c = env.db.pool.acquire().await?;
    let mut ctx = ownhealth_core::demo::DemoContext {
        settings: (*env.ctx.settings).clone(),
        blobs: env.ctx.blobs.clone(),
        family: env.fam.clone(),
        as_of: chrono::NaiveDate::from_ymd_opt(2026, 11, 2).ok_or_else(|| anyhow::anyhow!("date"))?,
        documents: Default::default(),
        src_ids: Default::default(),
    };
    ownhealth_core::demo::seed_into(&mut c, &mut ctx).await?;
    assert_eq!(
        env.n("select count(*) from quarantine_item where status = 'held'")
            .await?,
        1
    );
    assert_eq!(
        env.n("select count(*) from source where kind = 'email' and not enabled")
            .await?,
        1
    );
    assert_eq!(env.n("select count(*) from sender_rule").await?, 1);
    Ok(())
}

// ---------------------------------------------------- committed fixtures ----

/// The locked LabNorte report the API exit-criterion test reads (`fixtures/A-email/`). The API crate
/// has no PDF writer, so the file is committed; this test keeps it opening with its password, and
/// `cargo test -p ownhealth-inbox --test email -- --ignored write_locked_fixture` rewrites it.
const LOCKED_FIXTURE: &str = "../../../fixtures/A-email/labnorte-locked-12345678Z.pdf";

fn locked_report() -> anyhow::Result<Vec<u8>> {
    use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
    let pdf = lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe de resultados de laboratorio",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "02/11/2026",
        report_id: "LN-20261102-AL7",
        pages: vec![vec![
            LabRow::new("Glucosa", "94", "mg/dL", "70 - 110"),
            LabRow::new("Creatinina", "0,93", "mg/dL", "0,70 - 1,30"),
            LabRow::new("Colesterol total", "188", "mg/dL", "< 200"),
            LabRow::new("Triglicéridos", "142", "mg/dL", "< 150"),
        ]],
    })
    .0;
    lock(&pdf, "12345678Z")
}

#[test]
#[ignore = "rewrites the committed fixture"]
fn write_locked_fixture() -> R {
    std::fs::write(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(LOCKED_FIXTURE),
        locked_report()?,
    )?;
    Ok(())
}

#[test]
fn the_committed_locked_fixture_opens_only_with_its_password() -> R {
    use ownhealth_inbox::email::pdf_password::{Unlock, is_encrypted, unlock};
    let pdf = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(LOCKED_FIXTURE))?;
    assert!(is_encrypted(&pdf));
    assert_eq!(unlock(&pdf, &[SecretString::new("1234")])?, Unlock::NoPasswordWorked);
    assert!(matches!(
        unlock(&pdf, &[SecretString::new("12345678Z")])?,
        Unlock::Unlocked { index: 0, .. }
    ));
    Ok(())
}
