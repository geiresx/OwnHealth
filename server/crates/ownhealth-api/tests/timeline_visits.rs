//! Timeline 0.4.0: visits typed by hand or imported as a FHIR Encounter, non-lab documents and
//! procedures as events, and the bars lane (`spans`): one row per condition from onset to
//! abatement, and one row per medicine with a segment per course. Same scopes and lifecycle as the
//! other events; the same request gives the same bytes.

mod lane_i;

use axum::body::Body;
use http::{Request, StatusCode, header};
use lane_i::{App, R, app, manual_src};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_ingest_structured::fhir::mapping::{Mapped, encounter};
use serde_json::{Value, json};

async fn record(c: &mut sqlx::PgConnection, kind: &str, v: Value) -> anyhow::Result<uuid::Uuid> {
    let r = RecordIn::from_json(kind, v)?;
    Ok(
        add_record(c, &r, &SrcRef::New(manual_src()), RecordOptions::new("person"))
            .await?
            .id,
    )
}

/// The demo's records for Alex (two atorvastatin courses with a pause, the 2021 diagnosis), plus a
/// sensitive diagnosis, a procedure, an imported Encounter and two documents.
async fn seed(a: &App) -> R {
    let alex = a.family.profile("alex");
    let mut tx = a.rt.pool.begin().await?;
    for body in [
        json!({"profile_id": alex, "name": "Atorvastatin", "strength": "20 mg", "started": "2022-06-20",
               "stopped": "2025-12-01", "med_status": "stopped", "reason_text": "paused"}),
        json!({"profile_id": alex, "name": "Atorvastatin", "ingredient": "atorvastatin", "strength": "20 mg",
               "started": "2026-03-10"}),
        json!({"profile_id": alex, "name": "Omeprazole", "strength": "20 mg", "started": "2026-04-01"}),
    ] {
        record(&mut tx, "medication", body).await?;
    }
    record(
        &mut tx,
        "condition",
        json!({"profile_id": alex, "text": "Hypercholesterolaemia", "code_system": "icd10", "code": "E78.0",
               "verification_status": "confirmed", "onset": "2021-11-09"}),
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
        "condition",
        json!({"profile_id": alex, "text": "Ruled out", "onset": "2020-02-02", "verification_status": "refuted"}),
    )
    .await?;
    record(
        &mut tx,
        "procedure",
        json!({"profile_id": alex, "text": "Knee arthroscopy", "code_system": "snomed", "code": "76916001",
               "performed": "2025-12-20"}),
    )
    .await?;
    // A FHIR Encounter, mapped as the importer maps it.
    let resource = json!({
        "resourceType": "Encounter", "id": "enc-1", "status": "finished", "class": {"code": "AMB"},
        "period": {"start": "2026-05-12T08:30:00+02:00", "end": "2026-05-12T09:00:00+02:00"},
        "serviceProvider": {"display": "Centro de Salud Norte"},
        "reasonCode": [{"text": "Blood pressure review"}]
    });
    let Mapped::Record { record: visit, checks } = encounter(&resource, alex) else {
        anyhow::bail!("an Encounter maps to a visit");
    };
    assert!(checks.is_empty(), "{checks:?}");
    let src: SrcIn = serde_json::from_value(json!({
        "kind": "fhir_import", "profile_id": alex, "import_ref": "ips-1/Encounter/enc-1", "label": "IPS bundle"
    }))?;
    add_record(&mut tx, &visit, &SrcRef::New(src), RecordOptions::new("import")).await?;
    // A discharge report is a document event; a lab report is already a lab event.
    for (doc_type, name, day, sha) in [
        ("discharge", "Informe_alta_2025-11-12.pdf", "2025-11-12", "aa"),
        ("lab_report", "LabNorte_2026-09-14.pdf", "2026-09-14", "bb"),
    ] {
        sqlx::query(
            "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, \
             provider_name, collected_at) values ($1, $2, decode($3, 'hex'), $4, 'application/pdf', 10, $5, \
             'Hospital Norte', ($6::date + time '10:00') at time zone 'UTC')",
        )
        .bind(a.family.household_id)
        .bind(alex)
        .bind(sha.repeat(32))
        .bind(name)
        .bind(doc_type)
        .bind(day)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn of_type(v: &Value, t: &str) -> Vec<Value> {
    v["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == t)
        .cloned()
        .collect()
}

/// (label, [(date, end)]) of each span of one type.
fn spans(v: &Value, t: &str) -> Vec<(String, Vec<(Value, Value)>)> {
    v["spans"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["type"] == t)
        .map(|s| {
            let segs = s["segments"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|g| (g["date"].clone(), g["end"].clone()))
                .collect();
            (s["label"].as_str().unwrap_or_default().to_string(), segs)
        })
        .collect()
}

#[tokio::test]
async fn a_visit_is_a_record_kind_and_needs_its_day() -> R {
    let a = app().await?;
    let body = json!({"period_start": "2025-11-10T12:00:00Z", "reason_text": "Knee pain",
                      "provider_name": "Traumatología Centro"});
    let (s, v) = a.post("alex", "/profiles/alex/records/visit", Some(body)).await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!((&v["kind"], &v["title"]), (&json!("visit"), &json!("Knee pain")));
    assert_eq!(v["detail"]["period_start"], "2025-11-10T12:00:00Z");
    assert!(v["src"].as_str().is_some_and(|s| s.starts_with("src_")));
    let (_, list) = a.get("alex", "/profiles/alex/records/visit").await?;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(1));
    // the stored FHIR is an R4 Encounter built from the typed columns
    let fhir: Value = sqlx::query_scalar("select fhir from encounter where profile_id = $1")
        .bind(a.family.profile("alex"))
        .fetch_one(&a.rt.pool)
        .await?;
    assert_eq!(fhir["resourceType"], "Encounter");
    assert_eq!(fhir["class"]["code"], "AMB");
    assert_eq!(fhir["period"]["start"], "2025-11-10T12:00:00+00:00");
    assert_eq!(fhir["serviceProvider"]["display"], "Traumatología Centro");
    // a visit without its day has no place on the timeline: refused
    let (s, v) = a
        .post(
            "alex",
            "/profiles/alex/records/visit",
            Some(json!({"reason_text": "x"})),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, v) = a
        .post(
            "alex",
            "/profiles/alex/records/visit",
            Some(json!({"period_start": "2025-11-10T12:00:00Z", "period_end": "2025-11-09T12:00:00Z"})),
        )
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    Ok(())
}

#[tokio::test]
async fn the_timeline_shows_visits_documents_and_bars() -> R {
    let a = app().await?;
    seed(&a).await?;
    let (s, v) = a
        .post(
            "alex",
            "/profiles/alex/records/visit",
            Some(json!({"period_start": "2025-11-10T12:00:00Z", "provider_name": "Traumatología Centro"})),
        )
        .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, out) = a.get("alex", "/profiles/alex/timeline?lang=en").await?;
    assert_eq!(s, StatusCode::OK, "{out}");

    // Visits: the one typed by hand and the imported Encounter, each with its src.
    let visits = of_type(&out, "visit");
    let got: Vec<(Value, Value, Value)> = visits
        .iter()
        .map(|e| (e["date"].clone(), e["title"].clone(), e["provider"].clone()))
        .collect();
    assert_eq!(
        got,
        [
            (
                json!("2026-05-12"),
                json!("Visit · Blood pressure review"),
                json!("Centro de Salud Norte")
            ),
            (json!("2025-11-10"), json!("Visit"), json!("Traumatología Centro")),
        ]
    );
    assert!(
        visits
            .iter()
            .all(|e| e["src"].as_str().is_some_and(|s| s.starts_with("src_")))
    );
    assert_eq!(visits[0]["src_label"], "IPS bundle");

    // Documents: the discharge report, not the lab report; the filename goes out as untrusted text.
    let docs = of_type(&out, "document");
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert_eq!(
        (&docs[0]["date"], &docs[0]["title"]),
        (&json!("2025-11-12"), &json!("Discharge report"))
    );
    assert_eq!(
        docs[0]["filename"],
        json!({"trust": "untrusted_text", "origin": "filename", "text": "Informe_alta_2025-11-12.pdf"})
    );
    assert!(docs[0]["document_id"].is_string());
    let procs = of_type(&out, "procedure");
    assert_eq!(procs.len(), 1);
    assert_eq!(
        (&procs[0]["title"], &procs[0]["detail"]),
        (&json!("Knee arthroscopy"), &json!("SNOMED 76916001"))
    );

    // Bars: one row per medicine, a segment per course; one row per condition; refuted ones left out.
    assert_eq!(
        spans(&out, "medication"),
        [
            (
                "Atorvastatin".to_string(),
                vec![
                    (json!("2022-06-20"), json!("2025-12-01")),
                    (json!("2026-03-10"), Value::Null)
                ]
            ),
            ("Omeprazole".to_string(), vec![(json!("2026-04-01"), Value::Null)]),
        ]
    );
    assert_eq!(
        spans(&out, "diagnosis"),
        [
            (
                "Hypercholesterolaemia".to_string(),
                vec![(json!("2021-11-09"), Value::Null)]
            ),
            ("Hidden thing".to_string(), vec![(json!("2023-01-01"), Value::Null)]),
        ]
    );
    for s in out["spans"].as_array().into_iter().flatten() {
        for g in s["segments"].as_array().into_iter().flatten() {
            assert!(g["src"].as_str().is_some_and(|s| s.starts_with("src_")), "{g}");
            assert!(g["label"].is_string() && g.get("src_label").is_some(), "{g}");
        }
    }
    let atorva = &out["spans"][2]["segments"][0]["label"];
    assert_eq!(atorva, "Atorvastatin 20 mg");

    // A window keeps the bars that overlap it, and the type filter applies to them.
    let (_, w) = a
        .get("alex", "/profiles/alex/timeline?types=medication&from=2026-01-01")
        .await?;
    assert_eq!(
        spans(&w, "medication"),
        [
            ("Atorvastatin".to_string(), vec![(json!("2026-03-10"), Value::Null)]),
            ("Omeprazole".to_string(), vec![(json!("2026-04-01"), Value::Null)]),
        ]
    );
    assert!(spans(&w, "diagnosis").is_empty());

    // Spanish words for the new events.
    let (_, es) = a.get("alex", "/profiles/alex/timeline?lang=es").await?;
    assert_eq!(of_type(&es, "document")[0]["title"], "Informe de alta");
    assert_eq!(of_type(&es, "visit")[1]["title"], "Visita médica");

    // The same request gives the same bytes.
    let raw = || async {
        let req = Request::get("/profiles/alex/timeline?lang=en")
            .header(header::COOKIE, a.cookie("alex").await?)
            .body(Body::empty())?;
        a.send_raw(req).await
    };
    let (s1, _, b1) = raw().await?;
    let (s2, _, b2) = raw().await?;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
    assert_eq!(b1, b2);
    Ok(())
}

#[tokio::test]
async fn without_records_scope_no_bars_visits_or_documents() -> R {
    let a = app().await?;
    seed(&a).await?;
    let alex = a.family.profile("alex");
    let categories = |cats: &'static [&'static str]| {
        sqlx::query("update profile_grant set categories = $1 where user_id = $2 and profile_id = $3")
            .bind(cats.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .bind(a.family.user("alex"))
            .bind(alex)
            .execute(&a.rt.pool)
    };
    categories(&["read:labs"]).await?;
    let (s, out) = a.get("alex", "/profiles/alex/timeline").await?;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(out["spans"], json!([]));
    assert!(of_type(&out, "visit").is_empty() && of_type(&out, "document").is_empty());
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
    // A discharge report whose diagnosis is sensitive: the document is as sensitive as its facts.
    let mut tx = a.rt.pool.begin().await?;
    let doc: uuid::Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type, \
         provider_name, collected_at) values ($1, $2, decode(repeat('cc', 32), 'hex'), 'Alta_hidden_2023.pdf', \
         'application/pdf', 10, 'discharge', 'Hospital Norte', '2023-01-05T10:00:00Z') returning id",
    )
    .bind(a.family.household_id)
    .bind(alex)
    .fetch_one(&mut *tx)
    .await?;
    let src: SrcIn = serde_json::from_value(json!({"kind": "manual", "profile_id": alex, "document_id": doc}))?;
    let r = RecordIn::from_json(
        "condition",
        json!({"profile_id": alex, "text": "Hidden from the report", "onset": "2023-01-05",
               "sensitivity": ["mental_health"]}),
    )?;
    add_record(&mut tx, &r, &SrcRef::New(src), RecordOptions::new("person")).await?;
    tx.commit().await?;
    let names = |v: &Value| -> Vec<String> {
        of_type(v, "document")
            .iter()
            .map(|d| d["filename"]["text"].as_str().unwrap_or_default().to_string())
            .collect()
    };

    // Records without read:sensitive: the sensitive diagnosis has no bar, and its report is not listed.
    categories(&["read:records"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline").await?;
    let labels: Vec<String> = spans(&out, "diagnosis").into_iter().map(|(l, _)| l).collect();
    assert_eq!(labels, ["Hypercholesterolaemia"]);
    assert_eq!(names(&out), ["Informe_alta_2025-11-12.pdf"]);
    categories(&["read:records", "read:sensitive"]).await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline").await?;
    assert_eq!(names(&out), ["Informe_alta_2025-11-12.pdf", "Alta_hidden_2023.pdf"]);
    Ok(())
}

/// Two courses of one medicine that start on the same day: the row's name does not depend on the
/// order the rows sit on disk, so two calls give the same bytes.
#[tokio::test]
async fn a_medicine_row_has_one_name_when_two_courses_start_together() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let mut tx = a.rt.pool.begin().await?;
    // stored in the reverse of name order: a query without an order returns them as stored
    for name in ["Cardyl", "Atorvastatin"] {
        record(
            &mut tx,
            "medication",
            json!({"profile_id": alex, "name": name, "ingredient": "atorvastatin", "started": "2026-03-10"}),
        )
        .await?;
    }
    tx.commit().await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline?types=medication").await?;
    let rows = spans(&out, "medication");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "Cardyl");
    Ok(())
}

/// A course on hold with no stop date is not running now: it never reads as ongoing.
#[tokio::test]
async fn a_course_on_hold_is_not_ongoing() -> R {
    let a = app().await?;
    let alex = a.family.profile("alex");
    let mut tx = a.rt.pool.begin().await?;
    record(
        &mut tx,
        "medication",
        json!({"profile_id": alex, "name": "Metformin", "started": "2025-01-15", "med_status": "on_hold"}),
    )
    .await?;
    tx.commit().await?;
    let (_, out) = a.get("alex", "/profiles/alex/timeline?types=medication").await?;
    assert_eq!(
        spans(&out, "medication"),
        [(
            "Metformin".to_string(),
            vec![(json!("2025-01-15"), json!("2025-01-15"))]
        )]
    );
    Ok(())
}
