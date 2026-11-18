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
    assert!(
        es.iter().any(|x| x.contains("masa molar de C27H46O (386,654 g/mol)")),
        "{es:?}"
    );
    assert!(es.iter().any(|x| x.contains("(LOINC 18262-6)")), "{es:?}");
    assert!(!es.iter().any(|x| x.contains("Cholesterol in LDL")), "{es:?}");
    assert!(!es.iter().any(|x| x.contains("are ") || x.contains("Labs ")), "{es:?}");
    let st = &t["stats"];
    let last = values[2];
    let pct = st["last_vs_previous_pct"].as_f64().unwrap_or_default();
    assert_eq!(st["n"], 3);
    assert_eq!(pct, round1((last - 84.0) / 84.0 * 100.0));
    assert_ne!(pct, pct.round()); // one decimal, not a whole number
    let rcv = st["rcv_pct"].as_f64().unwrap_or_default();
    // each point after the first comes from another LOINC code: no change is judged against the RCV
    assert!(
        rcv > 0.0 && st["exceeds_rcv"].is_null() && st["method_change"] == true,
        "{st}"
    );
    let switched: Vec<&Value> = points.iter().map(|p| &p["method_change"]).collect();
    assert_eq!(switched, [&json!(false), &json!(true), &json!(true)]);
    assert!(
        lim.iter()
            .any(|x| x.contains("not compared with the reference change value")),
        "{lim:?}"
    );
    // every CV shown cites the paper it comes from
    assert_eq!((&st["cv_i"], &st["cv_a"]), (&json!("8.46"), &json!("1.77")));
    assert!(
        st["rcv_cite"]
            .as_str()
            .is_some_and(|c| c.starts_with("https://doi.org/")),
        "{st}"
    );
    // no earlier point shares the last one's code, so there is no usual level yet
    assert!(st["baseline"].is_null(), "{st}");
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

/// One LDL printed by a lab, on a document named after that lab.
async fn lab_row(c: &mut PgConnection, f: &Family, lab: &str, day: &str, value: &str) -> R {
    lab_row_in(c, f, lab, day, value, Some("mg/dL")).await
}

async fn lab_row_in(c: &mut PgConnection, f: &Family, lab: &str, day: &str, value: &str, unit: Option<&str>) -> R {
    let doc: uuid::Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, mime, size_bytes, doc_type, provider_name) \
         values ($1, $2, $3, 'application/pdf', 1, 'lab_report', $4) returning id",
    )
    .bind(f.household_id)
    .bind(f.profile("alex"))
    .bind(format!("{lab} {day} {value}").into_bytes())
    .bind(lab)
    .fetch_one(&mut *c)
    .await?;
    let when = Utc.from_utc_datetime(
        &chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")?
            .and_hms_opt(8, 0, 0)
            .unwrap_or_default(),
    );
    let n = normalize_row(
        c,
        &RowIn {
            profile_id: f.profile("alex"),
            printed_name: "Colesterol LDL",
            printed_value: value,
            printed_unit: unit,
            printed_range: Some("< 130"),
            printed_flag: None,
            effective_at: when.fixed_offset(),
            lang: "es",
            specimen: None,
            provider: None,
            loinc: None,
            method: None,
            document_id: Some(doc),
        },
    )
    .await?;
    let mut obs = n.observation;
    obs.document_id = Some(doc); // the caller links the document, as the PDF import does
    commit_observation(c, &obs, &SrcRef::New(SrcIn::manual(None)), CommitOptions::new("person")).await?;
    Ok(())
}

/// Inside one lab the last change is judged against the RCV and the person's usual level; a switch
/// to another lab is not judged, and the answer says why.
#[tokio::test]
async fn a_lab_switch_is_not_judged_as_a_change() -> R {
    let a = app().await?;
    let mut tx = a.rt.pool.begin().await?;
    for (day, v) in [
        ("2026-01-10", "100"),
        ("2026-03-10", "102"),
        ("2026-05-10", "101"),
        ("2026-07-10", "140"),
    ] {
        lab_row(&mut tx, &a.family, "LabNorte", day, v).await?;
    }
    tx.commit().await?;
    let (s, t) = get(&a, "alex", "/profiles/alex/trend?code=ldl_c&lang=en").await?;
    assert_eq!(s, StatusCode::OK, "{t}");
    let st = &t["stats"];
    assert_eq!(
        (&st["exceeds_rcv"], &st["method_change"]),
        (&json!(true), &json!(false)),
        "{st}"
    );
    // set point 101, half width 101 * 1.96 * sqrt(8.46^2 + 1.77^2) % * sqrt(1 + 1/3) = 19.76
    assert_eq!(
        st["baseline"],
        json!({"set_point": 101.0, "low": 81.24, "high": 120.76, "n": 3, "outside": true, "rule": "baseline.personal",
               "cite": "https://doi.org/10.1093/clinchem/hvaa233"}),
        "{st}"
    );
    let mut tx = a.rt.pool.begin().await?;
    lab_row(&mut tx, &a.family, "Hospital Norte", "2026-09-10", "104").await?;
    tx.commit().await?;
    let (_, t) = get(&a, "alex", "/profiles/alex/trend?code=ldl_c&lang=es").await?;
    let st = &t["stats"];
    assert_eq!(
        (&st["exceeds_rcv"], &st["method_change"]),
        (&Value::Null, &json!(true)),
        "{st}"
    );
    assert!(st["baseline"].is_null(), "{st}");
    let last = t["points"]
        .as_array()
        .and_then(|p| p.last())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        (&last["lab"], &last["method_change"]),
        (&json!("Hospital Norte"), &json!(true))
    );
    assert!(
        t["limitations"].as_array().is_some_and(|l| l
            .iter()
            .any(|x| x.as_str().is_some_and(|x| x.contains("otro método o laboratorio")))),
        "{t}"
    );
    Ok(())
}

/// The usual level reads every earlier result of the same lab and method, as the state flag does,
/// whatever window the chart asks for.
#[tokio::test]
async fn the_usual_level_does_not_depend_on_the_window() -> R {
    let a = app().await?;
    let today = chrono::Local::now().date_naive();
    let mut tx = a.rt.pool.begin().await?;
    for (days_ago, v) in [(1825, "100"), (1460, "102"), (1095, "101"), (730, "100"), (30, "140")] {
        let day = (today - chrono::Days::new(days_ago)).format("%Y-%m-%d").to_string();
        lab_row(&mut tx, &a.family, "LabNorte", &day, v).await?;
    }
    tx.commit().await?;
    let mut seen = Vec::new();
    for window in ["P1Y", "P3Y", "P10Y"] {
        let (s, t) = get(&a, "alex", &format!("/profiles/alex/trend?code=ldl_c&window={window}")).await?;
        assert_eq!(s, StatusCode::OK, "{t}");
        seen.push(t["stats"]["baseline"].clone());
    }
    // set point 100.75 from the four earlier results, even when the window shows only the last one
    assert_eq!(
        (&seen[0]["n"], &seen[0]["set_point"], &seen[0]["outside"]),
        (&json!(4), &json!(100.75), &json!(true)),
        "{seen:?}"
    );
    assert!(seen.iter().all(|b| *b == seen[0]), "{seen:?}");
    Ok(())
}

/// A last result printed with no unit has no chart unit to compare in, so there is no usual level.
#[tokio::test]
async fn a_result_with_no_unit_gets_no_usual_level() -> R {
    let a = app().await?;
    let today = chrono::Local::now().date_naive();
    let mut tx = a.rt.pool.begin().await?;
    for (days_ago, v, unit) in [
        (900, "100", Some("mg/dL")),
        (800, "102", Some("mg/dL")),
        (700, "101", Some("mg/dL")),
        (30, "300", None),
    ] {
        let day = (today - chrono::Days::new(days_ago)).format("%Y-%m-%d").to_string();
        lab_row_in(&mut tx, &a.family, "LabNorte", &day, v, unit).await?;
    }
    tx.commit().await?;
    let (s, t) = get(&a, "alex", "/profiles/alex/trend?code=ldl_c&window=P1Y").await?;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert!(t["stats"]["baseline"].is_null(), "{t}");
    Ok(())
}

/// Two results on the same day are ordered as the engine orders them, by when they were recorded,
/// so the last point the trend judges is the one the state flag judges.
#[tokio::test]
async fn results_on_one_day_keep_the_order_they_were_recorded_in() -> R {
    let a = app().await?;
    // find a pair whose id order is the reverse of its recording order; each insert is its own transaction
    let mut found = false;
    for i in 1..=30 {
        let day = format!("2026-01-{i:02}");
        let mut ids = Vec::new();
        for v in ["100", "140"] {
            let mut tx = a.rt.pool.begin().await?;
            lab_row(&mut tx, &a.family, "LabNorte", &day, v).await?;
            tx.commit().await?;
            let id: uuid::Uuid = sqlx::query_scalar(
                "select id from observation where alias_group = 'ldl_c' order by recorded_at desc limit 1",
            )
            .fetch_one(&a.rt.pool)
            .await?;
            ids.push(id.to_string());
        }
        if ids[0] > ids[1] {
            found = true;
            break;
        }
    }
    assert!(found, "no pair with ids in reverse recording order");
    let (_, t) = get(&a, "alex", "/profiles/alex/trend?code=ldl_c&window=P10Y").await?;
    let last = t["points"]
        .as_array()
        .and_then(|p| p.last())
        .cloned()
        .unwrap_or_default();
    assert_eq!(last["value"], json!(140.0), "{t}");
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
    .await?
    .plaintext;
    for uri in ["/profiles/alex/observations", "/profiles/alex/trend?code=ldl_c"] {
        let req = Request::get(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())?;
        assert_eq!(send(&a, req).await?.0, StatusCode::FORBIDDEN, "{uri}");
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
    // a window ending in a multi-byte char is a 422, not a panic
    for window in ["P99999Y", "P9999999D", "P1%C3%A9", "P%C3%A9"] {
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
