//! The next-draw planner (`rules::next_draw`): what is worth asking for at the next blood draw, and
//! the same list as a printable request for the doctor. Computed on read, never stored.
//!
//! ```text
//! GET /api/profiles/{p}/next-draw?as_of=&lang=                    read:labs
//! GET /api/profiles/{p}/next-draw.pdf?as_of=&lang=&hide_name=     read:labs
//! ```
//!
//! Every line is a lab value or a lab gap, so both need read:labs. `hide_name=1` prints a blank for
//! the name and leaves the birth date out. One audit row per call names the state hash; the PDF's row
//! (`via = export`) also names every src id printed.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::IntoResponse;
use axum::routing::get;
use chrono::NaiveDate;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, today_utc};
use ownhealth_core::audit::write_audit;
use ownhealth_rules::next_draw;
use serde_json::Value;

use super::auth::{Check, Meta, R, ok};
use super::status::{event, parse_date};
use crate::extract::Caller;

const ROUTE: &str = "GET /api/profiles/{p}/next-draw";
const ROUTE_PDF: &str = "GET /api/profiles/{p}/next-draw.pdf";

/// The plan in one language, with the permission it was read under (not yet audited or committed).
async fn rendered(
    tx: &mut sqlx::PgConnection,
    principal: &Principal,
    p: &str,
    as_of: NaiveDate,
    lang: &str,
) -> R<(Value, Permission)> {
    let perm = Caller(principal.clone())
        .require(&mut *tx, p, Some("read:labs"), None)
        .await?;
    let plan = next_draw::build(&mut *tx, perm.profile.id, as_of).await?;
    let pack = ownhealth_rules::load_pack().map_err(ownhealth_core::Error::from)?;
    Ok((next_draw::render(&plan, &pack, lang)?, perm))
}

pub async fn plan(
    rt: &Runtime,
    principal: &Principal,
    p: &str,
    as_of: Option<NaiveDate>,
    lang: Option<&str>,
) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let lang = lang.unwrap_or(&principal.lang);
    let (out, perm) = rendered(&mut tx, principal, p, as_of.unwrap_or_else(today_utc), lang).await?;
    let mut e = event(principal, perm.profile.id, ROUTE);
    e.resource_type = Some("next_draw".into());
    e.resource_ids = Some(vec![out["state_hash"].as_str().unwrap_or_default().to_string()]);
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    Ok(out)
}

struct Params {
    as_of: Option<NaiveDate>,
    lang: Option<String>,
    hide_name: bool,
}

fn params(q: &HashMap<String, String>) -> R<Params> {
    let mut check = Check::query();
    let as_of = parse_date(q, &mut check);
    let hide = q.get("hide_name").map(String::as_str);
    check.literal("hide_name", hide, &["0", "1", "true", "false"]);
    check.done()?;
    Ok(Params {
        as_of,
        lang: q.get("lang").filter(|v| !v.is_empty()).cloned(),
        hide_name: matches!(hide, Some("1" | "true")),
    })
}

async fn get_plan(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let q = params(&q)?;
    ok(plan(&rt, &c, &p, q.as_of, q.lang.as_deref()).await?)
}

async fn get_pdf(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    meta: Meta,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let q = params(&q)?;
    let as_of = q.as_of.unwrap_or_else(today_utc);
    let lang = if q.lang.as_deref().unwrap_or(&c.lang) == "es" {
        "es"
    } else {
        "en"
    };
    let mut tx = rt.pool.begin().await?;
    let (mut model, perm) = rendered(&mut tx, &c, &p, as_of, lang).await?;
    model["person"] = next_draw::person(&perm.profile, as_of, lang, q.hide_name)?;
    let sheet = ownhealth_ops::next_draw_pdf::render(&model);
    let mut e = event(&c, perm.profile.id, ROUTE_PDF);
    e.via = "export".into();
    e.action = "export".into();
    e.resource_type = Some("next_draw".into());
    e.resource_ids = Some(vec![model["state_hash"].as_str().unwrap_or_default().to_string()]);
    e.rows_out = Some(i32::try_from(sheet.printed.len()).unwrap_or(i32::MAX));
    e.src_ids = Some(sheet.printed);
    e.bytes_out = Some(i32::try_from(sheet.pdf.len()).unwrap_or(i32::MAX));
    e.args_summary = Some(format!(
        "lang={lang};hide_name={};pages={}",
        u8::from(q.hide_name),
        sheet.pages
    ));
    e.ip = meta.ip.clone();
    write_audit(&mut tx, &e).await?;
    tx.commit().await?;
    let filename = format!(
        "ownhealth-next-draw-{}-{lang}-{}.pdf",
        if q.hide_name { "unnamed" } else { &perm.profile.slug },
        as_of.format("%Y-%m-%d")
    );
    let mut res = sheet.pdf.into_response();
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
        .route("/profiles/{p}/next-draw", get(get_plan))
        .route("/profiles/{p}/next-draw.pdf", get(get_pdf))
}
