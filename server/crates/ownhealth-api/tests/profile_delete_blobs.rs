//! Deleting a profile removes every file and stored credential it owns: genome files (gVCF, chip
//! sites and their indexes), OCR word layers, raw device pushes, mail held on its own mailbox, and the
//! sealed passwords and tokens of its inboxes. A file another profile still names stays, and so does
//! a household mailbox with its password.

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use ownhealth_auth::household::UNLINK_JOB;
use ownhealth_core::Runtime;
use ownhealth_core::blobs::sha256_hex_to_bytes;
use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::{JobContext, drain};
use ownhealth_core::provenance::{SrcIn, create_src};
use ownhealth_core::secrets::{SecretBox, SecretString, store_secret};
use ownhealth_core::sources::{self, NewSource};
use ownhealth_inbox::folder::poll_root;
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// 32 bytes of 0x0b, base64. Test-only key.
const KEY: &str = "CwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCws=";

/// Every file in the blob store, by its sha256 hex name.
fn stored(root: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, out: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                out.insert(name.to_string());
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&root.join("sha256"), &mut out);
    out
}

/// A new inbox through the REST route; returns its id.
async fn add(me: &mut support::Client, body: Value) -> anyhow::Result<String> {
    let r = me.post("/api/inboxes", &body).await?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Ok(r.json["id"].as_str().unwrap_or_default().to_string())
}

/// One genome import on `profile` with the given blob columns set.
async fn genome(
    conn: &mut PgConnection,
    profile: Uuid,
    user: Uuid,
    assay: &str,
    blobs: &[(&str, &str)],
) -> anyhow::Result<()> {
    let src = create_src(
        &mut *conn,
        &SrcIn {
            profile_id: Some(profile),
            ..SrcIn::manual(Some(user))
        },
    )
    .await?;
    let mut cols = vec!["profile_id", "sample_id", "assay", "facts_schema", "facts_sha256"];
    cols.extend(["owngenome_version", "db_versions", "src_id"]);
    cols.extend(blobs.iter().map(|(c, _)| *c));
    let marks: Vec<String> = (1..=cols.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        "insert into genome_dataset ({}) values ({})",
        cols.join(", "),
        marks.join(", ")
    );
    let facts = blobs
        .iter()
        .find(|(c, _)| *c == "facts_blob")
        .map(|(_, s)| sha256_hex_to_bytes(s))
        .transpose()?
        .unwrap_or_default();
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(profile)
        .bind("RIVERA-SYN-01")
        .bind(assay)
        .bind("ownhealth.genome-facts/v1")
        .bind(facts)
        .bind("0.9.0")
        .bind(json!({}))
        .bind(src);
    for (_, sha) in blobs {
        q = q.bind(sha256_hex_to_bytes(sha)?);
    }
    q.execute(&mut *conn).await?;
    Ok(())
}

#[tokio::test]
async fn deleting_a_profile_leaves_none_of_its_files_or_credentials() -> R {
    let mut env = Env::new(true).await?;
    env.settings.secrets_key = Some(KEY.into());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    let sb = SecretBox::from_base64(KEY)?;
    let (alex, sam, user, household) = (
        env.profile("alex"),
        env.profile("sam"),
        env.user("alex"),
        env.fam().household_id,
    );
    let put = |bytes: &str| env.rt.blobs.put_bytes(bytes.as_bytes());
    let mut c = env.pool().acquire().await?;

    // a WGS import with its gVCF and index; Sam's import holds the very same gVCF file
    let gvcf = put("##fileformat=VCFv4.2 synthetic gVCF shared by two imports")?;
    genome(
        &mut c,
        alex,
        user,
        "wgs",
        &[
            ("facts_blob", &put("{\"facts\": \"alex wgs\"}")?),
            ("gvcf_blob", &gvcf),
            ("gvcf_tbi_blob", &put("synthetic tbi of alex's gVCF")?),
        ],
    )
    .await?;
    genome(
        &mut c,
        sam,
        env.user("sam"),
        "wgs",
        &[("facts_blob", &put("{\"facts\": \"sam wgs\"}")?), ("gvcf_blob", &gvcf)],
    )
    .await?;
    // a chip import with the file of every probed site and its index
    genome(
        &mut c,
        alex,
        user,
        "chip",
        &[
            ("facts_blob", &put("{\"facts\": \"alex chip\"}")?),
            ("sites_blob", &put("synthetic chip sites")?),
            ("sites_tbi_blob", &put("synthetic chip sites index")?),
        ],
    )
    .await?;

    // a scanned LabNorte page and the word layer our OCR read from it
    let scan = put("%PDF-1.7 synthetic LabNorte scan")?;
    let doc = create_document(
        &mut c,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            ..NewDocument::upload(household, Some(alex), &scan, "application/pdf", 32)
        },
    )
    .await?;
    sqlx::query(
        "insert into ocr_run (document_id, page_no, engine, engine_version, models_sha256, status, words_blob) \
         values ($1, 1, 'paddleocr-onnx', 'test', 'test', 'done', $2)",
    )
    .bind(doc.id)
    .bind(sha256_hex_to_bytes(&put("[{\"t\": \"Colesterol\"}]")?)?)
    .execute(&mut *c)
    .await?;
    drop(c);

    let mut me = env.as_user(user).await?;
    let mailbox = |profile: Option<Uuid>, user: &str| {
        json!({"kind": "email", "label": format!("Correo {user}"), "profile_id": profile,
               "secret": "imap-pass-not-real", "config": {"host": "imap.rivera.example", "port": 993,
               "tls": "implicit", "username": user}})
    };
    // Alex's own mailbox, a household mailbox (both with an IMAP password), Alex's Nightscout with
    // its token, and Alex's Health Auto Export inbox
    let own_mail = add(&mut me, mailbox(Some(alex), "alex.rivera")).await?;
    let home_mail = add(&mut me, mailbox(None, "salud.rivera")).await?;
    add(
        &mut me,
        json!({"kind": "nightscout", "label": "Sensor de Alex", "profile_id": alex,
               "secret": "ns-token-not-real", "config": {"base_url": "https://ns.rivera.example"}}),
    )
    .await?;
    let hae = add(
        &mut me,
        json!({"kind": "health_auto_export", "label": "iPhone de Alex", "profile_id": alex}),
    )
    .await?;
    let token = me.post0(&format!("/api/inboxes/{hae}/token")).await?;
    assert_eq!(token.status, StatusCode::OK, "{}", token.json);
    let bearer = format!("Bearer {}", token.json["token"].as_str().unwrap_or_default());
    let r = env
        .anon()
        .send(
            Method::POST,
            "/api/ingest/health-auto-export",
            Some(&json!({"data": {"metrics": []}})),
            &[("authorization", &bearer)],
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    let mut c = env.pool().acquire().await?;
    // mail held on each mailbox; a sender rule on the household mailbox that files to Alex, with the
    // PDF password it tries
    for (source, eml) in [
        (&own_mail, "From: x@unknown.example\r\n\r\nheld for Alex"),
        (&home_mail, "From: y@unknown.example\r\n\r\nheld for the household"),
    ] {
        sqlx::query(
            "insert into quarantine_item (household_id, source_id, raw_blob, reason) \
             values ($1, $2::uuid, $3, 'sender_not_allowed')",
        )
        .bind(household)
        .bind(source)
        .bind(sha256_hex_to_bytes(&put(eml)?)?)
        .execute(&mut *c)
        .await?;
    }
    let pdf_pass = store_secret(
        &mut c,
        &sb,
        household,
        "pdf_password",
        Some("DNI de Alex"),
        &SecretString::new("pdf-pass-not-real"),
    )
    .await?;
    let rule: Uuid = sqlx::query_scalar(
        "insert into sender_rule (source_id, pattern, profile_id) values ($1::uuid, 'labnorte.example', $2) \
         returning id",
    )
    .bind(&home_mail)
    .bind(alex)
    .fetch_one(&mut *c)
    .await?;
    sqlx::query("insert into pdf_password_rule (sender_rule_id, secret_id) values ($1, $2)")
        .bind(rule)
        .bind(pdf_pass)
        .execute(&mut *c)
        .await?;
    // a sender rule on Alex's own mailbox that files to Sam, with its PDF password: the rule goes with
    // Alex's mailbox, so its password goes too
    let sams_pass = store_secret(
        &mut c,
        &sb,
        household,
        "pdf_password",
        Some("DNI de Sam"),
        &SecretString::new("pdf-pass-sam-not-real"),
    )
    .await?;
    let rule: Uuid = sqlx::query_scalar(
        "insert into sender_rule (source_id, pattern, profile_id) values ($1::uuid, 'clinica.example', $2) \
         returning id",
    )
    .bind(&own_mail)
    .bind(sam)
    .fetch_one(&mut *c)
    .await?;
    sqlx::query("insert into pdf_password_rule (sender_rule_id, secret_id) values ($1, $2)")
        .bind(rule)
        .bind(sams_pass)
        .execute(&mut *c)
        .await?;
    // Sam's Health Auto Export inbox received the very same push body: one file for both inboxes
    let sams_hae: Uuid = sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label) \
         values ($1, $2, 'health_auto_export', 'iPhone de Sam') returning id",
    )
    .bind(household)
    .bind(sam)
    .fetch_one(&mut *c)
    .await?;
    let alex_push: String =
        sqlx::query_scalar("select encode(blob_sha256, 'hex') from source_item where source_id = $1::uuid")
            .bind(&hae)
            .fetch_one(&mut *c)
            .await?;
    sqlx::query("insert into source_item (source_id, external_id, blob_sha256) values ($1, $2, $3)")
        .bind(sams_hae)
        .bind(format!("push:{alex_push}"))
        .bind(sha256_hex_to_bytes(&alex_push)?)
        .execute(&mut *c)
        .await?;
    drop(c);

    let home_secret: Uuid = sqlx::query_scalar("select secret_id from source where id = $1::uuid")
        .bind(&home_mail)
        .fetch_one(env.pool())
        .await?;
    let home_eml = env
        .rt
        .blobs
        .put_bytes(b"From: y@unknown.example\r\n\r\nheld for the household")?;
    let before = stored(env.rt.blobs.root());
    assert_eq!(before.len(), 12, "{before:?}");
    assert_eq!(env.scalar_i64("select count(*) from secret").await?, 5);

    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    // what stays: the gVCF Sam's import names, Sam's facts, Sam's push, and the household mailbox's mail
    let after = stored(env.rt.blobs.root());
    let sams_facts: String =
        sqlx::query_scalar("select encode(facts_blob, 'hex') from genome_dataset where profile_id = $1")
            .bind(sam)
            .fetch_one(env.pool())
            .await?;
    assert_eq!(after, BTreeSet::from([gvcf, sams_facts, alex_push, home_eml]));
    assert_eq!(r.json["blobs"], json!(before.len() - 4), "{}", r.json);
    // what stays of the credentials: the household mailbox's password, nothing of Alex's
    let left: Vec<Uuid> = sqlx::query_scalar("select id from secret")
        .fetch_all(env.pool())
        .await?;
    assert_eq!(left, vec![home_secret]);
    assert_eq!(r.json["rows"]["secret"], json!(4), "{}", r.json);
    assert_eq!(r.json["rows"]["quarantine_item"], json!(1), "{}", r.json);
    assert_eq!(
        env.scalar_i64(&format!(
            "select count(*) from quarantine_item where source_id = '{home_mail}'"
        ))
        .await?,
        1
    );
    // every file went at once, so no retry waits
    assert_eq!(r.json["blobs_pending"], json!(0), "{}", r.json);
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind in ('blobs.unlink', 'folder.forget')")
            .await?,
        0
    );
    Ok(())
}

/// A household inbox (no profile of its own) that routes each item by a rule.
async fn household_source(env: &Env, kind: &str, config: Value) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "insert into source (household_id, kind, label, config, created_by) \
         values ($1, $2, 'Bandeja de casa', $3, $4) returning id",
    )
    .bind(env.fam().household_id)
    .bind(kind)
    .bind(config)
    .bind(env.user("alex"))
    .fetch_one(env.pool())
    .await?)
}

/// A document of `profile` holding `bytes`; returns its id and file hash.
async fn document(env: &Env, profile: Uuid, bytes: &str, mime: &str) -> anyhow::Result<(Uuid, String)> {
    let sha = env.rt.blobs.put_bytes(bytes.as_bytes())?;
    let mut c = env.pool().acquire().await?;
    let doc = create_document(
        &mut c,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            ..NewDocument::upload(
                env.fam().household_id,
                Some(profile),
                &sha,
                mime,
                i64::try_from(bytes.len())?,
            )
        },
    )
    .await?;
    Ok((doc.id, sha))
}

/// What an inbox remembers of one item it handed us.
async fn item(
    env: &Env,
    source: Uuid,
    external_id: &str,
    sha: &str,
    document: Option<Uuid>,
    quarantine: Option<Uuid>,
) -> anyhow::Result<()> {
    sqlx::query(
        "insert into source_item (source_id, external_id, blob_sha256, document_id, quarantine_id) \
         values ($1, $2, $3, $4, $5)",
    )
    .bind(source)
    .bind(external_id)
    .bind(sha256_hex_to_bytes(sha)?)
    .bind(document)
    .bind(quarantine)
    .execute(env.pool())
    .await?;
    Ok(())
}

/// An inbox's items as (external id, file hash).
async fn items(env: &Env, source: Uuid) -> anyhow::Result<Vec<(String, Option<String>)>> {
    Ok(sqlx::query_as(
        "select external_id, encode(blob_sha256, 'hex') from source_item where source_id = $1 order by external_id",
    )
    .bind(source)
    .fetch_all(env.pool())
    .await?)
}

#[tokio::test]
async fn a_file_that_reached_the_person_through_a_household_inbox_goes_too() -> R {
    let env = Env::new(true).await?;
    let (alex, sam) = (env.profile("alex"), env.profile("sam"));
    let paperless = household_source(
        &env,
        "paperless",
        json!({"base_url": "https://paperless.rivera.example", "health_tag": 3,
               "profile_by_tag": {"9": alex, "4": sam}}),
    )
    .await?;
    let folder = household_source(&env, "folder", json!({"subdir": "estudio"})).await?;
    // Paperless documents 7 (tagged for Alex) and 8 (for Sam); 10 is a FHIR bundle for Alex, whose
    // document the structured importer made, so its item names no document
    let (doc7, pdf7) = document(
        &env,
        alex,
        "%PDF-1.7 synthetic LabNorte report 7 for Alex",
        "application/pdf",
    )
    .await?;
    let (doc8, pdf8) = document(
        &env,
        sam,
        "%PDF-1.7 synthetic LabNorte report 8 for Sam",
        "application/pdf",
    )
    .await?;
    let (_, bundle) = document(
        &env,
        alex,
        "{\"resourceType\": \"Bundle\", \"id\": \"rivera-alex\"}",
        "application/fhir+json",
    )
    .await?;
    item(&env, paperless, "7", &pdf7, Some(doc7), None).await?;
    item(&env, paperless, "8", &pdf8, Some(doc8), None).await?;
    item(&env, paperless, "10", &bundle, None, None).await?;
    // a scan Alex dropped in the scanner folder, and a file of a kind nobody imports (no document)
    let (scan, scan_sha) = document(
        &env,
        alex,
        "%PDF-1.7 synthetic scan of a LabNorte page",
        "application/pdf",
    )
    .await?;
    let odt = env
        .rt
        .blobs
        .put_bytes(b"synthetic notes Alex dropped in the scanner folder")?;
    item(
        &env,
        folder,
        "alex/escaneo-labnorte.pdf:44:1",
        &scan_sha,
        Some(scan),
        None,
    )
    .await?;
    item(&env, folder, "alex/notas.odt:48:1", &odt, None, None).await?;
    // another household's scanner folder has a person with the same short name; that item is theirs
    let mut c = env.pool().acquire().await?;
    let ortega = ownhealth_core::access::create_household(&mut c, "Familia Ortega").await?;
    drop(c);
    let their_folder: Uuid = sqlx::query_scalar(
        "insert into source (household_id, kind, label, config) \
         values ($1, 'folder', 'Escáner Ortega', '{\"subdir\": \"ortega\"}') returning id",
    )
    .bind(ortega)
    .fetch_one(env.pool())
    .await?;
    let theirs = env
        .rt
        .blobs
        .put_bytes(b"synthetic notes of the other household's alex")?;
    item(&env, their_folder, "alex/notas.odt:51:1", &theirs, None, None).await?;

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    let left = stored(env.rt.blobs.root());
    for sha in [&pdf7, &bundle, &scan_sha, &odt] {
        assert!(!left.contains(sha), "a file of Alex is still on disk: {sha} {}", r.json);
    }
    assert!(left.contains(&pdf8), "Sam's Paperless document went too");
    // Paperless keeps its ids 7 and 10, so its next poll does not import them into another profile;
    // the bytes are gone
    assert_eq!(
        items(&env, paperless).await?,
        vec![("10".into(), None), ("7".into(), None), ("8".into(), Some(pdf8))]
    );
    // the scanner folder's items named Alex's short name and file names; the files are gone, so the
    // items go too
    assert_eq!(items(&env, folder).await?, vec![]);
    assert_eq!(
        items(&env, their_folder).await?,
        vec![("alex/notas.odt:51:1".into(), Some(theirs.clone()))],
        "the other household's item went with Alex"
    );
    assert!(left.contains(&theirs), "the other household's file went with Alex");
    Ok(())
}

#[tokio::test]
async fn a_paperless_tag_rule_stops_naming_the_person() -> R {
    let env = Env::new(true).await?;
    let (alex, sam) = (env.profile("alex"), env.profile("sam"));
    let paperless = household_source(
        &env,
        "paperless",
        // the settings route accepts any form Uuid::parse_str reads, so a rule can name Alex without
        // hyphens, in braces or as a URN; a value that is no id at all must not stop the delete
        json!({"base_url": "https://paperless.rivera.example", "health_tag": 3,
               "profile_by_tag": {"9": alex, "4": sam, "5": alex.simple().to_string(),
                                  "6": alex.braced().to_string(), "7": alex.urn().to_string(),
                                  "8": "no-es-un-id"}}),
    )
    .await?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    // the tag rules that filed to Alex go, Sam's stays, and no inbox setting names Alex any more
    let config: Value = sqlx::query_scalar("select config from source where id = $1")
        .bind(paperless)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(
        config["profile_by_tag"],
        json!({"4": sam, "8": "no-es-un-id"}),
        "{config}"
    );
    assert_eq!(
        config["base_url"],
        json!("https://paperless.rivera.example"),
        "{config}"
    );
    assert_eq!(
        env.scalar_i64(&format!(
            "select count(*) from source where config::text like '%{alex}%'"
        ))
        .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn mail_a_household_mailbox_filed_to_the_person_goes_too() -> R {
    let env = Env::new(true).await?;
    let (alex, household) = (env.profile("alex"), env.fam().household_id);
    let mailbox = household_source(&env, "email", json!({})).await?;
    let put = |bytes: &str| env.rt.blobs.put_bytes(bytes.as_bytes());
    let hold = |eml: String, status: &'static str, released: Option<Uuid>| {
        sqlx::query_scalar::<_, Uuid>(
            "insert into quarantine_item (household_id, source_id, raw_blob, reason, status, released_document_ids) \
             values ($1, $2, $3, 'sender_not_allowed', $4, case when $5::uuid is null then null else array[$5::uuid] end) \
             returning id",
        )
        .bind(household)
        .bind(mailbox)
        .bind(sha256_hex_to_bytes(&eml).unwrap_or_default())
        .bind(status)
        .bind(released)
        .fetch_one(env.pool())
    };
    // a sender rule filed this message to Alex: the whole .eml, with the report inside, is stored
    let (doc, _) = document(
        &env,
        alex,
        "%PDF-1.7 synthetic LabNorte report by mail",
        "application/pdf",
    )
    .await?;
    let eml = put("From: resultados@labnorte.example\r\n\r\n(base64 of Alex's LabNorte report)")?;
    item(&env, mailbox, "<m1@labnorte.example>", &eml, Some(doc), None).await?;
    // a message held because the sender was new, then released into Alex's profile
    let (released, _) = document(
        &env,
        alex,
        "%PDF-1.7 synthetic report from a new clinic",
        "application/pdf",
    )
    .await?;
    let held_eml = put("From: nueva@clinica.example\r\n\r\n(base64 of Alex's report, released)")?;
    let q = hold(held_eml.clone(), "released", Some(released)).await?;
    item(&env, mailbox, "<m2@clinica.example>", &held_eml, None, Some(q)).await?;
    // a message still held for the household stays
    let other = put("From: y@unknown.example\r\n\r\nheld for the household")?;
    let q3 = hold(other.clone(), "held", None).await?;
    item(&env, mailbox, "<m3@unknown.example>", &other, None, Some(q3)).await?;

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    let left = stored(env.rt.blobs.root());
    assert!(
        !left.contains(&eml),
        "the raw mail holding Alex's report is still on disk: {}",
        r.json
    );
    assert!(
        !left.contains(&held_eml),
        "the released mail is still on disk: {}",
        r.json
    );
    assert!(left.contains(&other), "the household's held mail went too");
    // the mailbox still knows both messages, so the next poll does not import them again
    assert_eq!(
        items(&env, mailbox).await?,
        vec![
            ("<m1@labnorte.example>".into(), None),
            ("<m2@clinica.example>".into(), None),
            ("<m3@unknown.example>".into(), Some(other)),
        ]
    );
    let quarantined: Vec<Uuid> = sqlx::query_scalar("select id from quarantine_item")
        .fetch_all(env.pool())
        .await?;
    assert_eq!(quarantined, vec![q3]);
    assert_eq!(r.json["rows"]["quarantine_item"], json!(1), "{}", r.json);
    Ok(())
}

/// A folder inbox Alex set up on `subdir` of the inbox folder.
async fn folder_source(env: &Env, subdir: &str, profile: Option<Uuid>) -> anyhow::Result<sources::Source> {
    let mut c = env.pool().acquire().await?;
    let id = sources::create_source(
        &mut c,
        &NewSource {
            profile_id: profile,
            config: json!({"subdir": subdir}),
            created_by: Some(env.user("alex")),
            ..NewSource::new(env.fam().household_id, "folder", subdir)
        },
    )
    .await?;
    Ok(sources::get_source(&mut c, id).await?.expect("source"))
}

#[tokio::test]
async fn the_scanner_folder_keeps_no_copy_of_the_persons_files() -> R {
    use std::os::unix::fs::symlink;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    let (alex, user) = (env.profile("alex"), env.user("alex"));
    // a household scanner folder, and Alex's own one whose root files are Alex's
    let shared = folder_source(&env, "estudio", None).await?;
    let own = folder_source(&env, "alex-movil", Some(alex)).await?;
    let (root, own_root) = (inbox.join("estudio"), inbox.join("alex-movil"));
    let report = |id: &str| {
        lab_report_pdf(&LabReport {
            lab: "LabNorte",
            title: "Informe",
            patient: "RIVERA, ALEX",
            birth: "01/02/1984",
            collected: "14/09/2026",
            report_id: id,
            pages: vec![vec![LabRow::new("Glucosa", "88", "mg/dL", "70 - 110")]],
        })
        .0
    };
    for (dir, id) in [
        (root.join("alex"), "LN-1"),
        (root.join("sam"), "LN-2"),
        (own_root.clone(), "LN-3"),
    ] {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{id}.pdf")), report(id))?;
    }
    for (source, dir) in [(&shared, &root), (&own, &own_root)] {
        poll_root(env.pool(), &env.rt.blobs, source, dir, Duration::ZERO).await?;
    }
    assert!(root.join(".done/alex/LN-1.pdf").is_file());
    assert!(own_root.join(".done/_root/LN-3.pdf").is_file());
    // Alex only views Sam's profile: Sam's drop stays for Sam's own inbox
    assert!(root.join("sam/LN-2.pdf").is_file());
    // a copy that failed earlier, a new drop not read yet, and links someone left where Alex's
    // files go: the links go, never what they point at
    let outside = env.dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside)?;
    std::fs::write(outside.join("keep.txt"), "not Alex's")?;
    std::fs::create_dir_all(root.join(".failed"))?;
    symlink(&outside, root.join(".failed/alex"))?;
    std::fs::write(root.join("alex/nuevo.pdf"), report("LN-4"))?;
    symlink(outside.join("keep.txt"), root.join("alex/enlace.txt"))?;
    // a parent folder that is a link: `.failed` of Alex's own inbox points outside the inbox, at a folder
    // that happens to hold a `_root` of its own; nothing is deleted through it
    let parent = env.dir.path().join("elsewhere-parent");
    std::fs::create_dir_all(parent.join("_root"))?;
    std::fs::write(parent.join("_root/keep.pdf"), b"%PDF-1.7 not in the inbox")?;
    symlink(&parent, own_root.join(".failed"))?;
    let docs: Vec<String> = sqlx::query_scalar("select encode(blob_sha256, 'hex') from document where profile_id = $1")
        .bind(alex)
        .fetch_all(env.pool())
        .await?;
    assert_eq!(docs.len(), 2);

    let mut me = env.as_user(user).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    let gone = |p: &Path| std::fs::symlink_metadata(p).is_err();
    for p in [
        root.join("alex"),
        root.join(".done/alex"),
        root.join(".failed/alex"),
        own_root.join(".done/_root"),
    ] {
        assert!(gone(&p), "{} is still there", p.display());
    }
    assert!(outside.join("keep.txt").is_file(), "a link was followed");
    assert!(
        parent.join("_root/keep.pdf").is_file(),
        "the delete went through a linked parent folder"
    );
    assert!(own_root.join(".failed").is_symlink(), "the linked parent folder went");
    assert!(root.join("sam/LN-2.pdf").is_file(), "Sam's drop went too");
    let left = stored(env.rt.blobs.root());
    assert!(docs.iter().all(|d| !left.contains(d)), "{left:?}");
    Ok(())
}

/// Alex's report whose file the disk will not let go yet (a folder sits where the file should be).
async fn stuck_report(env: &Env) -> anyhow::Result<(String, std::path::PathBuf)> {
    let (_, stuck) = document(
        env,
        env.profile("alex"),
        "%PDF-1.7 synthetic report the disk would not let go",
        "application/pdf",
    )
    .await?;
    let path = env.rt.blobs.path(&stuck)?;
    std::fs::remove_file(&path)?;
    std::fs::create_dir_all(path.join("busy"))?;
    Ok((stuck, path))
}

/// The unlink job's state, attempts, and whether it waits for a later run.
async fn unlink_state(env: &Env) -> anyhow::Result<(String, i32, bool)> {
    Ok(
        sqlx::query_as("select state, attempts, run_after > now() from job where kind = $1")
            .bind(UNLINK_JOB)
            .fetch_one(env.pool())
            .await?,
    )
}

/// Run the unlink job now.
async fn run_unlink(env: &Env) -> anyhow::Result<usize> {
    sqlx::query("update job set run_after = now() where kind = $1 and state = 'queued'")
        .bind(UNLINK_JOB)
        .execute(env.pool())
        .await?;
    let ctx = JobContext::new(env.pool().clone(), Arc::new(env.settings.clone()), env.rt.blobs.clone());
    Ok(drain(&ctx, "core", 5, Some(&[UNLINK_JOB.to_string()])).await?)
}

#[tokio::test]
async fn a_file_the_delete_could_not_remove_goes_on_the_retry() -> R {
    let env = Env::new(true).await?;
    ownhealth_auth::register();
    let sam = env.profile("sam");
    let (stuck, path) = stuck_report(&env).await?;
    let (_, again) = document(
        &env,
        env.profile("alex"),
        "%PDF-1.7 synthetic report Sam uploads again later",
        "application/pdf",
    )
    .await?;

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    // the response and the audit say what happened: one file removed now, one waiting for the retry
    assert_eq!(
        (&r.json["blobs"], &r.json["blobs_pending"]),
        (&json!(1), &json!(1)),
        "{}",
        r.json
    );
    let said: Vec<Option<String>> = sqlx::query_scalar(
        "select args_summary from audit_event where action = 'purge' and resource_type = 'blob' \
         and actor_kind <> 'system'",
    )
    .fetch_all(env.pool())
    .await?;
    assert_eq!(said, vec![Some("blobs=1 pending=1".into())]);
    // the job names only the file still on disk, and carries the household and file hashes only
    let queued: Value = sqlx::query_scalar("select payload from job where kind = $1")
        .bind(UNLINK_JOB)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(
        queued,
        json!({"household_id": env.fam().household_id, "blobs": [stuck.clone()]})
    );
    assert_eq!(unlink_state(&env).await?, ("queued".into(), 0, true));

    // the first retry fails too: the job waits for another try
    assert_eq!(run_unlink(&env).await?, 1);
    assert_eq!(unlink_state(&env).await?, ("queued".into(), 1, true));
    assert!(path.is_dir(), "the stuck file went without the retry");

    // the disk lets go; meanwhile Sam uploads the other report, so its file is named again
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    std::fs::remove_dir_all(&path)?;
    assert_eq!(
        env.rt
            .blobs
            .put_bytes(b"%PDF-1.7 synthetic report the disk would not let go")?,
        stuck
    );
    sqlx::query("update job set payload = jsonb_set(payload, '{blobs}', payload->'blobs' || to_jsonb($1::text))")
        .bind(&again)
        .execute(env.pool())
        .await?;
    document(
        &env,
        sam,
        "%PDF-1.7 synthetic report Sam uploads again later",
        "application/pdf",
    )
    .await?;
    assert_eq!(run_unlink(&env).await?, 1);

    let left = stored(env.rt.blobs.root());
    assert!(!left.contains(&stuck), "the retry left Alex's file");
    assert!(left.contains(&again), "the retry removed a file Sam's document names");
    assert_eq!(unlink_state(&env).await?.0, "done");
    let audit: Vec<(String, Option<String>)> =
        sqlx::query_as("select via, args_summary from audit_event where action = 'purge' and actor_kind = 'system'")
            .fetch_all(env.pool())
            .await?;
    assert_eq!(audit, vec![("job".into(), Some("blobs=1".into()))]);
    Ok(())
}

#[tokio::test]
async fn a_retry_that_runs_out_of_attempts_stays_for_the_operator() -> R {
    let env = Env::new(true).await?;
    ownhealth_auth::register();
    let (_, path) = stuck_report(&env).await?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    sqlx::query("update job set max_attempts = 1 where kind = $1")
        .bind(UNLINK_JOB)
        .execute(env.pool())
        .await?;
    assert_eq!(run_unlink(&env).await?, 1);
    let (state, error): (String, Option<String>) = sqlx::query_as("select state, last_error from job where kind = $1")
        .bind(UNLINK_JOB)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(state, "dead");
    assert!(error.is_some_and(|e| e.contains("could not be removed")));
    // an item of the household's review queue says so; the file is still listed in the job
    let items: Vec<(Option<Uuid>, Value)> =
        sqlx::query_as("select household_id, payload from review_item where kind = 'job_failed'")
            .fetch_all(env.pool())
            .await?;
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].0, Some(env.fam().household_id));
    assert_eq!(items[0].1["kind"], json!(UNLINK_JOB));
    assert!(path.is_dir());
    // and the household's owner sees it in the review list
    let r = me.get("/api/review?kind=job_failed").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let shown: Vec<&Value> = r.json["items"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    assert_eq!(shown.len(), 1, "{}", r.json);
    assert_eq!(shown[0]["payload"]["kind"], json!(UNLINK_JOB), "{}", r.json);
    Ok(())
}

#[tokio::test]
async fn the_retry_is_queued_in_the_delete_transaction() -> R {
    let env = Env::new(true).await?;
    let alex = env.profile("alex");
    let (_, report) = document(&env, alex, "%PDF-1.7 synthetic LabNorte report", "application/pdf").await?;
    let mut tx = env.pool().begin().await?;
    let deleted = ownhealth_auth::household::delete_profile(&mut tx, alex)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(deleted.blobs, vec![report.clone()]);
    // inside the transaction the job is there already, and it waits so the delete removes the file first
    let (payload, waits): (Value, bool) = sqlx::query_as("select payload, run_after > now() from job where kind = $1")
        .bind(UNLINK_JOB)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(
        payload,
        json!({"household_id": env.fam().household_id, "blobs": [report]})
    );
    assert!(waits);
    // a crash before the commit loses both the delete and the job
    tx.rollback().await?;
    assert_eq!(
        env.scalar_i64(&format!("select count(*) from job where kind = '{UNLINK_JOB}'"))
            .await?,
        0
    );
    assert_eq!(
        env.scalar_i64(&format!("select count(*) from profile where id = '{alex}'"))
            .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_demo_delete_leaves_the_real_inbox_alone() -> R {
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.settings.demo = true;
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    // the demo stack shares ./inbox with the real install, and the demo's scanner folder reads its root:
    // inbox/alex/ belongs to the real install's Alex, not to the demo's
    folder_source(&env, "", None).await?;
    let real = inbox.join("alex/escaneo-real.pdf");
    let filed = inbox.join(".done/alex/escaneo-real-anterior.pdf");
    for p in [&real, &filed] {
        std::fs::create_dir_all(p.parent().expect("parent"))?;
        std::fs::write(p, b"%PDF-1.7 the real install's scan")?;
    }
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(real.is_file(), "the demo delete removed the real install's unread scan");
    assert!(filed.is_file(), "the demo delete removed the real install's filed scan");
    // and it leaves no retry that would remove them later; a retry queued anyway does nothing in the demo
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind = 'folder.forget'")
            .await?,
        0
    );
    ownhealth_inbox::register();
    let mut c = env.pool().acquire().await?;
    ownhealth_inbox::folder::queue_forget(&mut c, env.fam().household_id, "alex", &[(json!({}), false)]).await?;
    drop(c);
    assert_eq!(run_forget(&env).await?, 1);
    assert!(
        real.is_file() && filed.is_file(),
        "a demo retry removed the real install's scans"
    );
    Ok(())
}

#[tokio::test]
async fn an_inbox_item_of_someone_else_with_the_same_bytes_stays() -> R {
    let env = Env::new(true).await?;
    let (alex, sam, household) = (env.profile("alex"), env.profile("sam"), env.fam().household_id);
    // Alex uploaded a file whose bytes Sam's own Health Auto Export inbox also received as a push
    let body = "{\"data\": {\"metrics\": []}}";
    let (_, sha) = document(&env, alex, body, "application/json").await?;
    let sams: Uuid = sqlx::query_scalar(
        "insert into source (household_id, profile_id, kind, label) \
         values ($1, $2, 'health_auto_export', 'iPhone de Sam') returning id",
    )
    .bind(household)
    .bind(sam)
    .fetch_one(env.pool())
    .await?;
    item(&env, sams, &format!("push:{sha}"), &sha, None, None).await?;
    // and so did a household inbox of another household on this server
    let mut c = env.pool().acquire().await?;
    let ortega = ownhealth_core::access::create_household(&mut c, "Familia Ortega").await?;
    drop(c);
    let theirs: Uuid = sqlx::query_scalar(
        "insert into source (household_id, kind, label) values ($1, 'paperless', 'Paperless Ortega') returning id",
    )
    .bind(ortega)
    .fetch_one(env.pool())
    .await?;
    item(&env, theirs, "5", &sha, None, None).await?;

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        items(&env, sams).await?,
        vec![(format!("push:{sha}"), Some(sha.clone()))]
    );
    assert_eq!(items(&env, theirs).await?, vec![("5".into(), Some(sha.clone()))]);
    assert!(
        stored(env.rt.blobs.root()).contains(&sha),
        "Sam's push file went with Alex"
    );
    Ok(())
}

#[tokio::test]
async fn a_mail_with_nothing_to_import_goes_with_the_person() -> R {
    let env = Env::new(true).await?;
    let alex = env.profile("alex");
    let mailbox = household_source(&env, "email", json!({})).await?;
    sqlx::query(
        "insert into sender_rule (source_id, pattern, profile_id, trust_unauthenticated) \
         values ($1, 'labnorte.example', $2, true)",
    )
    .bind(mailbox)
    .bind(alex)
    .execute(env.pool())
    .await?;
    // an allowed sender's notice with only a body: nothing becomes a document, the whole .eml is kept
    let raw = "From: Resultados LabNorte <resultados@labnorte.example>\r\n\
               To: salud@rivera.example\r\n\
               Subject: Sus resultados ya estan disponibles\r\n\
               Message-ID: <aviso-77@labnorte.example>\r\n\
               Date: Fri, 20 Nov 2026 09:00:00 +0100\r\n\
               MIME-Version: 1.0\r\n\
               Content-Type: text/plain; charset=utf-8\r\n\r\n\
               Sus resultados de Alex Rivera ya estan disponibles en el portal de LabNorte.\r\n";
    let mut c = env.pool().acquire().await?;
    let source = sources::get_source(&mut c, mailbox).await?.expect("mailbox");
    let out =
        ownhealth_inbox::email::ingest_message(&mut c, &env.rt.blobs, None, &source, raw.as_bytes(), None).await?;
    drop(c);
    assert!(out.document_ids.is_empty(), "{out:?}");
    let eml = env.rt.blobs.put_bytes(raw.as_bytes())?;
    assert_eq!(
        items(&env, mailbox).await?,
        vec![("mid:aviso-77@labnorte.example".into(), Some(eml.clone()))]
    );

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(
        !stored(env.rt.blobs.root()).contains(&eml),
        "the notice to Alex is still on disk: {}",
        r.json
    );
    // the mailbox still knows the message, so the next poll does not import it again
    assert_eq!(
        items(&env, mailbox).await?,
        vec![("mid:aviso-77@labnorte.example".into(), None)]
    );
    Ok(())
}

#[tokio::test]
async fn an_item_filed_to_someone_else_keeps_its_file() -> R {
    let env = Env::new(true).await?;
    let (alex, sam) = (env.profile("alex"), env.profile("sam"));
    let paperless = household_source(&env, "paperless", json!({})).await?;
    // Alex uploaded a file; the household Paperless later filed the same bytes to Sam, whose
    // structured import is still queued, so the item names Sam and no document yet
    let (_, sha) = document(&env, alex, "{\"synthetic\": \"LabNorte export\"}", "application/json").await?;
    item(&env, paperless, "41", &sha, None, None).await?;
    sqlx::query("update source_item set profile_id = $1 where source_id = $2")
        .bind(sam)
        .bind(paperless)
        .execute(env.pool())
        .await?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        items(&env, paperless).await?,
        vec![("41".into(), Some(sha.clone()))],
        "Sam's item lost its file"
    );
    assert!(
        stored(env.rt.blobs.root()).contains(&sha),
        "Sam's queued file went with Alex: {}",
        r.json
    );
    Ok(())
}

#[tokio::test]
async fn a_released_mail_with_only_a_body_goes_with_the_person() -> R {
    let env = Env::new(true).await?;
    let mailbox = household_source(&env, "email", json!({})).await?;
    // a new sender, so the household mailbox holds the notice; a person releases it into Alex
    let raw = "From: Avisos Clinica Nueva <avisos@clinica-nueva.example>\r\n\
               To: salud@rivera.example\r\n\
               Subject: Cita confirmada\r\n\
               Message-ID: <aviso-88@clinica-nueva.example>\r\n\
               Date: Fri, 20 Nov 2026 10:00:00 +0100\r\n\
               MIME-Version: 1.0\r\n\
               Content-Type: text/plain; charset=utf-8\r\n\r\n\
               La cita de Alex Rivera queda confirmada para el lunes.\r\n";
    let mut c = env.pool().acquire().await?;
    let source = sources::get_source(&mut c, mailbox).await?.expect("mailbox");
    let held = ownhealth_inbox::email::ingest_message(&mut c, &env.rt.blobs, None, &source, raw.as_bytes(), None)
        .await?
        .quarantine_id
        .expect("held");
    drop(c);
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me
        .post(
            &format!("/api/inboxes/quarantine/{held}/release"),
            &json!({"profile_id": env.profile("alex")}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["document_ids"], json!([]), "{}", r.json);
    let eml = env.rt.blobs.put_bytes(raw.as_bytes())?;

    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(
        !stored(env.rt.blobs.root()).contains(&eml),
        "the released notice is still on disk: {}",
        r.json
    );
    assert_eq!(
        env.scalar_i64(&format!("select count(*) from quarantine_item where id = '{held}'"))
            .await?,
        0,
        "the released notice's sender and subject stay"
    );
    // the mailbox still knows the message, so the next poll does not import it again
    assert_eq!(
        items(&env, mailbox).await?,
        vec![("mid:aviso-88@clinica-nueva.example".into(), None)]
    );
    Ok(())
}

#[tokio::test]
async fn a_push_an_inbox_received_for_someone_else_keeps_its_file() -> R {
    let env = Env::new(true).await?;
    let (alex, sam) = (env.profile("alex"), env.profile("sam"));
    let mut c = env.pool().acquire().await?;
    let id = sources::create_source(
        &mut c,
        &NewSource {
            profile_id: Some(alex),
            created_by: Some(env.user("alex")),
            ..NewSource::new(env.fam().household_id, "health_auto_export", "iPad de casa")
        },
    )
    .await?;
    // the iPad pushed for Alex, then the inbox moved to Sam and the same body came again for Sam
    let body = b"{\"data\": {\"metrics\": [], \"workouts\": []}}";
    let source = sources::get_source(&mut c, id).await?.expect("source");
    ownhealth_devices::hae::import_push(&mut c, &env.rt.blobs, &source, body).await?;
    sqlx::query("update source set profile_id = $1 where id = $2")
        .bind(sam)
        .bind(id)
        .execute(&mut *c)
        .await?;
    let source = sources::get_source(&mut c, id).await?.expect("source");
    ownhealth_devices::hae::import_push(&mut c, &env.rt.blobs, &source, body).await?;
    drop(c);
    let sha = env.rt.blobs.put_bytes(body)?;

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        items(&env, id).await?,
        vec![(format!("push:{sha}"), Some(sha.clone()))],
        "Sam's push lost its file"
    );
    assert!(stored(env.rt.blobs.root()).contains(&sha), "Sam's push went with Alex");
    Ok(())
}

/// Run the folder cleanup retry now.
async fn run_forget(env: &Env) -> anyhow::Result<usize> {
    sqlx::query("update job set run_after = now() where kind = 'folder.forget' and state = 'queued'")
        .execute(env.pool())
        .await?;
    let ctx = JobContext::new(env.pool().clone(), Arc::new(env.settings.clone()), env.rt.blobs.clone());
    Ok(drain(&ctx, "core", 5, Some(&["folder.forget".to_string()])).await?)
}

#[tokio::test]
async fn a_folder_cleanup_that_fails_is_retried() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    ownhealth_inbox::register();
    folder_source(&env, "estudio", None).await?;
    let root = inbox.join("estudio");
    // Alex's drop, and a filed copy the disk will not let go yet (its folder is read-only)
    std::fs::create_dir_all(root.join("alex"))?;
    std::fs::write(root.join("alex/escaneo.pdf"), b"%PDF-1.7 synthetic scan")?;
    let locked = root.join(".done/alex/2026-11");
    std::fs::create_dir_all(&locked)?;
    std::fs::write(locked.join("escaneo-anterior.pdf"), b"%PDF-1.7 synthetic filed scan")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))?;

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(!root.join("alex").exists(), "the drop folder stays");
    assert!(locked.join("escaneo-anterior.pdf").is_file());
    // the retry waits with the household, the short name and the folder inbox's settings
    let (payload, state, waits): (Value, String, bool) =
        sqlx::query_as("select payload, state, run_after > now() from job where kind = 'folder.forget'")
            .fetch_one(env.pool())
            .await?;
    assert_eq!(
        payload,
        json!({"household_id": env.fam().household_id, "slug": "alex",
               "folders": [{"config": {"subdir": "estudio"}, "own": false}]})
    );
    assert_eq!((state.as_str(), waits), ("queued", true));

    // the first retry fails too and waits again; then the disk lets go and the retry finishes
    assert_eq!(run_forget(&env).await?, 1);
    let (state, attempts): (String, i32) =
        sqlx::query_as("select state, attempts from job where kind = 'folder.forget'")
            .fetch_one(env.pool())
            .await?;
    assert_eq!((state.as_str(), attempts), ("queued", 1));
    assert!(locked.join("escaneo-anterior.pdf").is_file());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
    assert_eq!(run_forget(&env).await?, 1);
    assert!(!root.join(".done/alex").exists(), "the retry left Alex's filed copy");
    let (state, payload): (String, Value) =
        sqlx::query_as("select state, payload from job where kind = 'folder.forget'")
            .fetch_one(env.pool())
            .await?;
    assert_eq!(state, "done");
    // the job row stays, without the person's short name
    assert_eq!(
        payload,
        json!({"household_id": env.fam().household_id,
               "folders": [{"config": {"subdir": "estudio"}, "own": false}]})
    );
    Ok(())
}

#[tokio::test]
async fn a_folder_retry_that_runs_out_shows_in_review_without_the_name() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    ownhealth_inbox::register();
    folder_source(&env, "estudio", None).await?;
    let locked = inbox.join("estudio/.done/alex/2026-11");
    std::fs::create_dir_all(&locked)?;
    std::fs::write(locked.join("escaneo-anterior.pdf"), b"%PDF-1.7 synthetic filed scan")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    sqlx::query("update job set max_attempts = 1 where kind = 'folder.forget'")
        .execute(env.pool())
        .await?;
    assert_eq!(run_forget(&env).await?, 1);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
    let (state, payload): (String, Value) =
        sqlx::query_as("select state, payload from job where kind = 'folder.forget'")
            .fetch_one(env.pool())
            .await?;
    assert_eq!(state, "dead");
    assert!(
        payload.get("slug").is_none(),
        "a dead job kept the short name: {payload}"
    );
    let items: Vec<Option<Uuid>> = sqlx::query_scalar("select household_id from review_item where kind = 'job_failed'")
        .fetch_all(env.pool())
        .await?;
    assert_eq!(items, vec![Some(env.fam().household_id)]);
    Ok(())
}

#[tokio::test]
async fn a_new_person_with_the_same_short_name_keeps_their_drop() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    ownhealth_inbox::register();
    folder_source(&env, "estudio", None).await?;
    let root = inbox.join("estudio");
    let locked = root.join(".done/alex/2026-11");
    std::fs::create_dir_all(&locked)?;
    std::fs::write(locked.join("escaneo-anterior.pdf"), b"%PDF-1.7 synthetic filed scan")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind = 'folder.forget' and state = 'queued'")
            .await?,
        1
    );
    // a minute later the household adds a new person with the short name alex, who drops a scan
    sqlx::query("insert into profile (household_id, slug, display_name) values ($1, 'alex', 'Alex Ortega')")
        .bind(env.fam().household_id)
        .execute(env.pool())
        .await?;
    let drop = root.join("alex/escaneo-nuevo.pdf");
    std::fs::create_dir_all(drop.parent().expect("parent"))?;
    std::fs::write(&drop, b"%PDF-1.7 the new Alex scan, not read yet")?;
    // the disk lets go of the old file and the retry runs
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
    assert_eq!(run_forget(&env).await?, 1);
    assert!(drop.is_file(), "the retry removed the new Alex unread scan");
    assert!(
        locked.join("escaneo-anterior.pdf").is_file(),
        "the retry removed a filed copy that the new Alex may own"
    );
    let state: String = sqlx::query_scalar("select state from job where kind = 'folder.forget'")
        .fetch_one(env.pool())
        .await?;
    assert_eq!(state, "done");
    Ok(())
}

#[tokio::test]
async fn a_new_folder_inbox_on_the_same_folder_keeps_its_files() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    ownhealth_inbox::register();
    // Alex's own folder inbox: its root files went to Alex, so .done/_root and .failed/_root are Alex's
    folder_source(&env, "estudio", Some(env.profile("alex"))).await?;
    let root = inbox.join("estudio");
    let locked = root.join(".done/_root/2026-11");
    std::fs::create_dir_all(&locked)?;
    std::fs::write(locked.join("escaneo-anterior.pdf"), b"%PDF-1.7 synthetic filed scan")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind = 'folder.forget' and state = 'queued'")
            .await?,
        1
    );
    // another household now reads the same folder, with a person whose short name is alex
    let mut c = env.pool().acquire().await?;
    let ortega = ownhealth_core::access::create_household(&mut c, "Familia Ortega").await?;
    drop(c);
    sqlx::query(
        "insert into source (household_id, kind, label, config) \
         values ($1, 'folder', 'Escáner Ortega', '{\"subdir\": \"estudio\"}')",
    )
    .bind(ortega)
    .execute(env.pool())
    .await?;
    sqlx::query("insert into profile (household_id, slug, display_name) values ($1, 'alex', 'Alex Ortega')")
        .bind(ortega)
        .execute(env.pool())
        .await?;
    let failed = root.join(".failed/_root/factura.pdf");
    std::fs::create_dir_all(failed.parent().expect("parent"))?;
    std::fs::write(&failed, b"%PDF-1.7 a file the new inbox could not read")?;
    let theirs = root.join("alex/escaneo-ortega.pdf");
    std::fs::create_dir_all(theirs.parent().expect("parent"))?;
    std::fs::write(&theirs, b"%PDF-1.7 the other household's alex scan")?;
    // the disk lets go of the old file and the retry runs
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
    assert_eq!(run_forget(&env).await?, 1);
    assert!(failed.is_file(), "the retry removed the new inbox's failed file");
    assert!(theirs.is_file(), "the retry removed the other household's alex scan");
    Ok(())
}

#[tokio::test]
async fn a_folder_inbox_linked_out_of_the_inbox_keeps_outside_files() -> R {
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    let outside = env.dir.path().join("nas-familia");
    std::fs::create_dir_all(&inbox)?;
    for part in ["alex", ".done/alex", ".failed/alex"] {
        std::fs::create_dir_all(outside.join(part))?;
        std::fs::write(outside.join(part).join("foto-personal.jpg"), b"not an OwnHealth file")?;
    }
    std::os::unix::fs::symlink(&outside, inbox.join("escaneos"))?;
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    folder_source(&env, "escaneos", None).await?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    for part in ["alex", ".done/alex", ".failed/alex"] {
        assert!(
            outside.join(part).join("foto-personal.jpg").is_file(),
            "the delete removed a file outside the inbox folder: {part}"
        );
    }
    // the skip is quiet: no retry waits on a folder it will never clean
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind = 'folder.forget'")
            .await?,
        0
    );
    Ok(())
}

/// Delete Alex and expect `kept` to survive it.
async fn delete_alex_and_keep(env: &Env, kept: &Path, what: &str) -> R {
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(kept.is_file(), "the delete removed {what}");
    Ok(())
}

#[tokio::test]
async fn a_link_inside_the_inbox_to_the_same_folder_keeps_the_other_inbox_files() -> R {
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    std::fs::create_dir_all(inbox.join("estudio"))?;
    std::os::unix::fs::symlink(inbox.join("estudio"), inbox.join("consulta"))?;
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    // Alex's own inbox reads estudio; the household's shared inbox reads consulta, a link to estudio
    folder_source(&env, "estudio", Some(env.profile("alex"))).await?;
    folder_source(&env, "consulta", None).await?;
    let theirs = inbox.join("estudio/.failed/_root/factura.pdf");
    std::fs::create_dir_all(theirs.parent().expect("parent"))?;
    std::fs::write(&theirs, b"%PDF-1.7 a file the shared inbox could not read")?;
    delete_alex_and_keep(&env, &theirs, "the shared inbox's failed file through a link").await
}

#[tokio::test]
async fn a_letter_case_variant_of_the_same_folder_keeps_the_other_inbox_files() -> R {
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    std::fs::create_dir_all(inbox.join("estudio"))?;
    if !inbox.join("Estudio").is_dir() {
        // a file system that tells letter cases apart: Estudio is another folder
        return Ok(());
    }
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    folder_source(&env, "Estudio", Some(env.profile("alex"))).await?;
    folder_source(&env, "estudio", None).await?;
    let theirs = inbox.join("estudio/.failed/_root/factura.pdf");
    std::fs::create_dir_all(theirs.parent().expect("parent"))?;
    std::fs::write(&theirs, b"%PDF-1.7 a file the shared inbox could not read")?;
    delete_alex_and_keep(
        &env,
        &theirs,
        "the shared inbox's failed file through a letter-case variant",
    )
    .await
}

#[tokio::test]
async fn an_inbox_on_a_folder_not_made_yet_inside_the_drop_keeps_the_drop() -> R {
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    std::fs::create_dir_all(inbox.join("estudio/alex"))?;
    std::os::unix::fs::symlink(inbox.join("estudio/alex"), inbox.join("escaner"))?;
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    folder_source(&env, "estudio", None).await?;
    // a second inbox reads escaner/nuevo: estudio/alex/nuevo once someone makes it
    folder_source(&env, "escaner/nuevo", None).await?;
    let drop = inbox.join("estudio/alex/escaneo.pdf");
    std::fs::write(&drop, b"%PDF-1.7 synthetic scan")?;
    delete_alex_and_keep(&env, &drop, "the drop that another inbox will read a folder of").await
}

#[tokio::test]
async fn an_inbox_folder_the_server_cannot_resolve_keeps_the_files() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    let closed = inbox.join("cerrado");
    std::fs::create_dir_all(closed.join("escaner"))?;
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    folder_source(&env, "estudio", Some(env.profile("alex"))).await?;
    // the server cannot see where cerrado/escaner leads, so it could be estudio
    folder_source(&env, "cerrado/escaner", None).await?;
    let failed = inbox.join("estudio/.failed/_root/factura.pdf");
    std::fs::create_dir_all(failed.parent().expect("parent"))?;
    std::fs::write(&failed, b"%PDF-1.7 synthetic failed scan")?;
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000))?;
    let kept = delete_alex_and_keep(&env, &failed, "a file while another inbox could not be resolved").await;
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755))?;
    kept
}

#[tokio::test]
async fn a_new_inbox_inside_the_drop_keeps_its_files() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    ownhealth_inbox::register();
    folder_source(&env, "estudio", Some(env.profile("alex"))).await?;
    let root = inbox.join("estudio");
    let locked = root.join(".done/_root/2026-11");
    std::fs::create_dir_all(&locked)?;
    std::fs::write(locked.join("escaneo-anterior.pdf"), b"%PDF-1.7 synthetic filed scan")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    // before the retry runs, a new shared inbox starts reading estudio/alex
    sqlx::query("insert into source (household_id, kind, label, config) values ($1, 'folder', 'Nuevo', '{\"subdir\": \"estudio/alex\"}')")
        .bind(env.fam().household_id)
        .execute(env.pool())
        .await?;
    let drop = root.join("alex/escaneo-nuevo.pdf");
    std::fs::create_dir_all(drop.parent().expect("parent"))?;
    std::fs::write(&drop, b"%PDF-1.7 a scan of the new inbox")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
    assert_eq!(run_forget(&env).await?, 1);
    assert!(drop.is_file(), "the retry removed the new inbox's scan");
    Ok(())
}

#[tokio::test]
async fn a_namesake_in_an_unrelated_household_does_not_keep_the_copies() -> R {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    ownhealth_inbox::register();
    folder_source(&env, "estudio", None).await?;
    let root = inbox.join("estudio");
    let locked = root.join(".done/alex/2026-11");
    std::fs::create_dir_all(&locked)?;
    std::fs::write(locked.join("escaneo-anterior.pdf"), b"%PDF-1.7 synthetic filed scan")?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500))?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    // another household, with no folder inbox on estudio, has a person whose short name is alex
    let mut c = env.pool().acquire().await?;
    let ortega = ownhealth_core::access::create_household(&mut c, "Familia Ortega").await?;
    drop(c);
    sqlx::query("insert into profile (household_id, slug, display_name) values ($1, 'alex', 'Alex Ortega')")
        .bind(ortega)
        .execute(env.pool())
        .await?;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
    assert_eq!(run_forget(&env).await?, 1);
    assert!(
        !root.join(".done/alex").exists(),
        "a namesake in an unrelated household kept the deleted Alex's filed copies"
    );
    Ok(())
}

#[tokio::test]
async fn a_missing_inbox_folder_leaves_nothing_to_retry() -> R {
    let mut env = Env::new(true).await?;
    // the folder inbox is set up, but its folder was never made
    env.settings.inbox_dir = Some(env.dir.path().join("inbox"));
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    folder_source(&env, "estudio", None).await?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind = 'folder.forget'")
            .await?,
        0,
        "a folder that does not exist left a retry"
    );
    Ok(())
}

#[tokio::test]
async fn a_rerun_with_no_short_name_ends_as_skipped() -> R {
    let mut env = Env::new(true).await?;
    env.settings.inbox_dir = Some(env.dir.path().join("inbox"));
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    ownhealth_inbox::register();
    // an operator reruns a folder retry that ended once, so its short name is gone
    let mut c = env.pool().acquire().await?;
    ownhealth_core::jobs::enqueue(
        &mut c,
        "folder.forget",
        &json!({"household_id": env.fam().household_id,
                "folders": [{"config": {"subdir": "estudio"}, "own": false}]}),
        ownhealth_core::jobs::EnqueueOptions::default(),
    )
    .await?;
    drop(c);
    assert_eq!(run_forget(&env).await?, 1);
    let (state, result): (String, Option<Value>) =
        sqlx::query_as("select state, result from job where kind = 'folder.forget'")
            .fetch_one(env.pool())
            .await?;
    assert_eq!(
        (state.as_str(), result),
        ("done", Some(json!({"skipped": "no short name"})))
    );
    Ok(())
}

/// Make every audit row of `resource_type` fail, the way a full disk or a broken database would.
async fn fail_audit(env: &Env, resource_type: &str) -> R {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "create function test_fail_audit() returns trigger language plpgsql as $$ begin \
         if new.resource_type = '{resource_type}' then raise exception 'the audit is down'; end if; \
         return new; end $$"
    )))
    .execute(env.pool())
    .await?;
    sqlx::query(
        "create trigger test_fail_audit before insert on audit_event for each row \
         execute function test_fail_audit()",
    )
    .execute(env.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn the_folder_retry_is_queued_in_the_delete_transaction() -> R {
    let mut env = Env::new(true).await?;
    let inbox = env.dir.path().join("inbox");
    env.settings.inbox_dir = Some(inbox.clone());
    env.rt = Runtime::with_pool(env.settings.clone(), env.db.pool.clone())?;
    folder_source(&env, "estudio", None).await?;
    std::fs::create_dir_all(inbox.join("estudio/alex"))?;
    std::fs::write(inbox.join("estudio/alex/escaneo.pdf"), b"%PDF-1.7 synthetic scan")?;
    // the delete fails before its commit, so nothing of it may stay
    fail_audit(&env, "profile").await?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_ne!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        env.scalar_i64("select count(*) from profile where slug = 'alex'")
            .await?,
        1
    );
    assert_eq!(
        env.scalar_i64("select count(*) from job where kind = 'folder.forget'")
            .await?,
        0,
        "a retry outlived the rolled-back delete: it would remove a live person's folders"
    );
    assert!(inbox.join("estudio/alex/escaneo.pdf").is_file());
    Ok(())
}

#[tokio::test]
async fn a_done_delete_answers_success_when_its_last_audit_fails() -> R {
    let env = Env::new(true).await?;
    let (_, report) = document(
        &env,
        env.profile("alex"),
        "%PDF-1.7 synthetic LabNorte report",
        "application/pdf",
    )
    .await?;
    fail_audit(&env, "blob").await?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "a done delete answered a failure: {}", r.json);
    assert_eq!(
        (&r.json["deleted"], &r.json["blobs"], &r.json["blobs_pending"]),
        (&json!("alex"), &json!(1), &json!(0)),
        "{}",
        r.json
    );
    assert!(!stored(env.rt.blobs.root()).contains(&report));
    assert_eq!(
        env.scalar_i64("select count(*) from profile where slug = 'alex'")
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_file_already_missing_is_not_counted_as_removed() -> R {
    let env = Env::new(true).await?;
    let alex = env.profile("alex");
    let (_, gone) = document(&env, alex, "%PDF-1.7 synthetic report already gone", "application/pdf").await?;
    let (_, here) = document(&env, alex, "%PDF-1.7 synthetic LabNorte report", "application/pdf").await?;
    std::fs::remove_file(env.rt.blobs.path(&gone)?)?;
    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        (&r.json["blobs"], &r.json["blobs_pending"]),
        (&json!(1), &json!(0)),
        "a file that was already missing counted as removed: {}",
        r.json
    );
    assert!(!stored(env.rt.blobs.root()).contains(&here));
    Ok(())
}
