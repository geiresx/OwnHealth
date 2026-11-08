//! Lane I: `api/review.py` (tests/security/test_review_scoping.py and the review parts of
//! tests/test_app.py): accept never writes outside the item's profile, held payloads follow the
//! read scopes, and the request validation answers as FastAPI did.

mod lane_i;

use http::StatusCode;
use lane_i::{App, R, app, item, lab_document};
use serde_json::{Value, json};
use uuid::Uuid;

async fn count(a: &App, table: &str, pid: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select count(*) from {table} where profile_id = $1"
    )))
    .bind(pid)
    .fetch_one(&a.rt.pool)
    .await?)
}

async fn status_of(a: &App, id: Uuid) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar("select status from review_item where id = $1")
        .bind(id)
        .fetch_one(&a.rt.pool)
        .await?)
}

#[tokio::test]
async fn test_accept_edits_cannot_move_a_fact_to_another_profile() -> R {
    let a = app().await?;
    let f = &a.family;
    let sam = f.profile("sam"); // alex is only a viewer on sam
    let cand = item(&a, "alex", "candidate_row", None, json!({})).await?;
    let alex = f.profile("alex").to_string();
    let fhir = item(
        &a,
        "alex",
        "fhir_unmapped",
        Some(json!({
            "projection": "record",
            "record_kind": "condition",
            "record": {"profile_id": alex, "text": "Planted condition"},
            "src": {"kind": "fhir_import", "profile_id": alex, "import_ref": "bundle#1"},
        })),
        json!({}),
    )
    .await?;
    for id in [cand, fhir] {
        let (s, v) = a
            .post(
                "alex",
                &format!("/review/{id}/accept"),
                Some(json!({"edits": {"profile_id": sam}})),
            )
            .await?;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["detail"]["code"], "conflict");
    }
    assert_eq!(count(&a, "observation", sam).await?, 0);
    assert_eq!(count(&a, "condition", sam).await?, 0);
    let (s, v) = a
        .post(
            "alex",
            &format!("/review/{cand}/accept"),
            Some(json!({"edits": {"value_num": "98"}})),
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(count(&a, "observation", f.profile("alex")).await?, 1);
    Ok(())
}

#[tokio::test]
async fn test_document_identity_target_needs_import() -> R {
    let a = app().await?;
    let payload = json!({"suggested_profile_id": a.family.profile("sam").to_string()});
    let id = item(&a, "alex", "document_identity", Some(payload), json!({})).await?;
    let (s, v) = a.post("alex", &format!("/review/{id}/accept"), Some(json!({}))).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(status_of(&a, id).await?, "open");
    Ok(())
}

#[tokio::test]
async fn test_caregiver_without_read_sensitive_cannot_see_a_sensitive_held_value() -> R {
    let a = app().await?;
    let hidden = item(
        &a,
        "carmen",
        "candidate_row",
        None,
        json!({"value_num": "1", "printed_value": "1", "value_canonical": "1", "printed_name": "VIH Ac",
               "sensitivity": ["sexual_health"]}),
    )
    .await?;
    let plain = item(&a, "carmen", "candidate_row", None, json!({})).await?;
    // alex is a caregiver on carmen: no read:sensitive
    let (s, listed) = a.get("alex", "/review?profile=carmen").await?;
    assert_eq!(s, StatusCode::OK, "{listed}");
    let ids: Vec<Value> = listed["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|i| i["id"].clone())
        .collect();
    assert!(ids.contains(&json!(plain)) && !ids.contains(&json!(hidden)), "{ids:?}");
    assert_eq!(listed["withheld"], json!(1));
    assert_eq!(listed["open"], json!(2));
    let (s, v) = a.get("alex", &format!("/review/{hidden}")).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        v,
        json!({"detail": {"code": "not_found", "message": "No such review item."}})
    );
    assert_eq!(
        a.post("alex", &format!("/review/{hidden}/accept"), Some(json!({})))
            .await?
            .0,
        StatusCode::NOT_FOUND
    );
    let (s, one) = a.get("alex", &format!("/review/{plain}")).await?;
    assert_eq!(s, StatusCode::OK);
    // FastAPI: the list (`-> dict`) went through pydantic (`Z`), the single item through
    // jsonable_encoder (`+00:00`)
    let listed_at = listed["items"][0]["created_at"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let one_at = one["created_at"].as_str().unwrap_or_default().to_string();
    assert!(
        listed_at.ends_with('Z') && one_at.ends_with("+00:00"),
        "{listed_at} {one_at}"
    );
    assert_eq!(listed_at.trim_end_matches('Z'), one_at.trim_end_matches("+00:00"));
    let keys: Vec<&String> = one.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        [
            "id",
            "kind",
            "profile_id",
            "status",
            "reasons",
            "document_id",
            "candidate_row_id",
            "observation_id",
            "payload",
            "row_conf",
            "proposed_by_token",
            "context_src_ids",
            "decided_by",
            "decided_at",
            "decision_reason",
            "result_ids",
            "created_at"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn test_viewer_cannot_review_and_others_see_nothing() -> R {
    let a = app().await?;
    let id = item(&a, "alex", "candidate_row", None, json!({})).await?;
    // sam is a viewer on alex: the item is not listed, and deciding it is refused
    let (_, listed) = a.get("sam", "/review").await?;
    assert!(!listed.to_string().contains(&id.to_string()));
    let (s, v) = a.get("sam", &format!("/review/{id}")).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["detail"]["code"], "forbidden");
    assert_eq!(v["detail"]["message"], "The role viewer cannot review on this profile.");
    let (s, _) = a.get("alex", &format!("/review/{}", Uuid::new_v4())).await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_reject_reopen_dismiss_and_audit() -> R {
    let a = app().await?;
    let id = item(&a, "alex", "candidate_row", None, json!({})).await?;
    let (s, v) = a.post("alex", &format!("/review/{id}/reject"), Some(json!({}))).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v,
        json!({"detail": [{"type": "missing", "loc": ["body", "reason"], "msg": "Field required", "input": {}}]})
    );
    let (s, v) = a.post("alex", &format!("/review/{id}/reject"), None).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v,
        json!({"detail": [{"type": "missing", "loc": ["body"], "msg": "Field required", "input": null}]})
    );
    let (s, v) = a
        .post("alex", &format!("/review/{id}/reject"), Some(json!({"reason": 5})))
        .await?;
    assert_eq!(
        (s, v["detail"][0]["type"].clone()),
        (StatusCode::UNPROCESSABLE_ENTITY, json!("string_type"))
    );
    let (s, v) = a
        .post(
            "alex",
            &format!("/review/{id}/reject"),
            Some(json!({"reason": "misread"})),
        )
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"id": id, "status": "rejected"}));
    let (s, v) = a.post("alex", &format!("/review/{id}/reopen"), None).await?;
    assert_eq!((s, v), (StatusCode::OK, json!({"id": id, "status": "open"})));
    // dismiss takes an optional body
    let (s, v) = a.post("alex", &format!("/review/{id}/dismiss"), None).await?;
    assert_eq!((s, v), (StatusCode::OK, json!({"id": id, "status": "dismissed"})));
    let (s, _) = a.post("alex", &format!("/review/{id}/dismiss"), None).await?;
    assert_eq!(s, StatusCode::CONFLICT);
    let actions: Vec<String> = sqlx::query_scalar(
        "select action from audit_event where resource_type = 'review_item' and resource_ids = array[$1] order by id",
    )
    .bind(id.to_string())
    .fetch_all(&a.rt.pool)
    .await?;
    assert_eq!(actions, ["review_reject", "review_reopen", "review_dismiss"]);
    Ok(())
}

#[tokio::test]
async fn test_validation_answers_like_fastapi() -> R {
    let a = app().await?;
    let (s, v) = a.get("alex", "/review/x").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["detail"][0]["type"], "uuid_parsing");
    assert_eq!(v["detail"][0]["loc"], json!(["path", "item_id"]));
    assert_eq!(v["detail"][0]["input"], "x");
    let (s, v) = a.get("alex", "/review?limit=501&document_id=nope").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let types: Vec<Value> = v["detail"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|e| e["type"].clone())
        .collect();
    assert_eq!(types, [json!("less_than_equal"), json!("uuid_parsing")]);
    assert_eq!(v["detail"][0]["ctx"], json!({"le": 500}));
    let (s, v) = a
        .post(
            "alex",
            "/review/00000000-0000-0000-0000-000000000000/accept",
            Some(json!({"edits": [1]})),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v,
        json!({"detail": [{"type": "dict_type", "loc": ["body", "edits"], "msg": "Input should be a valid dictionary",
                           "input": [1]}]})
    );
    let (s, v) = a
        .post("alex", "/review/bulk-accept", Some(json!({"document_id": "nope"})))
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["detail"][0]["loc"], json!(["body", "document_id"]));
    Ok(())
}

#[tokio::test]
async fn test_bulk_accept_takes_only_clean_rows_of_a_document() -> R {
    use ownhealth_core::acceptance::Check;
    use ownhealth_core::review::{self, CandidateIn};
    let a = app().await?;
    let mut tx = a.rt.pool.begin().await?;
    let (doc, boxes) = lab_document(&mut tx, &a, "alex").await?;
    let run = review::create_extraction_run(&mut tx, doc, "rules:test", "1", b"x", None, None).await?;
    let mut queued = Vec::new();
    for (value, checks) in [
        ("97", vec![Check::pass("loinc")]),
        ("98", vec![Check::fail("loinc", "loinc_unknown")]),
    ] {
        let r = review::submit_candidate(
            &mut tx,
            run,
            &CandidateIn {
                page_no: 1,
                bbox: boxes[0],
                cells: json!({}),
                row_conf: 0.5,
                observation: lane_i::obs(a.family.profile("alex"), value, json!({})),
                checks,
            },
            "rules_text",
            "matched",
            0.95,
            false,
        )
        .await?;
        queued.push(r.review_item_id);
    }
    tx.commit().await?;
    let (s, v) = a
        .post("sam", "/review/bulk-accept", Some(json!({"document_id": doc})))
        .await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = a
        .post(
            "alex",
            "/review/bulk-accept",
            Some(json!({"document_id": doc, "min_row_conf": 0.9})),
        )
        .await?;
    assert_eq!((s, v), (StatusCode::OK, json!({"accepted": []})));
    let (s, v) = a
        .post("alex", "/review/bulk-accept", Some(json!({"document_id": doc})))
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({"accepted": [queued[0]]}));
    let (s, v) = a
        .post(
            "alex",
            "/review/bulk-accept",
            Some(json!({"document_id": Uuid::new_v4()})),
        )
        .await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        v,
        json!({"detail": {"code": "not_found", "message": "No such document."}})
    );
    let rows: i32 = sqlx::query_scalar(
        "select rows_out from audit_event where action = 'review_bulk_accept' order by id desc limit 1",
    )
    .fetch_one(&a.rt.pool)
    .await?;
    assert_eq!(rows, 1);
    Ok(())
}
