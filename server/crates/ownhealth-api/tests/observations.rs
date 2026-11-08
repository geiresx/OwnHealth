//! Lane A routes (tests/normalize/test_norm_api.py): observations, trend, aliases. Each test runs
//! the router on its own throwaway database (scripts/test-db.sh) as a browser session.

use axum::Router;
use axum::body::Body;
use chrono::{TimeZone, Utc};
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::observations::{CommitOptions, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::testing::TestDb;
use ownhealth_normalize::{RowIn, normalize_row};
use serde_json::{Value, json};
use sqlx::PgConnection;
use tower::ServiceExt;

type R = anyhow::Result<()>;

const ROWS: [(&str, &str, &str, &str, &str); 4] = [
    ("2024-03-02", "Colesterol LDL", "118", "mg/dL", "< 130"),
    ("2025-01-20", "Colesterol LDL directo", "84", "mg/dL", "< 130"),
    ("2026-09-14", "Colesterol LDL", "3,4", "mmol/L", "< 3,36"),
    ("2026-09-14", "Ferritina", "120", "ng/mL", "30 - 400"),
];

struct App {
    _db: TestDb,
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
        _db: db,
        _dir: dir,
        rt,
        family,
    })
}

async fn row(c: &mut PgConnection, f: &Family, day: &str, name: &str, value: &str, unit: &str, rng: &str) -> R {
    let mut parts = day.split('-').map(|x| x.parse::<u32>().unwrap_or(1));
    let (y, m, d) = (
        parts.next().unwrap_or(1),
        parts.next().unwrap_or(1),
        parts.next().unwrap_or(1),
    );
    let when = Utc
        .with_ymd_and_hms(y as i32, m, d, 8, 0, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("date"))?;
    let n = normalize_row(
        c,
        &RowIn {
            profile_id: f.profile("alex"),
            printed_name: name,
            printed_value: value,
            printed_unit: Some(unit),
            printed_range: Some(rng),
            printed_flag: None,
            effective_at: when.fixed_offset(),
            lang: "es",
            specimen: None,
            provider: None,
            loinc: None,
            method: None,
            document_id: None,
        },
    )
    .await?;
    let mut src = SrcIn::manual(None);
    src.label = Some(format!("LabNorte {day}"));
    commit_observation(c, &n.observation, &SrcRef::New(src), CommitOptions::new("person")).await?;
    Ok(())
}

async fn seed(a: &App) -> R {
    let mut tx = a.rt.pool.begin().await?;
    for (day, name, value, unit, rng) in ROWS {
        row(&mut tx, &a.family, day, name, value, unit, rng).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A browser session for one of the family's users.
async fn cookie(a: &App, who: &str) -> anyhow::Result<String> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    Ok(format!("{COOKIE}={secret}"))
}

fn api(a: &App) -> Router {
    ownhealth_api::router().with_state(a.rt.clone())
}

async fn send(a: &App, req: Request<Body>) -> anyhow::Result<(StatusCode, Value)> {
    let res = api(a).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 24).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

async fn get(a: &App, who: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
    let req = Request::get(uri)
        .header(header::COOKIE, cookie(a, who).await?)
        .body(Body::empty())?;
    send(a, req).await
}

async fn post(a: &App, who: &str, uri: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
    let req = Request::post(uri)
        .header(header::COOKIE, cookie(a, who).await?)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))?;
    send(a, req).await
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

#[tokio::test]
async fn test_observations_as_reported_and_converted() -> R {
    let a = app().await?;
    seed(&a).await?;
    let (s, body) = get(&a, "alex", "/profiles/alex/observations?alias_group=ldl_c").await?;
    assert_eq!(s, StatusCode::OK, "{body}");
    let items = body["items"].as_array().cloned().unwrap_or_default();
    let got: Vec<Value> = items
        .iter()
        .map(|i| json!([i["date"], i["value"], i["unit"]]))
        .collect();
    assert_eq!(
        got,
        vec![
            json!(["2026-09-14", 3.4, "mmol/L"]),
            json!(["2025-01-20", 84.0, "mg/dL"]),
            json!(["2024-03-02", 118.0, "mg/dL"]),
        ]
    );
    assert_eq!(items[0]["printed"]["value"], "3,4");
    assert_eq!(items[0]["src_label"], "LabNorte 2026-09-14");
    assert_eq!(items[0]["interpretation"], "H");
    assert_eq!(items[0]["name"], "Colesterol LDL");
    let (_, conv) = get(
        &a,
        "alex",
        "/profiles/alex/observations?alias_group=ldl_c&units=conventional",
    )
    .await?;
    assert_eq!(round1(conv["items"][0]["value"].as_f64().unwrap_or_default()), 131.5);
    let (_, si) = get(&a, "alex", "/profiles/alex/observations?q=LDL%20cholesterol&units=si").await?;
    let si = si["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(si.len(), 3);
    assert!(si.iter().all(|i| i["unit"] == "mmol/L"), "{si:?}");
    let n: i64 =
        sqlx::query_scalar("select count(*) from audit_event where resource_type = 'observation' and profile_id = $1")
            .bind(a.family.profile("alex"))
            .fetch_one(&a.rt.pool)
            .await?;
    assert_eq!(n, 3);
    Ok(())
}

#[tokio::test]
async fn test_trend() -> R {
    let a = app().await?;
    seed(&a).await?;
    let (s, t) = get(&a, "alex", "/profiles/alex/trend?code=LOINC:13457-7&window=P5Y&lang=en").await?;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert!(t["alias_group"] == "ldl_c" && t["unit"] == "mg/dL", "{t}");
    let points = t["points"].as_array().cloned().unwrap_or_default();
    let values: Vec<f64> = points.iter().filter_map(|p| p["value"].as_f64()).collect();
    assert_eq!(&values[..2], &[118.0, 84.0]);
    assert_eq!(round1(values[2]), 131.5);
    assert_eq!(points[2]["printed"], "3,4 mmol/L");
    assert_eq!(t["codes"], json!(["LOINC:13457-7", "LOINC:18262-6", "LOINC:39469-2"]));
    let lim: Vec<String> = t["limitations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    assert!(lim.iter().any(|x| x.contains("Direct assay")), "{lim:?}");
    assert!(lim.iter().any(|x| x.contains("molar mass of cholesterol")), "{lim:?}");
    let (_, es) = get(&a, "alex", "/profiles/alex/trend?code=LOINC:13457-7&window=P5Y&lang=es").await?;
    let es: Vec<String> = es["limitations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    assert!(es.iter().any(|x| x.contains("masa molar de cholesterol")), "{es:?}");
    assert!(!es.iter().any(|x| x.contains("are ") || x.contains("Labs ")), "{es:?}");
    let st = &t["stats"];
    let last = values[2];
    let pct = st["last_vs_previous_pct"].as_f64().unwrap_or_default();
    assert_eq!(st["n"], 3);
    assert_eq!(pct, round1((last - 84.0) / 84.0 * 100.0));
    assert_ne!(pct, pct.round()); // one decimal, not a whole number
    let rcv = st["rcv_pct"].as_f64().unwrap_or_default();
    assert!(rcv > 0.0 && st["exceeds_rcv"] == json!(pct.abs() > rcv), "{st}");
    let statuses: Vec<&Value> = points.iter().map(|p| &p["status"]).collect();
    // lane E's word as of each point's day
    assert!(
        statuses
            .iter()
            .all(|x| ["ok", "watch", "out", "stale", "nodata"].contains(&x.as_str().unwrap_or("?"))),
        "{statuses:?}"
    );
    let (_, one) = get(&a, "alex", "/profiles/alex/trend?code=ldl_c&window=P1Y").await?;
    assert_eq!(one["stats"]["n"], 1);
    let (s, _) = get(&a, "alex", "/profiles/alex/trend?code=ldl_c&window=bad").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = get(&a, "alex", "/profiles/alex/trend?code=zzz%20qqq").await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_scopes_are_enforced() -> R {
    let a = app().await?;
    seed(&a).await?;
    // a token with read:summary only: REST refuses tokens outright
    let token = ownhealth_auth::tokens::create_token(
        &mut *a.rt.pool.acquire().await?,
        &ownhealth_auth::tokens::TokenIn {
            user_id: a.family.user("alex"),
            label: "summary only",
            profile_ids: vec![a.family.profile("alex")],
            scopes: vec!["read:summary".into()],
            default_profile: None,
            days: 30,
            identity_mode: "full",
            lang: "es",
            units: "si",
        },
    )
    .await
    .ok()
    .map(|t| t.plaintext);
    if let Some(token) = token {
        for uri in ["/profiles/alex/observations", "/profiles/alex/trend?code=ldl_c"] {
            let req = Request::get(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())?;
            assert_eq!(send(&a, req).await?.0, StatusCode::FORBIDDEN, "{uri}");
        }
    }
    // a viewer on Alex: read:summary only
    let (s, _) = get(&a, "sam", "/profiles/alex/observations").await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_add_alias_relinks() -> R {
    let a = app().await?;
    {
        let mut tx = a.rt.pool.begin().await?;
        row(
            &mut tx,
            &a.family,
            "2026-09-14",
            "Coles LDL calc.",
            "97",
            "mg/dL",
            "< 130",
        )
        .await?;
        tx.commit().await?;
    }
    let body = json!({"profile": "alex", "alias": "Coles LDL calc.", "alias_group": "ldl_c", "lang": "es", "loinc": "13457-7"});
    let (s, r) = post(&a, "alex", "/aliases", body).await?;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["rederived"], 1);
    let (_, items) = get(&a, "alex", "/profiles/alex/observations?alias_group=ldl_c").await?;
    let loincs: Vec<&Value> = items["items"]
        .as_array()
        .map(|v| v.iter().map(|i| &i["loinc"]).collect())
        .unwrap_or_default();
    assert_eq!(loincs, vec![&json!("13457-7")]);
    let (s, bad) = post(
        &a,
        "alex",
        "/aliases",
        json!({"profile": "alex", "alias": "x", "alias_group": "nope"}),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad["detail"]["code"], "bad_input");
    let (s, _) = post(
        &a,
        "sam",
        "/aliases",
        json!({"profile": "alex", "alias": "y", "alias_group": "ldl_c"}),
    )
    .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_a_window_too_large_for_the_calendar_is_refused() -> R {
    let a = app().await?;
    seed(&a).await?;
    for window in ["P99999Y", "P9999999D"] {
        let (s, r) = get(&a, "alex", &format!("/profiles/alex/trend?code=ldl_c&window={window}")).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{window}");
        assert_eq!(r["detail"]["code"], "bad_input", "{window} {r}");
    }
    Ok(())
}

#[tokio::test]
async fn test_browser_reads_and_alias_additions_are_audited_as_ui() -> R {
    let a = app().await?;
    seed(&a).await?;
    assert_eq!(get(&a, "alex", "/profiles/alex/observations").await?.0, StatusCode::OK);
    let (s, r) = post(
        &a,
        "alex",
        "/aliases",
        json!({"profile": "alex", "alias": "LDL directo", "alias_group": "ldl_c"}),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{r}");
    let vias: Vec<(String, Vec<String>)> = sqlx::query_as(
        "select action, array_agg(distinct via) from audit_event where actor_id = $1 \
         and action in ('read', 'alias_add') and resource_type in ('observation', 'analyte_alias') \
         group by action order by action",
    )
    .bind(a.family.user("alex"))
    .fetch_all(&a.rt.pool)
    .await?;
    assert_eq!(
        vias,
        vec![
            ("alias_add".into(), vec!["ui".into()]),
            ("read".into(), vec!["ui".into()])
        ]
    );
    Ok(())
}

#[tokio::test]
async fn test_pretty_unit_and_bad_query_values() -> R {
    use ownhealth_api::routes::observations::pretty_unit;
    assert_eq!(pretty_unit("10*3/uL"), "10³/µL");
    assert_eq!(pretty_unit("u[IU]/mL"), "µIU/mL");
    assert_eq!(pretty_unit("{INR}"), "");
    assert_eq!(pretty_unit("mL/min/{1.73_m2}"), "mL/min/1.73 m²");
    assert_eq!(pretty_unit("umol/L"), "µmol/L");
    assert_eq!(pretty_unit("mU/L"), "mU/L");
    let a = app().await?;
    let (s, r) = get(&a, "alex", "/profiles/alex/observations?units=metric").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r["detail"][0]["loc"], json!(["query", "units"]));
    let (s, r) = get(&a, "alex", "/profiles/alex/observations?category=nope").await?;
    assert_eq!(
        (s, &r["detail"]["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("bad_input"))
    );
    Ok(())
}
