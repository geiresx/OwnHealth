//! Lane C routes (`tests/imports/test_records_api.py`): records (list, add, correct, retract) and
//! manual observations, each test on its own throwaway database as a browser session.

use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

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

async fn cookie(a: &App, who: &str) -> anyhow::Result<String> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    Ok(format!("{COOKIE}={secret}"))
}

async fn send(a: &App, req: Request<Body>) -> anyhow::Result<(StatusCode, Value)> {
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
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

fn merge(base: &Value, extra: Value) -> Value {
    let mut out = base.clone();
    if let (Some(o), Value::Object(e)) = (out.as_object_mut(), extra) {
        o.extend(e);
    }
    out
}

#[tokio::test]
async fn records_add_correct_retract_with_history() -> R {
    let a = app().await?;
    let (s, first) = post(
        &a,
        "alex",
        "/profiles/lucia/records/allergy",
        json!({"substance": "Peanut", "criticality": "high", "reaction_text": "Hives"}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{first}");
    assert_eq!(
        (&first["title"], &first["status"]),
        (&json!("Peanut"), &json!("current"))
    );
    assert!(first["src"].as_str().unwrap_or_default().starts_with("src_"));
    let fid = first["id"].as_str().unwrap_or_default().to_string();
    let (s, second) = post(
        &a,
        "alex",
        &format!("/profiles/lucia/records/allergy/{fid}/correct"),
        json!({"substance": "Peanut", "criticality": "low", "reason": "allergist re-test"}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{second}");
    assert_eq!(second["supersedes"], json!(fid));
    assert_eq!(second["detail"]["criticality"], "low");
    let sid = second["id"].as_str().unwrap_or_default().to_string();
    let (_, items) = get(&a, "alex", "/profiles/lucia/records/allergy").await?;
    let ids: Vec<&Value> = items["items"]
        .as_array()
        .map(|v| v.iter().map(|i| &i["id"]).collect())
        .unwrap_or_default();
    assert_eq!(ids, vec![&json!(sid)]);
    let (_, hist) = get(&a, "alex", "/profiles/lucia/records/allergy?history=true").await?;
    let hist = hist["items"].as_array().cloned().unwrap_or_default();
    let pairs: Vec<(Value, Value)> = hist.iter().map(|i| (i["id"].clone(), i["status"].clone())).collect();
    assert_eq!(
        pairs,
        vec![(json!(sid), json!("current")), (json!(fid), json!("superseded"))]
    );
    assert_eq!(hist[1]["status_reason"], "allergist re-test");
    // retract the current version with a reason; nothing is deleted
    let (s, out) = post(
        &a,
        "alex",
        &format!("/profiles/lucia/records/allergy/{sid}/retract"),
        json!({"reason": "entered in error"}),
    )
    .await?;
    assert_eq!((s, &out["status"]), (StatusCode::OK, &json!("retracted")));
    let (_, now) = get(&a, "alex", "/profiles/lucia/records/allergy").await?;
    assert_eq!(now["items"], json!([]));
    // correcting a retracted version is a lifecycle conflict
    let (s, _) = post(
        &a,
        "alex",
        &format!("/profiles/lucia/records/allergy/{sid}/correct"),
        json!({"substance": "Peanut"}),
    )
    .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    let lucia = a.family.profile("lucia");
    let n: i64 = sqlx::query_scalar("select count(*) from allergy where profile_id = $1")
        .bind(lucia)
        .fetch_one(&a.rt.pool)
        .await?;
    let actions: Vec<String> =
        sqlx::query_scalar("select action from audit_event where profile_id = $1 and tool = 'records' order by id")
            .bind(lucia)
            .fetch_all(&a.rt.pool)
            .await?;
    assert_eq!(n, 2);
    assert_eq!(actions, vec!["write", "correct", "read", "read", "retract", "read"]);
    Ok(())
}

/// The Python case also sends a personal access token, which may not write over REST whatever its
/// scopes. Here `Caller` refuses every token before the route runs (lane I), so that part lives in
/// the auth tests.
#[tokio::test]
async fn record_validation_and_permissions() -> R {
    let a = app().await?;
    let (s, _) = post(
        &a,
        "alex",
        "/profiles/lucia/records/medication",
        json!({"strength": "5 mg"}),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = get(&a, "alex", "/profiles/lucia/records/vaccine").await?; // unknown kind
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    // Alex is a viewer on Sam: reads the summary only, writes nothing
    let (s, _) = post(
        &a,
        "alex",
        "/profiles/sam/records/condition",
        json!({"text": "Migraine"}),
    )
    .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = get(&a, "alex", "/profiles/sam/records/condition").await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Sam has no grant on Carmen
    let (s, body) = get(&a, "sam", "/profiles/carmen/records/condition").await?;
    assert_eq!(
        (s, &body["detail"]["code"]),
        (StatusCode::NOT_FOUND, &json!("profile_not_granted"))
    );
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Row {
    status: String,
    status_reason: Option<String>,
    accepted_by: String,
    source_kind: String,
    category: String,
    /// `str(Decimal)` of the numeric column, read as text.
    value_num: Option<String>,
    reviewed_by: Option<Uuid>,
    kind: String,
    actor_id: Option<Uuid>,
}

#[tokio::test]
async fn manual_observation_and_its_correction() -> R {
    let a = app().await?;
    let body = json!({
        "name": "Peso",
        "value": "81,4",
        "unit": "kg",
        "effective_at": "2026-11-06T08:00:00+01:00",
        "category": "vital-signs",
        "note": "after breakfast",
    });
    let (s, first) = post(&a, "alex", "/profiles/alex/manual-observations", body.clone()).await?;
    assert_eq!(s, StatusCode::CREATED, "{first}");
    let (s, second) = post(
        &a,
        "alex",
        "/profiles/alex/manual-observations",
        merge(
            &body,
            json!({"value": "80,4", "corrects": first["id"], "reason": "typo"}),
        ),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{second}");
    assert_eq!(second["supersedes"], first["id"]);
    let rows: Vec<Row> = sqlx::query_as(
        "select o.status, o.status_reason, o.accepted_by, o.source_kind, o.category, o.value_num::text as value_num, \
         o.reviewed_by, s.kind, s.actor_id from observation o join src s on s.id = o.src_id \
         where o.profile_id = $1 order by o.recorded_at, o.supersedes nulls first",
    )
    .bind(a.family.profile("alex"))
    .fetch_all(&a.rt.pool)
    .await?;
    let got: Vec<(&str, Option<&str>)> = rows
        .iter()
        .map(|x| (x.status.as_str(), x.value_num.as_deref()))
        .collect();
    assert_eq!(got, vec![("superseded", Some("81.4")), ("current", Some("80.4"))]);
    assert_eq!(rows[0].status_reason.as_deref(), Some("typo"));
    let kinds: BTreeSet<(&str, &str, &str, &str)> = rows
        .iter()
        .map(|x| {
            (
                x.accepted_by.as_str(),
                x.source_kind.as_str(),
                x.category.as_str(),
                x.kind.as_str(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from([("person", "self_reported", "vital-signs", "manual")])
    );
    let alex = a.family.user("alex");
    assert!(
        rows.iter()
            .all(|x| x.actor_id == Some(alex) && x.reviewed_by == Some(alex))
    );
    let (note, _): (String, String) = sqlx::query_as("select body, attach_to from note where attach_to = $1")
        .bind(first["src"].as_str().unwrap_or_default())
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(note, "after breakfast");
    // no offset, or someone else's observation, is refused
    let (s, _) = post(
        &a,
        "alex",
        "/profiles/alex/manual-observations",
        merge(&body, json!({"effective_at": "2026-11-06T08:00:00"})),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = post(
        &a,
        "alex",
        "/profiles/lucia/manual-observations",
        merge(&body, json!({"corrects": first["id"], "reason": "x"})),
    )
    .await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn manual_observation_refuses_a_blank_name_or_value() -> R {
    let a = app().await?;
    let body = json!({"name": "Peso", "value": "81,4", "unit": "kg", "effective_at": "2026-11-06T08:00:00+01:00"});
    for blank in [json!({"name": "   "}), json!({"value": "  "})] {
        let (s, out) = post(
            &a,
            "alex",
            "/profiles/alex/manual-observations",
            merge(&body, blank.clone()),
        )
        .await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{blank} {out}");
    }
    Ok(())
}
