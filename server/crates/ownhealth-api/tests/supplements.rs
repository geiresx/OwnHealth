//! Supplements as first-class records (migration 1101): a label photo kept next to the entry, and the
//! person's own decision (keep, stop, do not repurchase) recorded as a new version.

use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::household::delete_profile;
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// A 1x1 PNG (a grey pixel).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x3A, 0x7E, 0x9B, 0x55, 0x00, 0x00, 0x00, 0x0A, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x68, 0x00, 0x00, 0x00, 0x82, 0x00, 0x81, 0x77, 0xCD, 0x72, 0xB6, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

const BOUNDARY: &str = "ownhealth-label-boundary";

struct App {
    db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    family: Family,
}

async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    Ok(App {
        db,
        _dir: dir,
        rt,
        family,
    })
}

async fn send(a: &App, user: &str, req: http::request::Builder, body: Body) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(user), None, None, "passkey").await?;
    let req = req.header(header::COOKIE, format!("{COOKIE}={secret}")).body(body)?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 24).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

async fn post(a: &App, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
    let req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    send(a, "alex", req, Body::from(body.to_string())).await
}

async fn get(a: &App, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
    send(a, "alex", Request::get(uri), Body::empty()).await
}

fn multipart(field: &str, filename: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn upload(a: &App, user: &str, uri: &str, filename: &str, bytes: &[u8]) -> anyhow::Result<(StatusCode, Value)> {
    let req = Request::post(uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    send(a, user, req, Body::from(multipart("file", filename, bytes))).await
}

async fn count(a: &App, sql: &str, id: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(&a.db.pool)
        .await?)
}

async fn supplement(a: &App, profile: &str) -> anyhow::Result<Value> {
    let (s, made) = post(
        a,
        &format!("/profiles/{profile}/records/medication"),
        json!({"kind": "supplement", "name": "Vitamin D3", "strength": "2000 IU", "started": "2026-01-10"}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    Ok(made)
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn a_label_photo_is_a_document_with_no_job_and_no_review_item() -> R {
    let a = app().await?;
    let made = supplement(&a, "alex").await?;
    assert_eq!(made["labels"], json!([]), "a new record has no label yet");
    let id = id_of(&made);
    let pid = a.family.profile("alex");
    let jobs_before = count(&a, "select count(*) from job where $1::uuid is not null", pid).await?;
    let reviews_before = count(&a, "select count(*) from review_item where profile_id = $1", pid).await?;

    let uri = format!("/profiles/alex/records/medication/{id}/label");
    let (s, out) = upload(&a, "alex", &uri, "label.png", PNG).await?;
    assert_eq!(s, StatusCode::CREATED, "{out}");
    let doc = Uuid::parse_str(out["document_id"].as_str().unwrap_or_default())?;
    assert_eq!(out["medication_id"], json!(id));

    let (doc_type, mime, owner): (Option<String>, String, Option<Uuid>) =
        sqlx::query_as("select doc_type, mime, profile_id from document where id = $1")
            .bind(doc)
            .fetch_one(&a.db.pool)
            .await?;
    assert_eq!(
        (doc_type.as_deref(), mime.as_str(), owner),
        (Some("other"), "image/png", Some(pid))
    );
    assert_eq!(
        count(&a, "select count(*) from job where $1::uuid is not null", pid).await?,
        jobs_before,
        "no extraction job"
    );
    assert_eq!(
        count(&a, "select count(*) from review_item where profile_id = $1", pid).await?,
        reviews_before,
        "no review item"
    );

    // the record read lists the label, and the file opens as the image it is
    let (_, list) = get(&a, "/profiles/alex/records/medication").await?;
    let row = list["items"]
        .as_array()
        .and_then(|i| i.iter().find(|r| r["id"] == json!(id)))
        .cloned()
        .unwrap_or_default();
    assert_eq!(row["labels"], json!([doc.to_string()]), "{row}");
    let req = Request::get(format!("/documents/{doc}/file"));
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user("alex"), None, None, "passkey").await?;
    let res = ownhealth_api::router()
        .with_state(a.rt.clone())
        .oneshot(
            req.header(header::COOKIE, format!("{COOKIE}={secret}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(axum::body::to_bytes(res.into_body(), 1 << 20).await?.as_ref(), PNG);
    Ok(())
}

#[tokio::test]
async fn a_label_is_refused_for_a_viewer_a_non_image_and_an_old_version() -> R {
    let a = app().await?;
    let made = supplement(&a, "alex").await?;
    let id = id_of(&made);
    let uri = format!("/profiles/alex/records/medication/{id}/label");
    // Sam only views Alex's profile
    let (s, out) = upload(&a, "sam", &uri, "label.png", PNG).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{out}");
    // a PDF is not a label photo
    let (s, out) = upload(&a, "alex", &uri, "label.pdf", b"%PDF-1.7\n%fake").await?;
    assert_eq!(
        (s, &out["detail"]["code"]),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, &json!("unsupported_upload")),
        "{out}"
    );
    // a missing file part
    let req = Request::post(&uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={BOUNDARY}"),
    );
    let (s, out) = send(&a, "alex", req, Body::from(format!("--{BOUNDARY}--\r\n"))).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    // another profile's record is unknown here
    let carmen = supplement(&a, "carmen").await?;
    let (s, _) = upload(
        &a,
        "alex",
        &format!("/profiles/alex/records/medication/{}/label", id_of(&carmen)),
        "label.png",
        PNG,
    )
    .await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // a version that was replaced takes no new label: the photo belongs on the current one
    let mut body = made["detail"].clone();
    body["dose_text"] = json!("1 capsule");
    let (s, _) = post(&a, &format!("/profiles/alex/records/medication/{id}/correct"), body).await?;
    assert_eq!(s, StatusCode::CREATED);
    let (s, out) = upload(&a, "alex", &uri, "label.png", PNG).await?;
    assert_eq!(s, StatusCode::CONFLICT, "{out}");
    let docs = count(
        &a,
        "select count(*) from document where profile_id = $1",
        a.family.profile("alex"),
    )
    .await?;
    assert_eq!(docs, 0, "nothing refused was stored as a document");
    Ok(())
}

#[tokio::test]
async fn a_decision_is_a_new_version_that_keeps_the_reason_and_the_label() -> R {
    let a = app().await?;
    let made = supplement(&a, "alex").await?;
    let id = id_of(&made);
    let (s, label) = upload(
        &a,
        "alex",
        &format!("/profiles/alex/records/medication/{id}/label"),
        "label.png",
        PNG,
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{label}");
    assert_eq!(made["detail"]["decision"], Value::Null);

    // the screen sends the row's own detail plus the decision
    let mut body = made["detail"].clone();
    body["decision"] = json!("do_not_repurchase");
    body["decision_reason"] = json!("Too large to swallow");
    body["decision_on"] = json!("2026-11-01");
    body["reason"] = json!("decision");
    let (s, next) = post(&a, &format!("/profiles/alex/records/medication/{id}/correct"), body).await?;
    assert_eq!(s, StatusCode::CREATED, "{next}");
    let d = &next["detail"];
    assert_eq!(
        (
            &d["decision"],
            &d["decision_reason"],
            &d["decision_on"],
            &next["supersedes"]
        ),
        (
            &json!("do_not_repurchase"),
            // the person's reason is free text, so a read marks it
            &json!({"trust": "untrusted_text", "origin": "person_note", "text": "Too large to swallow"}),
            &json!("2026-11-01"),
            &json!(id)
        )
    );
    // the label follows the new version, and the old version keeps its own
    assert_eq!(next["labels"], json!([label["document_id"]]), "{next}");
    let (_, hist) = get(&a, "/profiles/alex/records/medication?history=true").await?;
    let versions: Vec<&Value> = hist["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["detail"]["name"] == json!("Vitamin D3"))
        .collect();
    assert_eq!(versions.len(), 2, "{hist}");
    assert!(
        versions.iter().all(|v| v["labels"] == json!([label["document_id"]])),
        "{versions:?}"
    );

    // a later correction (a new dose) carries the decision and the label on
    let mut body = next["detail"].clone();
    body["dose_text"] = json!("1 capsule a week");
    let (s, third) = post(
        &a,
        &format!("/profiles/alex/records/medication/{}/correct", id_of(&next)),
        body,
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{third}");
    assert_eq!(
        (&third["detail"]["decision"], &third["labels"]),
        (&json!("do_not_repurchase"), &json!([label["document_id"]]))
    );

    // a decision outside the three is refused, field by field
    for (bad, loc) in [
        (json!({"name": "X", "decision": "buy_more"}), json!(["decision"])),
        (json!({"name": "X", "decision_on": "soon"}), json!(["decision_on"])),
        (json!({"name": "X", "decision_reason": 3}), json!(["decision_reason"])),
    ] {
        let (s, r) = post(&a, "/profiles/alex/records/medication", bad).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{r}");
        assert_eq!(r["detail"]["errors"][0]["loc"], loc, "{r}");
    }
    Ok(())
}

#[tokio::test]
async fn deleting_a_profile_with_a_labelled_supplement_succeeds() -> R {
    let a = app().await?;
    let made = supplement(&a, "carmen").await?;
    let id = id_of(&made);
    let (s, label) = upload(
        &a,
        "alex",
        &format!("/profiles/carmen/records/medication/{id}/label"),
        "label.png",
        PNG,
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{label}");
    let mut body = made["detail"].clone();
    body["decision"] = json!("keep");
    let (s, _) = post(&a, &format!("/profiles/carmen/records/medication/{id}/correct"), body).await?;
    assert_eq!(s, StatusCode::CREATED);
    let pid = a.family.profile("carmen");
    assert_eq!(
        count(
            &a,
            "select count(*) from medication_label l join medication_statement m on m.id = l.medication_id \
             where m.profile_id = $1",
            pid
        )
        .await?,
        2,
        "both versions have the label"
    );

    let mut c = a.db.pool.acquire().await?;
    let deleted = delete_profile(&mut c, pid).await?;
    drop(c);
    assert_eq!(
        deleted.blobs.len(),
        1,
        "the label's bytes are freed: {:?}",
        deleted.blobs
    );
    for sql in [
        "select count(*) from medication_statement where profile_id = $1",
        "select count(*) from document where profile_id = $1",
    ] {
        assert_eq!(count(&a, sql, pid).await?, 0, "{sql}");
    }
    let links: i64 = sqlx::query_scalar("select count(*) from medication_label")
        .fetch_one(&a.db.pool)
        .await?;
    assert_eq!(links, 0);
    Ok(())
}

#[tokio::test]
async fn a_label_photo_past_the_limit_is_told_its_size_not_a_parse_error() -> R {
    let a = app().await?;
    let made = supplement(&a, "alex").await?;
    let uri = format!("/profiles/alex/records/medication/{}/label", id_of(&made));
    // a 20 MB phone photo: a PNG header, then padding
    let mut big = PNG.to_vec();
    big.resize(20 * 1024 * 1024, 0);
    let (s, out) = upload(&a, "alex", &uri, "label.png", &big).await?;
    assert_eq!(
        (s, &out["detail"]["code"]),
        (StatusCode::PAYLOAD_TOO_LARGE, &json!("too_large")),
        "{out}"
    );
    Ok(())
}

#[tokio::test]
async fn a_new_version_that_says_nothing_of_the_decision_keeps_the_persons_decision() -> R {
    // a source update (a FHIR re-import accepted from review) sends the medicine without the
    // decision columns: it goes through the same supersede, so the recorded decision stays
    let a = app().await?;
    let made = supplement(&a, "alex").await?;
    let mut body = made["detail"].clone();
    body["decision"] = json!("stop");
    body["decision_reason"] = json!("Upset stomach");
    body["decision_on"] = json!("2026-11-01");
    let (s, decided) = post(
        &a,
        &format!("/profiles/alex/records/medication/{}/correct", id_of(&made)),
        body,
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{decided}");
    let (s, updated) = post(
        &a,
        &format!("/profiles/alex/records/medication/{}/correct", id_of(&decided)),
        json!({"kind": "supplement", "name": "Vitamin D3", "strength": "4000 IU", "started": "2026-01-10"}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{updated}");
    let d = &updated["detail"];
    assert_eq!(
        (&d["strength"], &d["decision"], &d["decision_reason"], &d["decision_on"]),
        (
            &json!("4000 IU"),
            &json!("stop"),
            &json!({"trust": "untrusted_text", "origin": "person_note", "text": "Upset stomach"}),
            &json!("2026-11-01")
        ),
        "{updated}"
    );
    // the stored resource says it too, so the export keeps it
    let fhir: Value = sqlx::query_scalar("select fhir from medication_statement where id = $1")
        .bind(Uuid::parse_str(&id_of(&updated))?)
        .fetch_one(&a.db.pool)
        .await?;
    assert!(fhir.to_string().contains("urn:ownhealth:decision"), "{fhir}");
    Ok(())
}
