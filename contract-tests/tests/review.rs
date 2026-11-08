//! The review queue over REST: accept (with edits), reject, reopen, dismiss, bulk-accept, and the
//! document lifecycle around it (assign to another profile, retract). Python: tests/test_review.py,
//! tests/security/test_review_scoping.py, tests/pdf/test_pdf_api.py, tests/imports/test_document_retract.py.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{Client, patient_line, pdf};
use serde_json::{Value, json};

/// A profile with one LabNorte report: LDL and glucose accepted, the white cells held for review.
async fn held_row(c: &Client) -> anyhow::Result<(String, String, String, String)> {
    let (p, name) = c.new_patient().await?;
    let rows = [
        pdf::Row("Colesterol LDL", "162", "mg/dL", "< 130", "H"),
        pdf::Row("Glucosa", "92", "mg/dL", "70 - 110", ""),
        pdf::Row("Leucocitos", "6800", "x10^3/µL", "4.0 - 11.0", ""),
    ];
    let doc = pdf::lab_report(
        &patient_line(&name),
        "02/04/1980",
        "14/09/2026",
        "LN-R",
        &rows,
    );
    let (up, jobs) = c
        .import(&p, &[("labnorte.pdf", &doc, "application/pdf")])
        .await?;
    anyhow::ensure!(
        jobs.first().is_some_and(|j| j["state"] == "done"),
        "import failed: {jobs:?}"
    );
    let d = up["items"][0]["document_id"]
        .as_str()
        .context("doc")?
        .to_string();
    let q = c
        .get(&format!("/api/review?profile={p}"))
        .await?
        .expect(200)?
        .json;
    let item = q["items"][0]["id"]
        .as_str()
        .context("held item")?
        .to_string();
    Ok((p, name, d, item))
}

fn count(v: &Value) -> usize {
    v["items"].as_array().map(Vec::len).unwrap_or(0)
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn accept_with_an_edit_writes_the_corrected_fact() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, _, _, item) = held_row(&c).await?;
    // edits never move a fact
    let r = c
        .post(
            &format!("/api/review/{item}/accept"),
            &json!({"edits": {"profile_id": "00000000-0000-4000-8000-000000000000"}}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("conflict")));
    assert_eq!(
        r.json["detail"]["message"],
        "edits cannot change profile_id"
    );
    let r = c
        .post(&format!("/api/review/{item}/accept"), &json!({"edits": {"value_num": "6.8", "printed_value": "6.8", "value_canonical": "6.8"}}))
        .await?
        .expect(200)?;
    assert_eq!(r.json["id"], json!(item));
    assert_eq!(r.json["status"], "accepted");
    let created = r.json["result_ids"].as_array().context("result_ids")?;
    assert_eq!(created.len(), 1);
    let obs = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    let wbc = obs["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["alias_group"] == "wbc")
        .context("wbc fact")?;
    assert_eq!(wbc["id"], created[0]);
    assert_eq!(wbc["accepted_by"], "person");
    assert_eq!(wbc["printed"]["value"], "6.8");
    // the item is decided; deciding again is a conflict
    let got = c
        .get(&format!("/api/review/{item}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(got["status"], "accepted");
    assert_eq!(got["result_ids"], json!(created));
    assert!(got["decided_by"].is_string() && got["decided_at"].is_string());
    let r = c
        .post(&format!("/api/review/{item}/accept"), &json!({}))
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("conflict")));
    let q = c
        .get(&format!("/api/review?profile={p}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!((q["open"].clone(), count(&q)), (json!(0), 0));
    let all = c
        .get(&format!("/api/review?profile={p}&status="))
        .await?
        .expect(200)?
        .json;
    assert_eq!(count(&all), 1);
    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "review_accept"
        && e["tool"] == "review"
        && e["resource_ids"] == json!([item])));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn reject_reopen_and_dismiss() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, _, _, item) = held_row(&c).await?;
    let r = c
        .post(
            &format!("/api/review/{item}/reject"),
            &json!({"reason": "  "}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("conflict")));
    let r = c
        .post(&format!("/api/review/{item}/reject"), &json!({}))
        .await?;
    assert_eq!(r.status, 422);
    let r = c
        .post(
            &format!("/api/review/{item}/reject"),
            &json!({"reason": " not mine "}),
        )
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"id": item, "status": "rejected"}));
    let got = c
        .get(&format!("/api/review/{item}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        (got["status"].clone(), got["decision_reason"].clone()),
        (json!("rejected"), json!("not mine"))
    );
    // a rejected row never becomes a fact
    let obs = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    assert!(
        !obs["items"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|o| o["alias_group"] == "wbc")
    );
    let rejected = c
        .get(&format!("/api/review?profile={p}&status=rejected"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(count(&rejected), 1);
    // reopen only a rejected item
    let r = c
        .post(&format!("/api/review/{item}/reopen"), &json!({}))
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"id": item, "status": "open"}));
    let got = c
        .get(&format!("/api/review/{item}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        (
            got["status"].clone(),
            got["decided_by"].clone(),
            got["decision_reason"].clone()
        ),
        (json!("open"), Value::Null, Value::Null)
    );
    let r = c
        .post(&format!("/api/review/{item}/reopen"), &json!({}))
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("conflict")));
    // dismiss works on any open item
    let r = c
        .post(
            &format!("/api/review/{item}/dismiss"),
            &json!({"reason": "duplicate"}),
        )
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"id": item, "status": "dismissed"}));
    let got = c
        .get(&format!("/api/review/{item}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        (got["status"].clone(), got["decision_reason"].clone()),
        (json!("dismissed"), json!("duplicate"))
    );
    let audit = c.audit(500).await?;
    for a in ["review_reject", "review_reopen", "review_dismiss"] {
        assert!(
            audit
                .iter()
                .any(|e| e["action"] == a && e["resource_ids"] == json!([item])),
            "no {a}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn job_failed_items_are_dismissed_not_accepted() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let bad = serde_json::to_vec(&json!({"schema": "ownhealth.genome-facts/v9"}))?;
    c.import(&p, &[("bad.json", &bad, "application/json")])
        .await?;
    let q = c
        .get(&format!("/api/review?profile={p}&kind=job_failed"))
        .await?
        .expect(200)?
        .json;
    let item = q["items"][0]["id"].as_str().context("job_failed")?;
    let r = c
        .post(&format!("/api/review/{item}/accept"), &json!({}))
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("conflict")));
    assert_eq!(
        r.json["detail"]["message"],
        "items of kind job_failed cannot be accepted; dismiss them instead"
    );
    let r = c
        .post(&format!("/api/review/{item}/dismiss"), &json!(null))
        .await?
        .expect(200)?;
    assert_eq!(r.json["status"], "dismissed");
    assert_eq!(
        c.get(&format!("/api/review/{item}")).await?.json["decision_reason"],
        Value::Null
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn items_outside_the_callers_reach_are_404() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, _, _, item) = held_row(&c).await?;
    let m = c
        .enrolled_member(&format!("Outsider {}", ownhealth_contract_tests::uniq()))
        .await?;
    for (method, path) in [
        ("GET", format!("/api/review/{item}")),
        ("POST", format!("/api/review/{item}/accept")),
    ] {
        let r = if method == "GET" {
            m.client.get(&path).await?
        } else {
            m.client.post(&path, &json!({})).await?
        };
        assert_eq!(
            (r.status, r.code()),
            (404, Some("not_found")),
            "{method} {path}"
        );
        assert_eq!(r.json["detail"]["message"], "No such review item.");
    }
    let r = c
        .get("/api/review/00000000-0000-4000-8000-000000000000")
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("not_found")));
    let r = c.get("/api/review/not-a-uuid").await?;
    assert_eq!(r.status, 422);
    let q = m.client.get("/api/review").await?.expect(200)?.json;
    assert_eq!(q, json!({"items": [], "open": 0, "withheld": 0}));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn bulk_accept_takes_only_clean_rows() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, _, d, item) = held_row(&c).await?;
    let r = c
        .post("/api/review/bulk-accept", &json!({"document_id": d}))
        .await?
        .expect(200)?;
    assert_eq!(
        r.json,
        json!({"accepted": []}),
        "the held row has reasons: it stays open"
    );
    assert_eq!(
        c.get(&format!("/api/review/{item}")).await?.json["status"],
        "open"
    );
    let r = c
        .post(
            "/api/review/bulk-accept",
            &json!({"document_id": "00000000-0000-4000-8000-000000000000"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("not_found")));
    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "review_bulk_accept"
        && e["resource_type"] == "document"
        && e["resource_ids"] == json!([d])
        && e["rows_out"] == 0
        && e["profile"] == json!(p)));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn retracting_a_document_withdraws_its_facts_and_items() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, _, d, item) = held_row(&c).await?;
    let r = c
        .post(
            &format!("/api/documents/{d}/retract"),
            &json!({"reason": "  "}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (422, Some("bad_input")));
    let r = c
        .post(
            &format!("/api/documents/{d}/retract"),
            &json!({"reason": "wrong patient"}),
        )
        .await?
        .expect(200)?;
    assert_shape("document_retracted", &r.json)?;
    let obs = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        obs["items"],
        json!([]),
        "retracted facts leave the current view"
    );
    let got = c
        .get(&format!("/api/review/{item}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        (got["status"].clone(), got["decision_reason"].clone()),
        (json!("dismissed"), json!("document retracted"))
    );
    let doc = c
        .get(&format!("/api/documents/{d}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(doc["status"], "retracted");
    let r = c
        .post(
            &format!("/api/documents/{d}/retract"),
            &json!({"reason": "again"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("conflict")));
    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "delete"
        && e["tool"] == "POST /api/documents/{id}/retract"
        && e["resource_ids"] == json!([d])));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn assigning_a_document_moves_it_and_reimports() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, _, d, _) = held_row(&c).await?;
    let (q, _) = c.new_patient().await?;
    let r = c
        .post(
            &format!("/api/documents/{d}/assign"),
            &json!({"profile": q}),
        )
        .await?
        .expect(200)?;
    assert_eq!(
        (r.json["document_id"].clone(), r.json["profile"].clone()),
        (json!(d), json!(q))
    );
    let job = r.json["job_id"].as_i64().context("job_id")?;
    let jobs = c.wait_jobs(&[job]).await?;
    assert_eq!(
        (jobs[0]["kind"].clone(), jobs[0]["state"].clone()),
        (json!("import.pdf"), json!("done"))
    );
    let old = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(old["items"], json!([]));
    let docs = c
        .get(&format!("/api/profiles/{q}/documents"))
        .await?
        .expect(200)?
        .json;
    assert!(
        docs["items"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|x| x["id"] == json!(d))
    );
    // a viewer's profile is not a target
    let r = c
        .post(
            &format!("/api/documents/{d}/assign"),
            &json!({"profile": "sam"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("forbidden")));
    Ok(())
}
