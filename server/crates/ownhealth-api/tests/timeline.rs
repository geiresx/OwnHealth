//! Lane I: `api/timeline.py` (tests/timeline/test_timeline.py): draws grouped, records dated,
//! lifecycle and scopes respected, audited.
//!
//! Python narrowed the scopes with a token principal through a dependency override; REST refuses
//! tokens here, so the same narrowing comes from the grant's categories (effective scopes = token
//! scopes ∩ grant categories − locks).

mod lane_i;

use http::StatusCode;
use lane_i::{App, R, app, manual_src, obs};
use ownhealth_core::observations::{CommitOptions, commit_observation};
use ownhealth_core::provenance::SrcRef;
use ownhealth_core::records::{RecordIn, RecordOptions, add_record, retract_record};
use serde_json::{Value, json};

async fn record(c: &mut sqlx::PgConnection, kind: &str, v: Value) -> anyhow::Result<uuid::Uuid> {
    let r = RecordIn::from_json(kind, v)?;
    Ok(
        add_record(c, &r, &SrcRef::New(manual_src()), RecordOptions::new("person"))
            .await?
            .id,
    )
}

async fn seed(a: &App) -> R {
    let alex = a.family.profile("alex");
    let mut tx = a.rt.pool.begin().await?;
    let person = || CommitOptions::new("person");
    // One draw: two values sharing a report_key, one flagged by the lab.
    for (value, group, flag) in [("97", "ldl_c", Value::Null), ("24", "vitamin_d", json!("L"))] {
        let o = obs(
            alex,
            value,
            json!({"alias_group": group, "interpretation": flag, "report_key": "LN-1"}),
        );
        commit_observation(&mut tx, &o, &SrcRef::New(manual_src()), person()).await?;
    }
    let o = obs(alex, "118", json!({"day": "2026-03-02"}));
    commit_observation(&mut tx, &o, &SrcRef::New(manual_src()), person()).await?;
    record(
        &mut tx,
        "medication",
        json!({"profile_id": alex, "name": "Atorvastatin", "strength": "20 mg", "started": "2022-06-20",
               "stopped": "2025-12-01", "reason_text": "paused"}),
    )
    .await?;
    let gone = record(
        &mut tx,
        "medication",
        json!({"profile_id": alex, "name": "Wrongpril", "started": "2024-01-01"}),
    )
    .await?;
    retract_record(&mut tx, "medication", gone, "entered in error").await?;
    record(
        &mut tx,
        "condition",
        json!({"profile_id": alex, "text": "Hypercholesterolaemia", "code_system": "icd10", "code": "E78.0",
               "onset": "2021-11-09"}),
    )
    .await?;
    record(
        &mut tx,
        "condition",
        json!({"profile_id": alex, "text": "Hidden thing", "onset": "2023-01-01", "sensitivity": ["mental_health"]}),
    )
    .await?;
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": alex, "vaccine": "Td booster", "occurred": "2016-05-10"}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

fn events(v: &Value) -> Vec<Value> {
    v["events"].as_array().cloned().unwrap_or_default()
}

fn strs(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn test_timeline_groups_draws_and_dates_records() -> R {
    let a = app().await?;
    seed(&a).await?;
    let (s, out) = a.get("alex", "/profiles/alex/timeline?lang=en").await?;
    assert_eq!(s, StatusCode::OK, "{out}");
    let ev = events(&out);
    let dates: Vec<String> = ev.iter().map(|e| strs(e, "date")).collect();
    let mut sorted = dates.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(dates, sorted);
    let labs: Vec<&Value> = ev.iter().filter(|e| e["type"] == "lab").collect();
    let got: Vec<(String, String)> = labs.iter().map(|e| (strs(e, "date"), strs(e, "title"))).collect();
    assert_eq!(
        got,
        [
            ("2026-09-14".to_string(), "Lab report · 2 results".to_string()),
            ("2026-03-02".to_string(), "Lab report · 1 result".to_string())
        ]
    );
    assert_eq!(labs[0]["detail"], "1 flagged by the lab");
    assert_eq!(labs[1]["detail"], Value::Null);
    let mut meds: Vec<(String, String)> = ev
        .iter()
        .filter(|e| e["type"] == "medication")
        .map(|e| (strs(e, "date"), strs(e, "title")))
        .collect();
    meds.sort();
    assert_eq!(
        meds,
        [
            ("2022-06-20".to_string(), "Atorvastatin 20 mg started".to_string()),
            ("2025-12-01".to_string(), "Atorvastatin 20 mg stopped".to_string())
        ]
    );
    let mut dx: Vec<String> = ev
        .iter()
        .filter(|e| e["type"] == "diagnosis")
        .map(|e| strs(e, "title"))
        .collect();
    dx.sort();
    assert_eq!(dx, ["Hidden thing", "Hypercholesterolaemia"]);
    let hyper = ev
        .iter()
        .find(|e| e["title"] == "Hypercholesterolaemia")
        .cloned()
        .unwrap_or_default();
    assert_eq!(hyper["detail"], "ICD10 E78.0 · provisional");
    let vaccines: Vec<String> = ev
        .iter()
        .filter(|e| e["type"] == "vaccine")
        .map(|e| strs(e, "date"))
        .collect();
    assert_eq!(vaccines, ["2016-05-10"]);
    assert!(ev.iter().all(|e| strs(e, "src").starts_with("src_")));
    let keys: Vec<&String> = ev[0].as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        [
            "date",
            "type",
            "title",
            "detail",
            "country",
            "flags",
            "src",
            "src_label",
            "provider"
        ]
    );
    assert_eq!(out["withheld"], json!({"by_scope": []}));
    let (_, es) = a.get("alex", "/profiles/alex/timeline?lang=es&types=lab").await?;
    assert_eq!(events(&es)[0]["title"], "Analítica · 2 resultados");
    assert_eq!(events(&es)[0]["detail"], "1 marcado por el laboratorio");
    Ok(())
}

#[tokio::test]
async fn test_timeline_filters_scopes_and_access() -> R {
    let a = app().await?;
    seed(&a).await?;
    let (_, only) = a
        .get(
            "alex",
            "/profiles/alex/timeline?types=vaccine,diagnosis&from=2020-01-01&to=2022-01-01",
        )
        .await?;
    let got: Vec<(String, String)> = events(&only)
        .iter()
        .map(|e| (strs(e, "type"), strs(e, "date")))
        .collect();
    assert_eq!(got, [("diagnosis".to_string(), "2021-11-09".to_string())]);
    let (s, v) = a.get("alex", "/profiles/alex/timeline?types=bogus,lab,zzz").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v,
        json!({"detail": {"code": "bad_input", "message": "Unknown types: ['bogus', 'zzz']"}})
    );
    let (s, v) = a.get("alex", "/profiles/alex/timeline?from=yesterday").await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["detail"][0]["loc"], json!(["query", "from"]));

    // Labs only: no records, no sensitive rows, and the withheld types are named.
    let alex = a.family.profile("alex");
    let categories = |cats: &'static [&'static str]| {
        sqlx::query("update profile_grant set categories = $1 where user_id = $2 and profile_id = $3")
            .bind(cats.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .bind(a.family.user("alex"))
            .bind(alex)
            .execute(&a.rt.pool)
    };
    categories(&["read:labs"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline").await?;
    assert!(events(&out).iter().all(|e| e["type"] == "lab"), "{out}");
    assert!(!events(&out).is_empty());
    assert_eq!(
        out["withheld"]["by_scope"],
        json!([
            "diagnosis",
            "document",
            "genome",
            "imaging",
            "medication",
            "procedure",
            "vaccine",
            "visit"
        ])
    );
    // Records only: the sensitive diagnosis stays out.
    categories(&["read:records"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline").await?;
    let titles: Vec<String> = events(&out).iter().map(|e| strs(e, "title")).collect();
    assert!(titles.iter().any(|t| t == "Hypercholesterolaemia") && !titles.iter().any(|t| t == "Hidden thing"));

    // Sam has no grant on Carmen.
    let (s, v) = a.get("sam", "/profiles/carmen/timeline").await?;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let n = a
        .count(&format!(
            "select count(*) from audit_event where resource_type = 'timeline' and profile_id = '{alex}'"
        ))
        .await?;
    assert!(n >= 3, "{n}");
    let withheld: Option<Value> = sqlx::query_scalar(
        "select withheld from audit_event where resource_type = 'timeline' and withheld is not null order by id limit 1",
    )
    .fetch_one(&a.rt.pool)
    .await?;
    assert_eq!(
        withheld,
        Some(
            json!({"by_scope": ["diagnosis", "document", "genome", "imaging", "medication", "procedure", "vaccine", "visit"]})
        )
    );
    Ok(())
}
