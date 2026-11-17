//! `GET /api/profiles/{p}/gaps` and `GET /api/profiles/{p}/gaps/{key}/letter.pdf`: a visit typed by
//! hand with no report on file is a gap with its words and src, its letter prints for a country that
//! has one, the download is audited without the name or the provider, and nothing in the request or
//! the schema holds an identity number.

mod lane_i;

use axum::body::Body;
use http::{Request, StatusCode, header};
use lane_i::{App, R, app};
use serde_json::{Value, json};

async fn visit(a: &App) -> anyhow::Result<String> {
    let body = json!({"period_start": "2025-11-10T09:30:00Z", "reason_text": "Right knee pain",
                      "provider_name": "Traumatología · Hospital Norte"});
    let (s, v) = a.post("alex", "/profiles/alex/records/visit", Some(body)).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, doc) = a.get("alex", "/profiles/alex/gaps?lang=es&as_of=2026-11-07").await?;
    assert_eq!(s, StatusCode::OK, "{doc}");
    Ok(doc["items"][0]["key"].as_str().unwrap_or_default().to_string())
}

async fn letter(a: &App, uri: &str) -> anyhow::Result<(StatusCode, http::HeaderMap, Vec<u8>)> {
    let req = Request::get(uri)
        .header(header::COOKIE, a.cookie("alex").await?)
        .body(Body::empty())?;
    a.send_raw(req).await
}

#[tokio::test]
async fn a_visit_without_a_report_is_a_gap_with_its_words_and_src() -> R {
    let a = app().await?;
    let key = visit(&a).await?;
    let (s, doc) = a.get("alex", "/profiles/alex/gaps?lang=es&as_of=2026-11-07").await?;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(doc["schema"], "ownhealth.gaps/v1");
    assert!(
        doc["gaps_hash"].as_str().is_some_and(|h| h.starts_with("sha256:")),
        "{doc}"
    );
    let items = doc["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "{doc:#}");
    let it = &items[0];
    assert!(key.starts_with("visit:2025-11-10:"), "{key}");
    assert_eq!(
        it["title"],
        "La visita del 2025-11-10 (Traumatología · Hospital Norte) no tiene informe archivado"
    );
    assert_eq!(it["letter"], "spain");
    assert!(it["src"][0].as_str().is_some_and(|s| s.starts_with("src_")), "{it}");
    assert_eq!(it["cite"], "https://eur-lex.europa.eu/eli/reg/2016/679/oj#art_15");
    let (s, v) = a.get("alex", "/profiles/alex/gaps?lang=fr").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // the audit row names the document's hash, never a provider
    let n = a
        .count("select count(*) from audit_event where tool = 'GET /api/profiles/{p}/gaps' and resource_type = 'gaps'")
        .await?;
    assert!(n >= 2, "{n}");
    Ok(())
}

#[tokio::test]
async fn the_letter_prints_and_is_audited_without_the_name_or_the_provider() -> R {
    let a = app().await?;
    let key = visit(&a).await?;
    for lang in ["es", "en"] {
        let (s, h, pdf) = letter(&a, &format!("/profiles/alex/gaps/{key}/letter.pdf?lang={lang}")).await?;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&pdf));
        assert_eq!(h[header::CONTENT_TYPE], "application/pdf");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert!(pdf.starts_with(b"%PDF-"));
    }
    type Row = (
        String,
        String,
        Option<String>,
        Option<Vec<String>>,
        Option<Vec<String>>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select via, action, resource_type, resource_ids, src_ids, args_summary from audit_event \
         where tool = 'GET /api/profiles/{p}/gaps/{key}/letter.pdf' order by id",
    )
    .fetch_all(&a.rt.pool)
    .await?;
    assert_eq!(rows.len(), 2);
    let (via, action, kind, ids, srcs, args) = &rows[0];
    assert_eq!(
        (via.as_str(), action.as_str(), kind.as_deref()),
        ("export", "export", Some("gap_letter"))
    );
    assert_eq!(ids.as_deref(), Some(&[key.clone()][..]));
    assert_eq!(srcs.as_ref().map(Vec::len), Some(1));
    assert_eq!(args.as_deref(), Some("letter=spain;lang=es"));
    // no payload: neither the patient's name nor the provider is anywhere in the audit log
    let leaked = a
        .count(
            "select count(*) from audit_event where row_to_json(audit_event)::text ilike '%Hospital Norte%' \
             or row_to_json(audit_event)::text ilike '%Rivera%'",
        )
        .await?;
    assert_eq!(leaked, 0);
    Ok(())
}

/// No identity number exists anywhere: the letter takes the language and nothing else, and no column
/// of the schema holds one.
#[tokio::test]
async fn no_identity_number_is_ever_sent_or_stored() -> R {
    let a = app().await?;
    let key = visit(&a).await?;
    let (s, _, body) = letter(
        &a,
        &format!("/profiles/alex/gaps/{key}/letter.pdf?lang=es&id_number=00000000T"),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let v: Value = serde_json::from_slice(&body)?;
    assert_eq!(v["detail"][0]["type"], "extra_forbidden", "{v}");
    let (s, _, _) = letter(
        &a,
        &format!("/profiles/alex/gaps/{key}/letter.pdf?address=Calle%20Falsa"),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let columns = a
        .count(
            "select count(*) from information_schema.columns where table_schema = 'public' and column_name ~* \
             '(^|_)(dni|nie|nif|passport|id_number|identity_number|document_number|national_id|address)($|_)'",
        )
        .await?;
    assert_eq!(columns, 0, "a column for an identity number or an address");
    Ok(())
}

#[tokio::test]
async fn no_letter_for_an_unknown_gap_or_a_country_without_one() -> R {
    let a = app().await?;
    let key = visit(&a).await?;
    let (s, _, _) = letter(&a, "/profiles/alex/gaps/visit:2020-01-01:00000000/letter.pdf").await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    sqlx::query("update profile set country = 'US' where id = $1")
        .bind(a.family.profile("alex"))
        .execute(&a.rt.pool)
        .await?;
    let (_, doc) = a.get("alex", "/profiles/alex/gaps?lang=en").await?;
    assert_eq!(doc["items"][0]["key"], key.as_str(), "the gap stays");
    assert!(doc["items"][0].get("letter").is_none(), "{doc}");
    let (s, _, _) = letter(&a, &format!("/profiles/alex/gaps/{key}/letter.pdf")).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}
