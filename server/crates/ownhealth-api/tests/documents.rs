//! Lane B routes (`tests/pdf/test_pdf_api.py`, `test_pdf_reupload.py`): the document vault REST API,
//! each test on its own throwaway database as a browser session.
//!
//! The report is a 3-page LabNorte report from the demo PDF writer, imported with lane A's
//! normalizer (the system as it ships); lane B's own tests cover the stand-in.

use std::sync::Arc;

use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_auth::tokens::{TokenIn, create_token, verify_token};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, StoredFile, create_document, dispatch_upload};
use ownhealth_core::jobs::{JobContext, run_one};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

struct App {
    _db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    fam: Family,
}

async fn app() -> anyhow::Result<App> {
    ownhealth_ingest_pdf::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    Ok(App {
        _db: db,
        _dir: dir,
        rt,
        fam,
    })
}

/// Alex's LabNorte report: 3 pages, one result still pending.
fn labnorte() -> Vec<u8> {
    let page1 = vec![
        LabRow::new("Hemoglobina", "14,2", "g/dL", "13,5 - 17,5"),
        LabRow::new("Hematocrito", "43,1", "%", "40,0 - 52,0"),
        LabRow::new("Leucocitos", "6,4", "x10^3/µL", "4,0 - 11,0"),
    ];
    let page2 = vec![
        LabRow::new("Glucosa", "92", "mg/dL", "70 - 110"),
        LabRow::new("Creatinina", "0,95", "mg/dL", "0,70 - 1,30"),
        LabRow::new("Vitamina D (25-OH)", "Pendiente", "", ""),
    ];
    let page3 = vec![
        LabRow::new("Colesterol total", "182", "mg/dL", "< 200"),
        LabRow::new("Triglicéridos", "160", "mg/dL", "< 150").flagged("H"),
    ];
    lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe de resultados de laboratorio",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id: "LN-20260914-AL1",
        pages: vec![page1, page2, page3],
    })
    .0
}

/// The LabNorte report imported for Alex, committed; returns the document id.
async fn imported(a: &App) -> anyhow::Result<Uuid> {
    let pdf = labnorte();
    let sha = a.rt.blobs.put_bytes(&pdf)?;
    let mut tx = a.rt.pool.begin().await?;
    let doc = create_document(
        &mut tx,
        &NewDocument {
            filename: Some("LabNorte_2026-09-14.pdf"),
            ..NewDocument::upload(
                a.fam.household_id,
                Some(a.fam.profile("alex")),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?
    .id;
    let s = ownhealth_ingest_pdf::import_pdf(&mut tx, &a.rt.blobs, doc, false).await?;
    tx.commit().await?;
    assert!(s.accepted >= 7, "{}", s.to_value());
    Ok(doc)
}

async fn cookie(a: &App, who: &str) -> anyhow::Result<String> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.fam.user(who), None, None, "passkey").await?;
    Ok(format!("{COOKIE}={secret}"))
}

struct Answer {
    status: StatusCode,
    headers: http::HeaderMap,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

async fn send(a: &App, req: Request<Body>) -> anyhow::Result<Answer> {
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let (status, headers) = (res.status(), res.headers().clone());
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 26).await?.to_vec();
    Ok(Answer { status, headers, bytes })
}

async fn get(a: &App, who: &str, uri: &str) -> anyhow::Result<Answer> {
    let req = Request::get(uri)
        .header(header::COOKIE, cookie(a, who).await?)
        .body(Body::empty())?;
    send(a, req).await
}

async fn post(a: &App, who: &str, uri: &str, body: Value) -> anyhow::Result<Answer> {
    let req = Request::post(uri)
        .header(header::COOKIE, cookie(a, who).await?)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))?;
    send(a, req).await
}

async fn audits(a: &App, tool: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar("select count(*) from audit_event where tool = $1")
        .bind(tool)
        .fetch_one(&a.rt.pool)
        .await?)
}

#[tokio::test]
async fn list_search_and_detail() -> R {
    let a = app().await?;
    let doc = imported(&a).await?;
    let tool = "GET /api/profiles/{p}/documents";
    let before = audits(&a, tool).await?;
    let r = get(&a, "alex", "/profiles/alex/documents").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    let items = r.json()["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        items.iter().map(|i| i["id"].clone()).collect::<Vec<_>>(),
        [json!(doc.to_string())]
    );
    let item = &items[0];
    assert_eq!(
        item["filename"],
        json!({"trust": "untrusted_text", "origin": "filename", "text": "LabNorte_2026-09-14.pdf"})
    );
    assert_eq!(
        (
            &item["doc_type"],
            &item["provider_name"],
            &item["pages"],
            &item["status"]
        ),
        (&json!("lab_report"), &json!("LabNorte"), &json!(3), &json!("active"))
    );
    assert!(item["flags"].get("metadata_text").is_none());
    let hit = get(&a, "alex", "/profiles/alex/documents?q=hemoglobina").await?.json()["items"].clone();
    assert_eq!(hit.as_array().map_or(0, Vec::len), 1, "{hit}");
    assert_eq!(hit[0]["snippet"]["trust"], "untrusted_text");
    assert!(
        hit[0]["snippet"]["text"].as_str().is_some_and(|t| t.contains("<b>")),
        "{hit}"
    );
    let miss = get(&a, "alex", "/profiles/alex/documents?q=colonoscopia").await?.json();
    assert_eq!(miss["items"], json!([]));
    assert_eq!(audits(&a, tool).await?, before + 3);

    let detail = get(&a, "alex", &format!("/documents/{doc}")).await?.json();
    let pages: Vec<Value> = detail["pages_info"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| p["page_no"].clone())
        .collect();
    assert_eq!(pages, [json!(1), json!(2), json!(3)]);
    assert_eq!(detail["mime"], "application/pdf");
    assert_eq!(detail["expected_tests"][0]["reason"]["text"], "Pendiente");
    assert_eq!(detail["expected_tests"][0]["printed_name"]["trust"], "untrusted_text");

    let too_long = get(&a, "alex", &format!("/profiles/alex/documents?q={}", "x".repeat(201))).await?;
    assert_eq!(too_long.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(too_long.json()["detail"][0]["type"], "string_too_long");
    Ok(())
}

#[tokio::test]
async fn file_page_png_and_words() -> R {
    let a = app().await?;
    let doc = imported(&a).await?;
    let f = get(&a, "alex", &format!("/documents/{doc}/file")).await?;
    assert_eq!(f.status, StatusCode::OK);
    assert!(f.bytes.starts_with(b"%PDF"));
    assert_eq!(f.headers[header::CONTENT_SECURITY_POLICY], "sandbox");
    assert_eq!(f.headers[header::CONTENT_TYPE], "application/pdf");
    assert_eq!(f.headers[header::CONTENT_DISPOSITION], "inline");
    let png = get(&a, "alex", &format!("/documents/{doc}/pages/2.png?scale=1")).await?;
    assert_eq!(png.status, StatusCode::OK);
    assert!(png.bytes.starts_with(b"\x89PNG"));
    assert_eq!(png.headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(
        get(&a, "alex", &format!("/documents/{doc}/pages/4.png")).await?.status,
        StatusCode::NOT_FOUND
    );
    let bad = get(&a, "alex", &format!("/documents/{doc}/pages/1.png?scale=9")).await?;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let words = get(&a, "alex", &format!("/documents/{doc}/pages/1/words"))
        .await?
        .json();
    assert_eq!(words["trust"], "untrusted_text");
    assert!(
        words["words"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|w| w["text"] == "LabNorte")
    );
    assert_eq!(
        get(&a, "alex", &format!("/documents/{doc}/pages/9/words"))
            .await?
            .status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

/// A PAT never reaches REST (`Caller` refuses it), so the scope rules are checked on the functions
/// the routes call, with the principal a token resolves to.
#[tokio::test]
async fn tokens_need_their_scopes_and_other_households_see_nothing() -> R {
    let a = app().await?;
    let doc = imported(&a).await?;
    let mut c = a.rt.pool.acquire().await?;
    let token = create_token(
        &mut c,
        &TokenIn {
            user_id: a.fam.user("alex"),
            label: "labs",
            profile_ids: vec![a.fam.profile("alex")],
            scopes: vec!["read:summary".into(), "read:labs".into()],
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "es",
            units: "si",
        },
    )
    .await?;
    let p = verify_token(&mut c, &token.plaintext).await?.expect("a valid token");
    let stranger = create_rivera_family(&mut c, "Strangers").await?;
    use ownhealth_api::routes::documents::{list_for, visible_document, words_for};
    assert!(visible_document(&mut c, &p, doc, None, false).await.is_ok());
    let err = words_for(&mut c, &a.rt.blobs, &p, doc, 1).await.expect_err("refused");
    assert_eq!(
        (err.status, err.code.as_str()),
        (StatusCode::FORBIDDEN, "insufficient_scope")
    );
    // without read:documents_text a search matches filenames only, and returns no snippet
    let list = async |c: &mut sqlx::PgConnection, q: &str| -> anyhow::Result<Value> {
        list_for(c, &p, "alex", Some(q), None, 100)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))
    };
    assert_eq!(list(&mut c, "hemoglobina").await?["items"], json!([]));
    let by_name = list(&mut c, "LabNorte_2026").await?;
    assert_eq!(by_name["items"][0]["snippet"], Value::Null);
    drop(c);
    let other_cookie = {
        let mut c = a.rt.pool.acquire().await?;
        let secret = create_session(&mut c, stranger.user("alex"), None, None, "passkey").await?;
        format!("{COOKIE}={secret}")
    };
    for url in [
        format!("/documents/{doc}"),
        format!("/documents/{doc}/file"),
        format!("/documents/{doc}/pages/1.png"),
    ] {
        let req = Request::get(&url)
            .header(header::COOKIE, &other_cookie)
            .body(Body::empty())?;
        assert_eq!(send(&a, req).await?.status, StatusCode::NOT_FOUND, "{url}");
    }
    Ok(())
}

#[tokio::test]
async fn assign_and_retract() -> R {
    let a = app().await?;
    let doc = imported(&a).await?;
    let r = post(
        &a,
        "alex",
        &format!("/documents/{doc}/assign"),
        json!({"profile": "lucia"}),
    )
    .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    assert_eq!(r.json()["profile"], "lucia");
    assert!(r.json()["job_id"].as_i64().is_some());
    let owner: Option<Uuid> = sqlx::query_scalar("select profile_id from document where id = $1")
        .bind(doc)
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(owner, Some(a.fam.profile("lucia")));
    let current: i64 =
        sqlx::query_scalar("select count(*) from observation where document_id = $1 and status = 'current'")
            .bind(doc)
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!(current, 0);
    let missing = post(&a, "alex", &format!("/documents/{doc}/assign"), json!({})).await?;
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
    // Sam, Lucía's guardian: a blank reason is refused
    let blank = post(&a, "sam", &format!("/documents/{doc}/retract"), json!({"reason": " "})).await?;
    assert_eq!(blank.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", blank.json());
    let r = post(
        &a,
        "alex",
        &format!("/documents/{doc}/retract"),
        json!({"reason": "wrong patient"}),
    )
    .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    assert_eq!(r.json()["status"], "retracted");
    let again = post(
        &a,
        "alex",
        &format!("/documents/{doc}/retract"),
        json!({"reason": "again"}),
    )
    .await?;
    assert_eq!(again.status, StatusCode::CONFLICT);
    Ok(())
}

async fn upload(a: &App, pdf: &[u8]) -> anyhow::Result<Value> {
    let sha = a.rt.blobs.put_bytes(pdf)?;
    let mut tx = a.rt.pool.begin().await?;
    let out = dispatch_upload(
        &mut tx,
        &a.rt.blobs,
        a.fam.household_id,
        a.fam.profile("alex"),
        &[StoredFile {
            filename: "LabNorte_2026-09-14.pdf".into(),
            sha256: sha,
            size: pdf.len() as i64,
        }],
        None,
    )
    .await?;
    tx.commit().await?;
    let items = out["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1);
    Ok(items[0].clone())
}

async fn drain(a: &App) -> R {
    let ctx = JobContext {
        pool: a.rt.pool.clone(),
        settings: Arc::clone(&a.rt.settings),
        blobs: a.rt.blobs.clone(),
        worker_id: "test".into(),
    };
    let kinds = ["import.pdf".to_string()];
    for _ in 0..20 {
        if run_one(&ctx, "core", Some(&kinds)).await?.is_none() {
            return Ok(());
        }
    }
    anyhow::bail!("the import.pdf queue did not drain in 20 runs")
}

async fn state(a: &App, doc: Uuid) -> anyhow::Result<(String, i64)> {
    let status: String = sqlx::query_scalar("select status from document where id = $1")
        .bind(doc)
        .fetch_one(&a.rt.pool)
        .await?;
    let current: i64 = sqlx::query_scalar(
        "select count(*) from observation where document_id = $1 and status in ('current', 'provisional')",
    )
    .bind(doc)
    .fetch_one(&a.rt.pool)
    .await?;
    Ok((status, current))
}

/// Uploading a file again after its document was retracted brings the document and its values back.
#[tokio::test]
async fn reupload_after_retract_brings_the_document_back() -> R {
    let a = app().await?;
    let pdf = labnorte();
    let first = upload(&a, &pdf).await?;
    drain(&a).await?;
    let doc: Uuid = first["document_id"].as_str().unwrap_or("").parse()?;
    let (status, imported) = state(&a, doc).await?;
    assert!(status == "active" && imported > 0);

    let r = post(
        &a,
        "alex",
        &format!("/documents/{doc}/retract"),
        json!({"reason": "uploaded twice"}),
    )
    .await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    assert_eq!(
        r.json()["retracted_observations"].as_array().map_or(0, Vec::len) as i64,
        imported
    );
    assert_eq!(state(&a, doc).await?, ("retracted".to_string(), 0));

    let again = upload(&a, &pdf).await?;
    assert_eq!(again["document_id"], json!(doc.to_string()));
    assert!(
        again["job_id"].as_i64().is_some() && again["duplicate"] != true,
        "{again}"
    );
    drain(&a).await?;
    assert_eq!(state(&a, doc).await?, ("active".to_string(), imported));
    Ok(())
}

// ------------------------------------------- sensitive documents (test_document_sensitive.py) ----

const SLIP: &[u8] = b"%PDF-1.4\n% synthetic LabNorte slip\n%%EOF\n";

/// A one-page LabNorte slip for `who` holding one result with `sensitivity`.
async fn doc_with(a: &App, who: &str, sensitivity: &[&str]) -> anyhow::Result<Uuid> {
    use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
    use ownhealth_core::provenance::{SrcIn, SrcRef};
    let pid = a.fam.profile(who);
    let bytes = [SLIP, format!("{sensitivity:?}").as_bytes()].concat();
    let sha = a.rt.blobs.put_bytes(&bytes)?;
    let mut tx = a.rt.pool.begin().await?;
    let doc = create_document(
        &mut tx,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            doc_type: Some("lab_report"),
            ..NewDocument::upload(
                a.fam.household_id,
                Some(pid),
                &sha,
                "application/pdf",
                SLIP.len() as i64,
            )
        },
    )
    .await?
    .id;
    let obs = ObservationIn::from_json(json!({
        "profile_id": pid, "printed_name": "VIH Ac", "printed_value": "97", "printed_unit": "mg/dL",
        "printed_ref_range": "< 130", "value_num": "97", "ref_high": "130", "loinc": "13457-7",
        "alias_group": "ldl_c", "unit_ucum": "mg/dL", "value_canonical": "97", "unit_canonical": "mg/dL",
        "derivation_version": "test", "effective_at": "2026-09-14T08:00:00+00:00",
        "document_id": doc, "sensitivity": sensitivity,
    }))?;
    let mut src = SrcIn::new("manual");
    src.profile_id = Some(pid);
    src.actor_kind = "system".into();
    commit_observation(&mut tx, &obs, &SrcRef::New(src), CommitOptions::new("import")).await?;
    tx.commit().await?;
    Ok(doc)
}

async fn page_text(a: &App, doc: Uuid, text: &str) -> R {
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, text_content, fts) \
         values ($1, 1, 595, 842, true, $2, to_tsvector('simple', $2))",
    )
    .bind(doc)
    .bind(text)
    .execute(&a.rt.pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn caregiver_cannot_fetch_a_sensitive_document() -> R {
    let a = app().await?;
    // Alex is caregiver on Carmen: read:labs, no read:sensitive
    let sensitive = doc_with(&a, "carmen", &["sexual_health"]).await?;
    let plain = doc_with(&a, "carmen", &[]).await?;
    let mut c = a.rt.pool.acquire().await?;
    assert!(ownhealth_core::documents::document_is_sensitive(&mut c, sensitive).await?);
    assert!(!ownhealth_core::documents::document_is_sensitive(&mut c, plain).await?);
    let r = get(&a, "alex", &format!("/documents/{sensitive}/file")).await?;
    assert_eq!(
        (r.status, &r.json()["detail"]["code"]),
        (StatusCode::FORBIDDEN, &json!("insufficient_scope")),
        "{}",
        r.json()
    );
    let r = get(&a, "alex", &format!("/documents/{sensitive}/pages/1.png")).await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = get(&a, "alex", &format!("/documents/{plain}/file")).await?;
    assert_eq!(r.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn search_without_read_sensitive_neither_matches_nor_quotes_a_sensitive_page() -> R {
    let a = app().await?;
    let sensitive = doc_with(&a, "carmen", &["sexual_health"]).await?;
    let plain = doc_with(&a, "carmen", &[]).await?;
    for d in [sensitive, plain] {
        page_text(&a, d, "LabNorte serologia VIH resultado reactivo").await?;
    }
    // Alex, caregiver on Carmen, is given read:documents_text but not read:sensitive
    sqlx::query(
        "update profile_grant set categories = array_append(categories, 'read:documents_text') \
         where user_id = $1 and profile_id = $2",
    )
    .bind(a.fam.user("alex"))
    .bind(a.fam.profile("carmen"))
    .execute(&a.rt.pool)
    .await?;
    let items = get(&a, "alex", "/profiles/carmen/documents?q=serologia").await?.json()["items"].clone();
    assert_eq!(items.as_array().map(|i| i.len()), Some(1), "{items}");
    assert_eq!(items[0]["id"], json!(plain.to_string()));
    let snippet = items[0]["snippet"]["text"].as_str().unwrap_or_default().to_lowercase();
    assert!(snippet.contains("serologia"), "{items}");
    // the filename still matches every document, without a snippet from the sensitive one
    let items = get(&a, "alex", "/profiles/carmen/documents?q=labnorte").await?.json()["items"].clone();
    let by_id: std::collections::HashMap<String, Value> = items
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| (i["id"].as_str().unwrap_or_default().to_string(), i.clone()))
        .collect();
    let ids: std::collections::BTreeSet<&String> = by_id.keys().collect();
    assert_eq!(ids, [&sensitive.to_string(), &plain.to_string()].into_iter().collect());
    assert_eq!(by_id[&sensitive.to_string()]["snippet"], Value::Null);
    // the owner of the data (Alex on his own profile has read:sensitive) is unaffected
    let mine = doc_with(&a, "alex", &["sexual_health"]).await?;
    page_text(&a, mine, "LabNorte serologia VIH").await?;
    let items = get(&a, "alex", "/profiles/alex/documents?q=serologia").await?.json()["items"].clone();
    assert_eq!(items.as_array().map(|i| i.len()), Some(1), "{items}");
    assert_eq!(items[0]["id"], json!(mine.to_string()));
    assert_ne!(items[0]["snippet"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn document_bytes_are_never_kept_in_the_browser_cache() -> R {
    let a = app().await?;
    let doc = doc_with(&a, "alex", &[]).await?;
    let r = get(&a, "alex", &format!("/documents/{doc}/file")).await?;
    assert_eq!(r.status, StatusCode::OK);
    let cache = r
        .headers
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(cache.contains("no-store"), "{cache}");
    // the bytes stream from the blob with their length up front
    let len = r
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(len, r.bytes.len().to_string());
    assert!(r.bytes.starts_with(SLIP));
    Ok(())
}
