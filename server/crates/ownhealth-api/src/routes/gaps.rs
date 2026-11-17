//! Document gaps (`rules::gaps`): visits with no report on file and long stretches with no records,
//! computed on read and never stored, and the letter that asks a provider for a copy.
//!
//! ```text
//! GET /api/profiles/{p}/gaps?as_of=&lang=               read:records
//! GET /api/profiles/{p}/gaps/{key}/letter.pdf?lang=     read:records
//! ```
//!
//! The first answers the ownhealth.gaps/v1 document with the words in `lang`; a hole whose edge is a
//! lab value also needs read:labs (what the scopes leave out is counted in `withheld`). The second
//! prints the access-request letter of a visit gap open today, for a profile whose country has one
//! (Spain: GDPR Art. 15 and Ley 41/2002 art. 18; the other EU and EEA countries: GDPR Art. 15). It
//! takes the language and nothing else: the identity number and the address print as blank lines,
//! so they are never sent or stored. The audit row names the gap key and the visit's src id, never
//! the name or the provider.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use ownhealth_core::Runtime;
use ownhealth_core::access::today_utc;
use ownhealth_core::audit::write_audit;
use ownhealth_rules::gaps::{self, LANGS};
use serde_json::{Value, json};

use super::auth::{Check, R, ok};
use super::status::{event, parse_date};
use crate::error::ApiError;
use crate::extract::Caller;

fn lang_of<'a>(q: &'a HashMap<String, String>, check: &mut Check) -> Option<&'a str> {
    let lang = q.get("lang").filter(|v| !v.is_empty()).map(String::as_str);
    check.literal("lang", lang, &LANGS);
    lang
}

async fn get_gaps(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let mut check = Check::query();
    let as_of = parse_date(&q, &mut check);
    let lang = lang_of(&q, &mut check);
    check.done()?;
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(c.clone())
        .require(&mut tx, &p, Some("read:records"), None)
        .await?;
    let doc = gaps::document(&mut tx, &perm.profile, as_of.unwrap_or_else(today_utc)).await?;
    let mut e = event(&c, perm.profile.id, "GET /api/profiles/{p}/gaps");
    e.resource_type = Some("gaps".into());
    e.resource_ids = Some(vec![doc["gaps_hash"].as_str().unwrap_or_default().to_string()]);
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    ok(gaps::render(&doc, lang.unwrap_or(&c.lang), &perm.scopes)?)
}

async fn get_letter(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path((p, key)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    // access first, so an input check never answers for a profile the caller cannot read
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(c.clone())
        .require(&mut tx, &p, Some("read:records"), None)
        .await?;
    let mut check = Check::query();
    let lang = lang_of(&q, &mut check).unwrap_or(if c.lang == "es" { "es" } else { "en" });
    // the language is the only input: nothing a person types reaches the letter or the audit row
    let mut extra: Vec<&String> = q.keys().filter(|k| *k != "lang").collect();
    extra.sort();
    for k in extra {
        check.push("extra_forbidden", k, "Extra inputs are not permitted", json!(q[k]));
    }
    check.done()?;
    let g = gaps::load()?;
    let doc = gaps::document(&mut tx, &perm.profile, today_utc()).await?;
    let unknown = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown",
            "No such gap with a letter for this profile.",
        )
    };
    let item = doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["key"].as_str() == Some(key.as_str()))
        .cloned()
        .ok_or_else(unknown)?;
    let letter = item["letter"].as_str().and_then(|l| g.letter(l)).ok_or_else(unknown)?;
    let text = |k: &str| item[k].as_str().unwrap_or_default().to_string();
    let (provider, date) = (text("provider"), text("date"));
    let pdf = ownhealth_ops::letter_pdf::render(
        g,
        letter,
        &ownhealth_ops::letter_pdf::LetterIn {
            lang,
            name: &perm.profile.display_name,
            provider: &provider,
            date: &date,
        },
    );
    let srcs: Vec<String> = item["src"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let mut e = event(&c, perm.profile.id, "GET /api/profiles/{p}/gaps/{key}/letter.pdf");
    e.via = "export".into();
    e.action = "export".into();
    e.resource_type = Some("gap_letter".into());
    e.resource_ids = Some(vec![key.clone()]);
    e.rows_out = Some(1);
    e.src_ids = Some(srcs);
    e.bytes_out = Some(i32::try_from(pdf.len()).unwrap_or(i32::MAX));
    e.args_summary = Some(format!("letter={};lang={lang}", letter.id));
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    let filename = format!("ownhealth-letter-{}-{date}-{lang}.pdf", perm.profile.slug);
    let mut res = pdf.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/gaps", get(get_gaps))
        .route("/profiles/{p}/gaps/{key}/letter.pdf", get(get_letter))
}
