//! Acting on a due item (migration 1130): snooze, done with evidence, dismiss with a reason, and the
//! answers some guideline rows need. Each test runs on its own throwaway database as a browser
//! session of the synthetic Rivera family.

use axum::body::Body;
use chrono::{Duration, Months, NaiveDate};
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_core::Runtime;
use ownhealth_core::access::{ProfileIn, Scopes, all_scopes, create_profile, set_grant, today_utc};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

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

async fn send(a: &App, who: &str, method: &str, uri: &str, body: Option<Value>) -> anyhow::Result<(StatusCode, Value)> {
    let mut c = a.rt.pool.acquire().await?;
    let secret = create_session(&mut c, a.family.user(who), None, None, "passkey").await?;
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("{COOKIE}={secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 24).await?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

async fn due(a: &App, who: &str, p: &str, as_of: NaiveDate) -> anyhow::Result<Value> {
    let (s, v) = send(
        a,
        who,
        "GET",
        &format!("/profiles/{p}/due?lang=en&as_of={}", iso(as_of)),
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    Ok(v)
}

fn find<'a>(doc: &'a Value, list: &str, key: &str) -> Option<&'a Value> {
    doc[list].as_array()?.iter().find(|i| i["key"] == key)
}

async fn decide(a: &App, who: &str, p: &str, key: &str, body: Value) -> anyhow::Result<(StatusCode, Value)> {
    send(a, who, "POST", &format!("/profiles/{p}/due/{key}/decision"), Some(body)).await
}

/// A synthetic person of the household, `years` old today.
async fn person(a: &App, slug: &str, years: u32, sex: &str, country: &str) -> anyhow::Result<Uuid> {
    let mut c = a.db.pool.acquire().await?;
    let born = today_utc()
        .checked_sub_months(Months::new(years * 12 + 2))
        .unwrap_or_default();
    let pid = create_profile(
        &mut c,
        &ProfileIn {
            household_id: a.family.household_id,
            slug: slug.into(),
            display_name: format!("{slug} Rivera"),
            relationship: Some("other".into()),
            birth_date: Some(born),
            sex_at_birth: Some(sex.into()),
            country: Some(country.into()),
            ..ProfileIn::default()
        },
    )
    .await?;
    set_grant(&mut c, a.family.user("alex"), pid, "guardian", None, None, None).await?;
    Ok(pid)
}

const KEY: &str = "es.cancer.breast";

/// Carmen (71, Spain) has breast cancer screening due; Alex is her caregiver and may act.
#[tokio::test]
async fn a_snooze_hides_the_item_until_its_date_and_it_is_back_the_day_after() -> R {
    let a = app().await?;
    let today = today_utc();
    let before = due(&a, "alex", "carmen", today).await?;
    assert_eq!(find(&before, "items", KEY).map(|i| &i["status"]), Some(&json!("due")));
    let until = today + Duration::days(30);
    let (s, v) = decide(
        &a,
        "alex",
        "carmen",
        KEY,
        json!({"kind": "snooze", "until": iso(until)}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    for day in [today, until] {
        let d = due(&a, "alex", "carmen", day).await?;
        assert!(find(&d, "items", KEY).is_none(), "{day}: still listed");
        let h = find(&d, "hidden", KEY).expect("listed as hidden");
        assert_eq!(h["decision"], json!({"kind": "snooze", "until": iso(until)}));
    }
    let after = due(&a, "alex", "carmen", until + Duration::days(1)).await?;
    let back = find(&after, "items", KEY).expect("shown again the day after");
    assert_eq!(back["decision"]["kind"], "snooze");
    assert!(after.get("hidden").is_none(), "{after}");

    // the decision moves the hash, and undoing it brings the old one back
    let snoozed = due(&a, "alex", "carmen", today).await?;
    assert_ne!(snoozed["prevention_hash"], before["prevention_hash"]);
    let (s, v) = send(
        &a,
        "alex",
        "DELETE",
        &format!("/profiles/carmen/due/{KEY}/decision"),
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let undone = due(&a, "alex", "carmen", today).await?;
    assert_eq!(undone["prevention_hash"], before["prevention_hash"]);
    assert_eq!(undone, before);
    // nothing left to undo
    let (s, _) = send(
        &a,
        "alex",
        "DELETE",
        &format!("/profiles/carmen/due/{KEY}/decision"),
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // a snooze into the past (before the local today of a browser west of UTC), or with no date, is
    // refused
    for body in [
        json!({"kind": "snooze", "until": iso(today - Duration::days(2))}),
        json!({"kind": "snooze"}),
    ] {
        let (s, v) = decide(&a, "alex", "carmen", KEY, body).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    }
    // the local today of a browser west of UTC is yesterday here, and the screen offers it
    let body = json!({"kind": "snooze", "until": iso(today - Duration::days(1))});
    let (s, v) = decide(&a, "alex", "carmen", KEY, body).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    // the screen asks for its own local day, on which the item is hidden
    let doc = due(&a, "alex", "carmen", today - Duration::days(1)).await?;
    assert!(find(&doc, "hidden", KEY).is_some(), "{doc}");
    Ok(())
}

#[tokio::test]
async fn a_dismissal_needs_a_reason_and_hides_the_item_from_the_status_too() -> R {
    let a = app().await?;
    let today = today_utc();
    for body in [
        json!({"kind": "dismiss"}),
        json!({"kind": "dismiss", "reason": "   "}),
        json!({"kind": "dismiss", "reason": "x".repeat(501)}),
    ] {
        let (s, v) = decide(&a, "alex", "carmen", KEY, body).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    }
    let (s, v) = decide(
        &a,
        "alex",
        "carmen",
        "es.no.such.row",
        json!({"kind": "dismiss", "reason": "r"}),
    )
    .await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    let pid = a.family.profile("carmen");
    let pack = ownhealth_rules::load_pack()?;
    let mut c = a.db.pool.acquire().await?;
    let body = ownhealth_rules::build_state(&mut c, pid, today, None).await?;
    let open = ownhealth_rules::prevention::status_section(&mut c, pid, today, &body, &pack, &all_scopes())
        .await?
        .unwrap_or_default();
    assert!(open.get("due_hidden").is_none(), "{open}");

    let (s, v) = decide(
        &a,
        "alex",
        "carmen",
        KEY,
        json!({"kind": "dismiss", "reason": "Done abroad in 2025"}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let d = due(&a, "alex", "carmen", today).await?;
    assert!(find(&d, "items", KEY).is_none());
    let h = find(&d, "hidden", KEY).expect("hidden");
    assert_eq!(h["decision"]["kind"], "dismiss");
    assert_eq!(h["decision"]["reason"], "Done abroad in 2025");
    assert!(h["title"].is_string(), "{h}");

    let now = ownhealth_rules::prevention::status_section(&mut c, pid, today, &body, &pack, &all_scopes())
        .await?
        .unwrap_or_default();
    assert_eq!(now["due_hidden"], 1, "{now}");
    assert_eq!(now["due_total"].as_u64(), open["due_total"].as_u64().map(|n| n - 1));
    assert!(!now.to_string().contains(KEY), "{now}");
    Ok(())
}

async fn procedure(a: &App, p: &str, text: &str, day: NaiveDate) -> anyhow::Result<String> {
    let (s, v) = send(
        a,
        "alex",
        "POST",
        &format!("/profiles/{p}/records/procedure"),
        Some(json!({"text": text, "performed": iso(day)})),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    Ok(v["id"].as_str().unwrap_or_default().to_string())
}

#[tokio::test]
async fn done_counts_its_evidence_and_a_retracted_record_reopens_the_item() -> R {
    let a = app().await?;
    let today = today_utc();
    // a record the evidence join does not know by name
    let on = today - Duration::days(40);
    let own = procedure(&a, "carmen", "Breast imaging, private clinic", on).await?;
    let theirs = procedure(&a, "alex", "Breast imaging, private clinic", on).await?;
    assert_eq!(
        find(&due(&a, "alex", "carmen", today).await?, "items", KEY).map(|i| &i["status"]),
        Some(&json!("due"))
    );

    // another profile's record is refused, as is a record that does not exist
    for id in [theirs.clone(), Uuid::new_v4().to_string()] {
        let (s, v) = decide(
            &a,
            "alex",
            "carmen",
            KEY,
            json!({"kind": "done", "evidence": {"table": "procedure", "id": id}}),
        )
        .await?;
        assert_eq!(
            (s, v["detail"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("evidence_refused")),
            "{v}"
        );
    }
    let (s, v) = decide(&a, "alex", "carmen", KEY, json!({"kind": "done"})).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    let (s, v) = decide(
        &a,
        "alex",
        "carmen",
        KEY,
        json!({"kind": "done", "evidence": {"table": "procedure", "id": own}}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let d = due(&a, "alex", "carmen", today).await?;
    let it = find(&d, "items", KEY).expect("a guideline row stays listed");
    assert_eq!(it["status"], "up_to_date", "{it}");
    assert_eq!(it["last"]["date"], iso(on));
    assert_eq!(it["decision"]["kind"], "done");
    assert_eq!(it["decision"]["evidence"], json!({"table": "procedure", "id": own}));

    // the record is withdrawn: the item is open again, as if nothing had been decided
    let (s, v) = send(
        &a,
        "alex",
        "POST",
        &format!("/profiles/carmen/records/procedure/{own}/retract"),
        Some(json!({"reason": "entered in error"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let d = due(&a, "alex", "carmen", today).await?;
    let it = find(&d, "items", KEY).expect("listed");
    assert_eq!(it["status"], "due");
    assert!(it.get("decision").is_none(), "{it}");
    Ok(())
}

#[tokio::test]
async fn done_survives_a_correction_and_hides_a_lab_gap() -> R {
    let a = app().await?;
    let today = today_utc();
    let on = today - Duration::days(10);
    let id = procedure(&a, "carmen", "Breast imaging", on).await?;
    let (s, v) = decide(
        &a,
        "alex",
        "carmen",
        KEY,
        json!({"kind": "done", "evidence": {"table": "procedure", "id": id}}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = send(
        &a,
        "alex",
        "POST",
        &format!("/profiles/carmen/records/procedure/{id}/correct"),
        Some(json!({"text": "Breast imaging (corrected)", "performed": iso(on), "reason": "typo"})),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let it = find(&due(&a, "alex", "carmen", today).await?, "items", KEY)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        it["status"], "up_to_date",
        "the corrected version still proves it: {it}"
    );

    // a lab gap is not a guideline row: done hides it while its evidence holds
    let gap = "lipid.lpa.once_adult";
    assert!(find(&due(&a, "alex", "carmen", today).await?, "items", gap).is_some());
    let (s, v) = decide(
        &a,
        "alex",
        "carmen",
        gap,
        json!({"kind": "done", "evidence": {"table": "procedure", "id": id}}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let d = due(&a, "alex", "carmen", today).await?;
    assert!(find(&d, "items", gap).is_none());
    assert_eq!(
        find(&d, "hidden", gap).map(|h| &h["decision"]["kind"]),
        Some(&json!("done"))
    );
    Ok(())
}

#[tokio::test]
async fn an_answer_moves_a_row_from_not_assessed_to_due_and_clearing_it_reverts() -> R {
    let a = app().await?;
    let today = today_utc();
    person(&a, "dana", 60, "female", "US").await?;
    let lung = "us.cancer.lung";
    let before = due(&a, "alex", "dana", today).await?;
    let it = find(&before, "items", lung).expect("listed");
    assert_eq!(
        (&it["status"], &it["reason_code"], &it["answer"]),
        (&json!("not_assessed"), &json!("no_answer"), &json!("smoking_history"))
    );
    assert!(
        it["reason"].as_str().is_some_and(|r| r.contains("20 pack-years")),
        "{it}"
    );

    let (s, v) = send(&a, "alex", "GET", "/profiles/dana/prevention-answers?lang=es", None).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let keys: Vec<&str> = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["key"].as_str())
        .collect();
    assert_eq!(keys, ["smoking_history", "hysterectomy"]);
    assert!(
        v["items"][0]["question"]
            .as_str()
            .is_some_and(|q| q.contains("paquetes-año")),
        "{v}"
    );
    assert_eq!(v["items"][0]["value"], Value::Null);

    let (s, v) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers",
        Some(json!({"smoking_history": "yes"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["value"], "yes");
    assert!(
        v["items"][0]["src"].as_str().is_some_and(|s| s.starts_with("src_")),
        "{v}"
    );
    // the reply speaks the language the screen asks for, not the user's stored one (Spanish)
    let (s, v) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers?lang=en",
        Some(json!({"smoking_history": "yes"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(
        v["items"][0]["question"]
            .as_str()
            .is_some_and(|q| q.contains("pack-years")),
        "{v}"
    );
    let yes = due(&a, "alex", "dana", today).await?;
    assert_eq!(find(&yes, "items", lung).map(|i| &i["status"]), Some(&json!("due")));
    assert_ne!(yes["prevention_hash"], before["prevention_hash"]);

    let (s, _) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers",
        Some(json!({"smoking_history": "no"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK);
    assert!(
        find(&due(&a, "alex", "dana", today).await?, "items", lung).is_none(),
        "a no takes the row away"
    );

    let (s, _) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers",
        Some(json!({"smoking_history": null})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK);
    let cleared = due(&a, "alex", "dana", today).await?;
    assert_eq!(cleared["prevention_hash"], before["prevention_hash"]);

    // a hysterectomy that removed the cervix takes the cervical rows away; unanswered, they stay
    let cervix = "us.cancer.cervix_hpv";
    assert!(find(&cleared, "items", cervix).is_some());
    let (s, _) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers",
        Some(json!({"hysterectomy": "yes"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK);
    assert!(find(&due(&a, "alex", "dana", today).await?, "items", cervix).is_none());

    for body in [
        json!({"pregnant": "yes"}),
        json!({"smoking_history": "maybe"}),
        json!(["yes"]),
    ] {
        let (s, v) = send(&a, "alex", "PUT", "/profiles/dana/prevention-answers", Some(body)).await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    }
    // a man in Spain has no question that applies to him
    let (_, v) = send(&a, "alex", "GET", "/profiles/alex/prevention-answers", None).await?;
    assert_eq!(v["items"], json!([]));
    // nor a woman of 71, past the age of every row that asks
    let (_, v) = send(&a, "alex", "GET", "/profiles/carmen/prevention-answers", None).await?;
    assert_eq!(v["items"], json!([]));
    Ok(())
}

#[tokio::test]
async fn deleting_a_profile_with_decisions_and_answers_succeeds() -> R {
    let a = app().await?;
    let today = today_utc();
    let pid = person(&a, "dana", 60, "female", "US").await?;
    let (s, v) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers",
        Some(json!({"smoking_history": "yes"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers",
        Some(json!({"smoking_history": "no"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK);
    let until = iso(today + Duration::days(9));
    let (s, v) = decide(
        &a,
        "alex",
        "dana",
        "us.cancer.colorectal",
        json!({"kind": "snooze", "until": until}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = decide(
        &a,
        "alex",
        "dana",
        "us.cancer.colorectal",
        json!({"kind": "dismiss", "reason": "r"}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let mut tx = a.db.pool.begin().await?;
    ownhealth_auth::household::delete_profile(&mut tx, pid).await?;
    tx.commit().await?;
    for table in ["prevention_decision", "prevention_answer"] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select count(*) from {table} where profile_id = $1"
        )))
        .bind(pid)
        .fetch_one(&a.db.pool)
        .await?;
        assert_eq!(n, 0, "{table}");
    }
    Ok(())
}

#[tokio::test]
async fn audit_rows_carry_ids_and_codes_never_the_reason_or_the_answer() -> R {
    let a = app().await?;
    let reason = "Seen by Dr Ortega at LabNorte";
    let (s, v) = decide(&a, "alex", "carmen", KEY, json!({"kind": "dismiss", "reason": reason})).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    person(&a, "dana", 60, "female", "US").await?;
    let (s, _) = send(
        &a,
        "alex",
        "PUT",
        "/profiles/dana/prevention-answers",
        Some(json!({"hysterectomy": "yes"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK);
    let rows: Vec<Value> = sqlx::query_scalar("select to_jsonb(a) from audit_event a where tool = 'due' order by id")
        .fetch_all(&a.db.pool)
        .await?;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["args_summary"], format!("dismiss {KEY}"));
    assert_eq!(rows[1]["args_summary"], "hysterectomy");
    let text = serde_json::to_string(&rows)?;
    assert!(!text.contains("Ortega") && !text.contains("\"yes\""), "{text}");
    Ok(())
}

#[tokio::test]
async fn on_a_minors_profile_a_guardian_acts_and_a_viewer_cannot() -> R {
    let a = app().await?;
    let pid = person(&a, "noa", 13, "female", "ES").await?;
    {
        let mut c = a.db.pool.acquire().await?;
        set_grant(&mut c, a.family.user("sam"), pid, "viewer", None, None, None).await?;
    }
    let key = "es.vac.hpv";
    let d = due(&a, "alex", "noa", today_utc()).await?;
    assert!(find(&d, "items", key).is_some(), "{d}");
    let body = json!({"kind": "snooze", "until": iso(today_utc() + Duration::days(5))});
    let (s, v) = decide(&a, "sam", "noa", key, body.clone()).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, _) = send(
        &a,
        "sam",
        "PUT",
        "/profiles/noa/prevention-answers",
        Some(json!({"hysterectomy": "no"})),
    )
    .await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = decide(&a, "alex", "noa", key, body).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _) = send(&a, "sam", "DELETE", &format!("/profiles/noa/due/{key}/decision"), None).await?;
    assert_eq!(s, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn evidence_without_a_day_or_from_the_future_is_refused() -> R {
    let a = app().await?;
    let today = today_utc();
    // a record with no day, and one dated after today
    let mut ids = Vec::new();
    for performed in [None, Some(iso(today + Duration::days(2)))] {
        let (s, v) = send(
            &a,
            "alex",
            "POST",
            "/profiles/carmen/records/procedure",
            Some(json!({"text": "Breast imaging", "performed": performed})),
        )
        .await?;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        ids.push(v["id"].as_str().unwrap_or_default().to_string());
    }
    for id in ids {
        let (s, v) = decide(
            &a,
            "alex",
            "carmen",
            KEY,
            json!({"kind": "done", "evidence": {"table": "procedure", "id": id}}),
        )
        .await?;
        assert_eq!(
            (s, v["detail"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("evidence_undated")),
            "{v}"
        );
    }
    // a document with no collection or report date is undated too: the day it was stored says
    // nothing about when the test was done
    let doc: Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, mime, size_bytes) \
         select household_id, id, decode(repeat('ab', 32), 'hex'), 'application/pdf', 1 from profile where id = $1 \
         returning id",
    )
    .bind(a.family.profile("carmen"))
    .fetch_one(&a.db.pool)
    .await?;
    let body = json!({"kind": "done", "evidence": {"table": "document", "id": doc}});
    let (s, v) = decide(&a, "alex", "carmen", KEY, body.clone()).await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("evidence_undated")),
        "{v}"
    );
    // once the document carries its report date, it counts
    sqlx::query("update document set reported_at = now() - interval '3 days' where id = $1")
        .bind(doc)
        .execute(&a.db.pool)
        .await?;
    let (s, v) = decide(&a, "alex", "carmen", KEY, body).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    Ok(())
}

/// A caregiver whose grant leaves out read:labs cannot see a lab gap, so they can neither probe for
/// it nor hide it, nor undo a decision about it.
#[tokio::test]
async fn a_lab_gap_needs_read_labs_to_act_on() -> R {
    let a = app().await?;
    let gap = "lipid.lpa.once_adult";
    let body = json!({"kind": "dismiss", "reason": "measured abroad"});
    let (s, v) = decide(&a, "alex", "carmen", gap, body.clone()).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    {
        let mut c = a.db.pool.acquire().await?;
        let pid = a.family.profile("carmen");
        let cats: Scopes = all_scopes().into_iter().filter(|s| s != "read:labs").collect();
        set_grant(&mut c, a.family.user("alex"), pid, "caregiver", Some(&cats), None, None).await?;
    }
    let (s, v) = decide(&a, "alex", "carmen", gap, body).await?;
    assert_eq!(
        (s, v["detail"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("unknown")),
        "{v}"
    );
    let (s, v) = send(
        &a,
        "alex",
        "DELETE",
        &format!("/profiles/carmen/due/{gap}/decision"),
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    // a row the grant still covers takes a decision as before
    let (s, v) = decide(
        &a,
        "alex",
        "carmen",
        KEY,
        json!({"kind": "snooze", "until": iso(today_utc() + Duration::days(7))}),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    Ok(())
}
