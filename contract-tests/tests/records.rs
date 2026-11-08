//! Records a person writes in the app (api/records.py), manual observations and how they show up
//! in observations, trends, the timeline and the status. Python: tests/imports/test_records_api.py,
//! tests/test_records.py, tests/normalize/test_norm_api.py.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{Client, src_ids};
use serde_json::{Value, json};

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn add_correct_retract_and_history_of_each_record_kind() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let bodies = [
        (
            "condition",
            json!({"text": "Asthma", "clinical_status": "active", "verification_status": "confirmed", "onset": "2001-05-01"}),
            "Asthma",
        ),
        (
            "medication",
            json!({"name": "Salbutamol", "strength": "100 mcg", "dose_text": "2 puffs as needed", "started": "2020-01-10"}),
            "Salbutamol 100 mcg",
        ),
        (
            "allergy",
            json!({"substance": "Penicillin", "criticality": "high", "reaction_text": "Hives"}),
            "Penicillin",
        ),
        (
            "immunization",
            json!({"vaccine": "Tetanus", "occurred": "2019-03-03", "dose_number": 3}),
            "Tetanus",
        ),
        (
            "procedure",
            json!({"text": "Appendectomy", "performed": "1999-07-07"}),
            "Appendectomy",
        ),
    ];
    for (kind, body, title) in &bodies {
        let r = c
            .post(&format!("/api/profiles/{p}/records/{kind}"), body)
            .await?
            .expect(201)?;
        assert_shape(&format!("record_added_{kind}"), &r.json)?;
        assert_eq!(r.json["kind"], *kind);
        assert_eq!(r.json["title"], *title);
        assert_eq!(r.json["status"], "current");
        assert_eq!(r.json["supersedes"], Value::Null);
        assert!(
            r.json["src"]
                .as_str()
                .is_some_and(|s| s.starts_with("src_"))
        );
        let id = r.json["id"].as_str().context("id")?.to_string();
        let list = c
            .get(&format!("/api/profiles/{p}/records/{kind}"))
            .await?
            .expect(200)?;
        assert_eq!(list.json["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(list.json["items"][0]["id"], json!(id));
        // the source of a manual record names the person who wrote it
        let src = r.json["src"].as_str().context("src")?;
        let s = c.get(&format!("/api/sources/{src}")).await?.expect(200)?;
        assert_eq!(s.json["kind"], "manual");
        assert_eq!(s.json["status"], "current");
        assert_eq!(s.json["document"], Value::Null);
        assert_eq!(s.json["location"], Value::Null);
    }

    // correct the medication: the old version is superseded, history shows both
    let meds = c
        .get(&format!("/api/profiles/{p}/records/medication"))
        .await?
        .expect(200)?
        .json;
    let old = meds["items"][0]["id"].as_str().context("id")?.to_string();
    let r = c
        .post(
            &format!("/api/profiles/{p}/records/medication/{old}/correct"),
            &json!({"name": "Salbutamol", "strength": "200 mcg", "reason": "wrong strength"}),
        )
        .await?
        .expect(201)?;
    assert_eq!(r.json["supersedes"], json!(old));
    assert_eq!(r.json["title"], "Salbutamol 200 mcg");
    let new_id = r.json["id"].as_str().context("id")?.to_string();
    let hist = c
        .get(&format!(
            "/api/profiles/{p}/records/medication?history=true"
        ))
        .await?
        .expect(200)?
        .json;
    let states: Vec<(String, String)> = hist["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            (
                i["id"].as_str().unwrap_or_default().to_string(),
                i["status"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        states,
        vec![
            (new_id.clone(), "current".into()),
            (old.clone(), "superseded".into())
        ]
    );
    assert_shape("records_history", &hist)?;

    // retract it: listed as retracted with its reason, gone from the current list
    let r = c
        .post(
            &format!("/api/profiles/{p}/records/medication/{new_id}/retract"),
            &json!({"reason": "  entered twice "}),
        )
        .await?
        .expect(200)?;
    assert_eq!(r.json["status"], "retracted");
    assert_eq!(r.json["status_reason"], "entered twice");
    let now = c
        .get(&format!("/api/profiles/{p}/records/medication"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(now["items"], json!([]));
    let r = c
        .post(
            &format!("/api/profiles/{p}/records/medication/{new_id}/retract"),
            &json!({"reason": ""}),
        )
        .await?;
    assert_eq!(r.status, 422);

    // bad input is the route's own 422 body, an unknown kind the path's 422, another profile's id 404
    let r = c
        .post(
            &format!("/api/profiles/{p}/records/allergy"),
            &json!({"criticality": "extreme"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (422, Some("bad_input")));
    assert_eq!(r.json["detail"]["message"], "Invalid record.");
    assert!(r.json["detail"]["errors"].is_array());
    let r = c
        .post(
            &format!("/api/profiles/{p}/records/diagnosis"),
            &json!({"text": "x"}),
        )
        .await?;
    assert_eq!(r.status, 422);
    let alex_conditions = c
        .get("/api/profiles/alex/records/condition")
        .await?
        .expect(200)?
        .json;
    let alex_id = alex_conditions["items"][0]["id"]
        .as_str()
        .context("alex condition")?;
    let r = c
        .post(
            &format!("/api/profiles/{p}/records/condition/{alex_id}/retract"),
            &json!({"reason": "x"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));

    let audit = c.audit(500).await?;
    let mine: Vec<&Value> = audit
        .iter()
        .filter(|e| e["profile"] == json!(p) && e["tool"] == "records")
        .collect();
    let actions: Vec<&str> = mine.iter().filter_map(|e| e["action"].as_str()).collect();
    for a in ["write", "read", "correct", "retract"] {
        assert!(actions.contains(&a), "no {a} audit row: {actions:?}");
    }
    assert!(mine.iter().any(|e| e["action"] == "correct"
        && e["resource_ids"] == json!([old, new_id])
        && e["rows_out"] == 2));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn manual_observations_resolve_and_show_everywhere() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let r = c
        .post(
            &format!("/api/profiles/{p}/manual-observations"),
            &json!({"name": "Colesterol LDL", "value": "162", "unit": "mg/dL", "effective_at": "2026-06-01T08:00:00+00:00", "category": "laboratory"}),
        )
        .await?
        .expect(201)?;
    assert_shape("manual_observation", &r.json)?;
    assert_eq!(r.json["alias_group"], "ldl_c");
    assert_eq!(
        (
            r.json["printed_name"].clone(),
            r.json["printed_value"].clone()
        ),
        (json!("Colesterol LDL"), json!("162"))
    );
    assert_eq!(r.json["status"], "current");
    let first = r.json["id"].as_str().context("id")?.to_string();

    // a correction supersedes it
    let r = c
        .post(
            &format!("/api/profiles/{p}/manual-observations"),
            &json!({"name": "Colesterol LDL", "value": "126", "unit": "mg/dL", "effective_at": "2026-06-01T08:00:00Z",
                    "corrects": first, "reason": "typo"}),
        )
        .await?
        .expect(201)?;
    assert_eq!(r.json["supersedes"], json!(first));
    c.post(
        &format!("/api/profiles/{p}/manual-observations"),
        &json!({"name": "LDL", "value": "3.1", "unit": "mmol/L", "effective_at": "2026-09-01T08:00:00Z"}),
    )
    .await?
    .expect(201)?;

    let obs = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    let items = obs["items"].as_array().context("items")?;
    assert_eq!(items.len(), 2, "the superseded value is not listed: {obs}");
    assert!(
        items
            .iter()
            .all(|i| i["accepted_by"] == "person" && i["alias_group"] == "ldl_c")
    );
    assert_shape("observations_manual", &obs)?;
    // newest first
    assert_eq!(items[0]["date"], "2026-09-01");
    let si = c
        .get(&format!("/api/profiles/{p}/observations?units=si"))
        .await?
        .expect(200)?
        .json;
    assert!(
        si["items"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|i| i["unit"] == "mmol/L"),
        "{si}"
    );
    let trend = c
        .get(&format!(
            "/api/profiles/{p}/trend?code=ldl_c&units=conventional"
        ))
        .await?
        .expect(200)?
        .json;
    assert_shape("trend_manual", &trend)?;
    let r = c
        .get(&format!(
            "/api/profiles/{p}/trend?code=nothing_like_this_xyz"
        ))
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));
    let r = c.get(&format!("/api/profiles/{p}/trend")).await?;
    assert_eq!(r.status, 422);

    // the timeline and the status see them, with src ids that resolve
    let tl = c
        .get(&format!("/api/profiles/{p}/timeline"))
        .await?
        .expect(200)?
        .json;
    assert!(
        !tl["events"].as_array().map(Vec::is_empty).unwrap_or(true),
        "{tl}"
    );
    let st = c
        .get(&format!("/api/profiles/{p}/status"))
        .await?
        .expect(200)?
        .json;
    for src in src_ids(&st["data"]).iter().take(5) {
        c.get(&format!("/api/sources/{src}")).await?.expect(200)?;
    }

    // naive datetimes and bad LOINC codes are refused; another profile's observation is 404
    let r = c
        .post(
            &format!("/api/profiles/{p}/manual-observations"),
            &json!({"name": "X", "value": "1", "effective_at": "2026-06-01T08:00:00"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (422, Some("bad_input")));
    let r = c
        .post(
            &format!("/api/profiles/{p}/manual-observations"),
            &json!({"name": "X", "value": "1", "effective_at": "2026-06-01T08:00:00Z", "loinc": "abc"}),
        )
        .await?;
    assert_eq!(r.status, 422);
    let alex = c
        .get("/api/profiles/alex/observations?q=ldl")
        .await?
        .expect(200)?
        .json;
    let alex_id = alex["items"][0]["id"].as_str().context("alex obs")?;
    let r = c
        .post(
            &format!("/api/profiles/{p}/manual-observations"),
            &json!({"name": "X", "value": "1", "effective_at": "2026-06-01T08:00:00Z", "corrects": alex_id}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn household_aliases_map_new_names() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let r = c
        .post("/api/aliases", &json!({"profile": p, "alias": "Kolesterol Jahat", "alias_group": "ldl_c", "lang": "es", "unit": "mg/dL"}))
        .await?
        .expect(200)?;
    assert_shape("alias_added", &r.json)?;
    assert_eq!(r.json["alias_group"], "ldl_c");
    // the alias re-derives every household profile, so the count depends on what other tests left
    assert!(r.json["rederived"].is_u64(), "{}", r.json);
    let r = c
        .post(
            &format!("/api/profiles/{p}/manual-observations"),
            &json!({"name": "Kolesterol Jahat", "value": "140", "unit": "mg/dL", "effective_at": "2026-06-01T08:00:00Z"}),
        )
        .await?
        .expect(201)?;
    assert_eq!(r.json["alias_group"], "ldl_c");
    let r = c
        .post(
            "/api/aliases",
            &json!({"profile": p, "alias": "Foo", "alias_group": "no_such_group_xyz"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (422, Some("bad_input")));
    assert_eq!(
        r.json["detail"]["message"],
        "unknown alias_group 'no_such_group_xyz'"
    );
    Ok(())
}
