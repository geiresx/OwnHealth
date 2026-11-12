//! FHIR R4 import: IPS document bundles, US Core collections, NDJSON, review routing, idempotency
//! (`tests/imports/test_fhir_import.py`). The pure mapping cases (LOINC check digit, patient matching)
//! are unit tests in `src/fhir/mapping.rs`.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{Env, JSON, R, family, fhir_file, store_doc};
use ownhealth_core::Error;
use ownhealth_core::records::{active_medications, list_records};
use ownhealth_core::review::{self, ItemQuery, ReviewItem};
use ownhealth_ingest_structured::fhir::import_fhir;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

async fn items(conn: &mut PgConnection, doc: Uuid, kind: Option<&str>) -> anyhow::Result<Vec<ReviewItem>> {
    let mut rows = review::list_items(
        conn,
        &ItemQuery {
            document_id: Some(doc),
            status: Some("open".into()),
            kind: kind.map(String::from),
            ..ItemQuery::default()
        },
    )
    .await?;
    rows.sort_by_key(|r| r.payload["resource_type"].as_str().unwrap_or_default().to_string());
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Obs {
    id: Uuid,
    printed_name: String,
    printed_value: String,
    printed_unit: Option<String>,
    loinc: Option<String>,
    unit_ucum: Option<String>,
    interpretation: Option<String>,
    alias_group: String,
    value_canonical: Option<Decimal>,
    unit_canonical: Option<String>,
    category: String,
    report_key: Option<String>,
    accepted_by: String,
    document_id: Option<Uuid>,
    status: String,
    effective_at: chrono::DateTime<chrono::Utc>,
    import_ref: Option<String>,
    src_kind: String,
}

async fn obs(conn: &mut PgConnection, profile: Uuid) -> anyhow::Result<Vec<Obs>> {
    Ok(sqlx::query_as(
        "select o.id, o.printed_name, o.printed_value, o.printed_unit, o.loinc, o.unit_ucum, o.interpretation, \
         o.alias_group, o.value_canonical, o.unit_canonical, o.category, o.report_key, o.accepted_by, o.document_id, \
         o.status, o.effective_at, s.import_ref, s.kind as src_kind \
         from observation o join src s on s.id = o.src_id where o.profile_id = $1 order by o.printed_name",
    )
    .bind(profile)
    .fetch_all(conn)
    .await?)
}

async fn records(conn: &mut PgConnection, kind: &str, p: Uuid) -> anyhow::Result<Vec<Map<String, Value>>> {
    Ok(list_records(conn, kind, p, None, true).await?)
}

fn reasons(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

#[tokio::test]
async fn ips_document_bundle_projects_records_and_queues_the_rest() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        &fhir_file("ips-alex.json"),
        "ips.json",
        JSON,
    )
    .await?;
    let s = import_fhir(&mut tx, &env.blobs, doc).await?;
    // 6 records (the Encounter is a visit) + HbA1c + 2 blood pressure components; the CarePlan
    // (stored, not projected) and the bad LOINC wait
    assert_eq!((s.accepted, s.queued, s.skipped), (9, 2, 1));
    assert_eq!(
        s.reasons,
        reasons(&[
            ("unmapped", 1),
            ("loinc_unknown", 1),
            ("skip:status_entered-in-error", 1)
        ])
    );

    let conds = records(&mut tx, "condition", p).await?;
    assert_eq!(conds.len(), 1);
    let cond = &conds[0];
    assert_eq!(
        (&cond["text"], &cond["code_system"], &cond["code"]),
        (&json!("Hypertensive disorder"), &json!("snomed"), &json!("38341003"))
    );
    assert_eq!(
        (&cond["verification_status"], &cond["accepted_by"]),
        (&json!("confirmed"), &json!("import"))
    );
    assert_eq!(cond["fhir"]["id"], "alex-rivera-htn"); // the received JSON is kept
    let meds = active_medications(&mut tx, p, None, None, None, true).await?;
    assert_eq!(meds.len(), 1);
    assert_eq!(
        (&meds[0]["name"], &meds[0]["code_system"], &meds[0]["code"]),
        (&json!("Enalapril 10 mg"), &json!("atc"), &json!("C09AA02"))
    ); // via Medication
    let allergies = records(&mut tx, "allergy", p).await?;
    assert_eq!(allergies.len(), 1);
    assert_eq!(
        (&allergies[0]["criticality"], &allergies[0]["reaction_text"]),
        (&json!("high"), &json!("Rash"))
    );
    let vacs = records(&mut tx, "immunization", p).await?;
    assert_eq!(vacs.len(), 1);
    assert_eq!(
        (&vacs[0]["occurred"], &vacs[0]["lot"]),
        (&json!("2025-10-20"), &json!("SYN-0425"))
    );
    assert_eq!(records(&mut tx, "procedure", p).await?.len(), 1);
    let visits = records(&mut tx, "visit", p).await?;
    assert_eq!(visits.len(), 1);
    assert_eq!(
        (
            &visits[0]["class"],
            &visits[0]["period_start"],
            &visits[0]["accepted_by"]
        ),
        (&json!("AMB"), &json!("2026-05-12T06:30:00+00:00"), &json!("import"))
    );
    assert_eq!(visits[0]["fhir"]["id"], "alex-enc-1"); // the received JSON is kept

    let all = obs(&mut tx, p).await?;
    let names: BTreeSet<&str> = all.iter().map(|o| o.printed_name.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from(["Hemoglobin A1c", "Systolic blood pressure", "Diastolic blood pressure"])
    );
    let a1c = all
        .iter()
        .find(|o| o.printed_name == "Hemoglobin A1c")
        .ok_or_else(|| anyhow::anyhow!("a1c"))?;
    assert_eq!(
        (
            a1c.loinc.as_deref(),
            a1c.unit_ucum.as_deref(),
            a1c.printed_value.as_str(),
            a1c.interpretation.as_deref()
        ),
        (Some("4548-4"), Some("%"), "5.9", Some("H"))
    );
    assert_eq!(a1c.src_kind, "fhir_import");
    assert_eq!(
        a1c.import_ref.as_deref(),
        Some("ips-alex-2026/Observation/alex-rivera-hba1c")
    );
    assert_eq!(
        a1c.report_key.as_deref(),
        Some("ips-alex-2026/DiagnosticReport/alex-dr-1")
    );
    assert_eq!((a1c.accepted_by.as_str(), a1c.document_id), ("auto", Some(doc)));
    let sys = all
        .iter()
        .find(|o| o.printed_name == "Systolic blood pressure")
        .ok_or_else(|| anyhow::anyhow!("bp"))?;
    assert!(
        sys.import_ref
            .as_deref()
            .unwrap_or_default()
            .ends_with("/Observation/alex-bp-1#component0")
    );
    assert_eq!(sys.category, "vital-signs");

    let unmapped = items(&mut tx, doc, Some("fhir_unmapped")).await?;
    let held = items(&mut tx, doc, Some("candidate_row")).await?;
    let types: Vec<&str> = unmapped
        .iter()
        .map(|i| i.payload["resource_type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(types, vec!["CarePlan"]);
    assert_eq!(unmapped[0].payload["note"], "stored, not projected");
    assert_eq!(unmapped[0].payload["resource"]["id"], "alex-cp-1"); // stored as received
    assert_eq!(held[0].reasons, vec!["loinc_unknown".to_string()]);
    // the injection text in Observation.note stays inside the stored resource, never in a typed column
    assert!(
        held[0].payload["observation"]["fhir"]
            .to_string()
            .contains("Ignore previous")
    );
    let (flags, identity): (Value, Option<String>) =
        sqlx::query_as("select flags, identity_match from document where id = $1")
            .bind(doc)
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(identity.as_deref(), Some("matched"));
    assert_eq!(flags["import"]["accepted"], 9);
    Ok(())
}

#[tokio::test]
async fn same_bundle_twice_and_reexport_do_not_double_count() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let data = fhir_file("ips-alex.json");
    let doc = store_doc(&mut tx, &env.blobs, &fam, "alex", &data, "ips.json", JSON).await?;
    let first = import_fhir(&mut tx, &env.blobs, doc).await?;
    let again = import_fhir(&mut tx, &env.blobs, doc).await?; // a job retry: the stored summary, no new rows
    assert_eq!(again, first);
    // a new export of the same record (new Bundle.id, new bytes): every resource is a duplicate
    let mut bundle: Value = serde_json::from_slice(&data)?;
    bundle["id"] = json!("ips-alex-2026-b");
    let doc2 = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        bundle.to_string().as_bytes(),
        "ips2.json",
        JSON,
    )
    .await?;
    let second = import_fhir(&mut tx, &env.blobs, doc2).await?;
    assert_eq!((second.accepted, second.queued), (0, 0));
    assert_eq!((second.skipped, second.reasons["duplicate"]), (12, 11));
    assert_eq!(obs(&mut tx, p).await?.len(), 3);
    assert_eq!(records(&mut tx, "condition", p).await?.len(), 1);
    Ok(())
}

fn entries_mut(data: &mut Value) -> &mut Vec<Value> {
    data["entry"].as_array_mut().unwrap_or_else(|| panic!("a bundle"))
}

#[tokio::test]
async fn changed_resource_waits_in_review_and_accepting_supersedes() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let mut data: Value = serde_json::from_slice(&fhir_file("ips-alex.json"))?;
    let d1 = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        data.to_string().as_bytes(),
        "a.json",
        JSON,
    )
    .await?;
    import_fhir(&mut tx, &env.blobs, d1).await?;
    for e in entries_mut(&mut data) {
        if e["resource"]["resourceType"] == "Condition" && e["resource"]["id"] == "alex-rivera-htn" {
            e["resource"]["clinicalStatus"]["coding"][0]["code"] = json!("remission");
        }
    }
    data["id"] = json!("ips-alex-2027");
    let doc2 = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        data.to_string().as_bytes(),
        "b.json",
        JSON,
    )
    .await?;
    let s = import_fhir(&mut tx, &env.blobs, doc2).await?;
    assert_eq!((s.queued, s.reasons["conflict"], s.reasons["duplicate"]), (1, 1, 10));
    let held = items(&mut tx, doc2, Some("fhir_unmapped")).await?;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].payload["projection"], "record");
    assert_eq!(held[0].reasons, vec!["conflict".to_string()]);
    let ids = review::accept(&mut tx, held[0].id, fam.user("alex"), None).await?;
    let conds = records(&mut tx, "condition", p).await?;
    assert_eq!(conds.len(), 1);
    assert_eq!(conds[0]["id"], json!(ids[0].to_string()));
    assert_eq!(conds[0]["clinical_status"], "remission");
    assert!(!conds[0]["supersedes"].is_null());
    assert_eq!(conds[0]["accepted_by"], "person");
    Ok(())
}

async fn a1c_values(conn: &mut PgConnection, p: Uuid) -> anyhow::Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "select printed_value, status from observation where profile_id = $1 and loinc = '4548-4' order by recorded_at",
    )
    .bind(p)
    .fetch_all(conn)
    .await?)
}

fn correct_a1c(data: &mut Value) {
    for e in entries_mut(data) {
        if e["resource"]["id"] == "alex-rivera-hba1c" {
            e["resource"]["valueQuantity"]["value"] = json!(6.4); // the lab corrected the value upstream
        }
    }
    data["id"] = json!("ips-alex-2027");
}

#[tokio::test]
async fn a_corrected_observation_names_the_version_it_replaces_and_accepting_supersedes() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("alex");
    let mut data: Value = serde_json::from_slice(&fhir_file("ips-alex.json"))?;
    let d1 = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        data.to_string().as_bytes(),
        "a.json",
        JSON,
    )
    .await?;
    import_fhir(&mut tx, &env.blobs, d1).await?;
    let before = a1c_values(&mut tx, p).await?;
    assert_eq!(before, vec![("5.9".to_string(), "current".to_string())]);
    let old_id: Uuid = sqlx::query_scalar("select id from observation where profile_id = $1 and loinc = '4548-4'")
        .bind(p)
        .fetch_one(&mut *tx)
        .await?;
    correct_a1c(&mut data);
    let doc2 = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "alex",
        data.to_string().as_bytes(),
        "b.json",
        JSON,
    )
    .await?;
    import_fhir(&mut tx, &env.blobs, doc2).await?;
    let held: Vec<ReviewItem> = items(&mut tx, doc2, Some("candidate_row"))
        .await?
        .into_iter()
        .filter(|i| i.reasons.iter().any(|r| r == "conflict"))
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].payload["supersedes"], json!(old_id.to_string()));
    review::accept(&mut tx, held[0].id, fam.user("alex"), None).await?;
    let mut after = a1c_values(&mut tx, p).await?;
    after.sort();
    assert_eq!(
        after,
        vec![
            ("5.9".to_string(), "superseded".to_string()),
            ("6.4".to_string(), "current".to_string())
        ]
    );
    Ok(())
}

fn asthma(rid: &str, subject: &str) -> Value {
    json!({
        "resourceType": "Condition",
        "id": rid,
        "clinicalStatus": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "active"}]},
        "verificationStatus": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-ver-status", "code": "confirmed"}]},
        "code": {"coding": [{"system": "http://snomed.info/sct", "code": "195967001", "display": "Asthma"}]},
        "subject": {"reference": subject},
    })
}

/// FHIR ids are unique per server only: another provider's Condition/alex-rivera-htn is not a new
/// version of ours.
#[tokio::test]
async fn another_source_reusing_a_resource_id_is_a_separate_fact() -> R {
    for with_patient in [false, true] {
        let env = Env::new().await?;
        let mut tx = env.tx().await?;
        let fam = family(&mut tx).await?;
        let p = fam.profile("alex");
        let d1 = store_doc(
            &mut tx,
            &env.blobs,
            &fam,
            "alex",
            &fhir_file("ips-alex.json"),
            "a.json",
            JSON,
        )
        .await?;
        import_fhir(&mut tx, &env.blobs, d1).await?;
        let subject = if with_patient {
            "Patient/p-889"
        } else {
            "Patient/alex-rivera"
        };
        let mut entries = vec![json!({"resource": asthma("alex-rivera-htn", subject)})];
        if with_patient {
            entries.insert(
                0,
                json!({"fullUrl": "urn:uuid:p-889", "resource": {
                    "resourceType": "Patient",
                    "id": "p-889",
                    "identifier": [{"system": "urn:oid:2.16.724.4.77", "value": "HN-889"}],
                    "name": [{"family": "Rivera", "given": ["Alex"]}],
                    "birthDate": "1979-05-14",
                }}),
            );
        }
        let other = json!({"resourceType": "Bundle", "id": "other-hospital-1", "type": "collection", "entry": entries});
        let doc2 = store_doc(
            &mut tx,
            &env.blobs,
            &fam,
            "alex",
            other.to_string().as_bytes(),
            "o.json",
            JSON,
        )
        .await?;
        let s = import_fhir(&mut tx, &env.blobs, doc2).await?;
        assert!(
            !s.reasons.contains_key("conflict") && !s.reasons.contains_key("duplicate"),
            "{s:?}"
        );
        for item in items(&mut tx, doc2, None).await? {
            assert!(!py_truthy(&item.payload["supersedes"]));
            review::accept(&mut tx, item.id, fam.user("alex"), None).await?;
        }
        let mut texts: Vec<String> = records(&mut tx, "condition", p)
            .await?
            .iter()
            .map(|c| c["text"].as_str().unwrap_or_default().to_string())
            .collect();
        texts.sort();
        assert_eq!(
            texts,
            vec!["Asthma", "Hypertensive disorder"],
            "with_patient={with_patient}"
        );
    }
    Ok(())
}

fn py_truthy(v: &Value) -> bool {
    ownhealth_ingest_structured::py::truthy(Some(v))
}

#[tokio::test]
async fn patient_mismatch_holds_every_fact_for_review() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("sam");
    // Alex's file, Sam's profile
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "sam",
        &fhir_file("ips-alex.json"),
        "ips.json",
        JSON,
    )
    .await?;
    let s = import_fhir(&mut tx, &env.blobs, doc).await?;
    assert_eq!((s.accepted, s.queued, s.reasons["identity_mismatch"]), (0, 11, 10));
    assert!(obs(&mut tx, p).await?.is_empty() && records(&mut tx, "condition", p).await?.is_empty());
    let held: Vec<ReviewItem> = items(&mut tx, doc, Some("fhir_unmapped"))
        .await?
        .into_iter()
        .filter(|i| i.payload["projection"] == "record")
        .collect();
    let kinds: BTreeSet<&str> = held
        .iter()
        .map(|i| i.payload["record_kind"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from([
            "condition",
            "allergy",
            "medication",
            "immunization",
            "procedure",
            "visit"
        ])
    );
    // a person confirms one: it becomes a record, accepted by that person
    let cond = held
        .iter()
        .find(|i| i.payload["record_kind"] == "condition")
        .ok_or_else(|| anyhow::anyhow!("condition"))?;
    review::accept(&mut tx, cond.id, fam.user("sam"), None).await?;
    let rows = records(&mut tx, "condition", p).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["accepted_by"], "person");
    let unmapped = items(&mut tx, doc, Some("fhir_unmapped"))
        .await?
        .into_iter()
        .find(|i| i.payload["projection"].is_null())
        .ok_or_else(|| anyhow::anyhow!("unmapped"))?;
    let mut sp = sqlx::Connection::begin(&mut *tx).await?;
    match review::accept(&mut sp, unmapped.id, fam.user("sam"), None).await {
        Err(Error::Review(m)) => assert!(m.contains("stored, not projected"), "{m}"),
        other => panic!("expected a review error, got {other:?}"),
    }
    sp.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn us_core_collection_medication_requests_and_sensitivity() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("sam");
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "sam",
        &fhir_file("uscore-sam.json"),
        "sam.json",
        JSON,
    )
    .await?;
    let s = import_fhir(&mut tx, &env.blobs, doc).await?;
    assert_eq!((s.accepted, s.queued), (4, 2));
    assert_eq!(s.reasons, reasons(&[("unit_unconvertible", 1), ("unmapped", 1)]));
    let meds = records(&mut tx, "medication", p).await?;
    let by_name = |n: &str| meds.iter().find(|m| m["name"] == n).cloned().unwrap_or_default();
    assert_eq!(by_name("Levothyroxine 50 mcg")["med_status"], "active");
    let amox = by_name("Amoxicillin 500 mg");
    assert_eq!(
        (&amox["med_status"], &amox["stopped"]),
        (&json!("stopped"), &json!("2026-02-08"))
    );
    let active: Vec<Value> = active_medications(&mut tx, p, None, None, None, true)
        .await?
        .iter()
        .map(|m| m["name"].clone())
        .collect();
    assert_eq!(active, vec![json!("Levothyroxine 50 mcg")]);
    let moods = records(&mut tx, "condition", p).await?;
    assert_eq!(moods.len(), 1);
    assert_eq!(moods[0]["sensitivity"], json!(["mental_health"]));
    assert!(list_records(&mut tx, "condition", p, None, false).await?.is_empty());
    let weights = obs(&mut tx, p).await?;
    assert_eq!(weights.len(), 1);
    // -04:00 kept as the same instant
    assert_eq!(weights[0].effective_at.to_rfc3339(), "2026-10-02T11:30:00+00:00");
    let types: Vec<String> = items(&mut tx, doc, Some("fhir_unmapped"))
        .await?
        .iter()
        .map(|i| i.payload["resource_type"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(types, vec!["CarePlan"]);
    Ok(())
}

#[tokio::test]
async fn ndjson_bulk_lines() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let p = fam.profile("carmen");
    let data = fhir_file("bulk-carmen.ndjson");
    let doc = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "carmen",
        &data,
        "x.ndjson",
        "application/fhir+ndjson",
    )
    .await?;
    let s = import_fhir(&mut tx, &env.blobs, doc).await?;
    assert_eq!((s.accepted, s.queued), (2, 1));
    let vacs = records(&mut tx, "immunization", p).await?;
    assert_eq!(vacs.len(), 1);
    assert_eq!(vacs[0]["dose_number"], 5);
    let held = items(&mut tx, doc, Some("fhir_unmapped")).await?;
    assert_eq!(held.len(), 1);
    assert!(
        held[0].payload["note"]
            .as_str()
            .unwrap_or_default()
            .contains("not-done")
    );
    assert_eq!(held[0].payload["json_pointer"], "L4");
    // a new export of the same lines (new bytes): the held not-done Immunization is not queued again
    let mut data2 = data.clone();
    data2.push(b'\n');
    let doc2 = store_doc(
        &mut tx,
        &env.blobs,
        &fam,
        "carmen",
        &data2,
        "y.ndjson",
        "application/fhir+ndjson",
    )
    .await?;
    let s2 = import_fhir(&mut tx, &env.blobs, doc2).await?;
    assert_eq!((s2.accepted, s2.queued), (0, 0));
    assert!(items(&mut tx, doc2, Some("fhir_unmapped")).await?.is_empty());
    Ok(())
}

/// One Observation with a valid LOINC (2276-4, ferritin), a vendor display no alias table knows, and
/// a printed unit text beside its UCUM code.
fn ferritin_bundle(display: &str, unit_text: &str) -> anyhow::Result<Value> {
    let ips: Value = serde_json::from_slice(&fhir_file("ips-alex.json"))?;
    let patient = ips["entry"]
        .as_array()
        .and_then(|e| e.iter().find(|x| x["resource"]["resourceType"] == "Patient"))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("patient"))?;
    let obs = json!({
        "resourceType": "Observation",
        "id": "vendor-ferritin-1",
        "status": "final",
        "category": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "laboratory"}]}],
        "code": {"coding": [{"system": "http://loinc.org", "code": "2276-4", "display": display}]},
        "subject": {"reference": "Patient/alex-rivera"},
        "effectiveDateTime": "2026-06-02T09:00:00+02:00",
        "valueQuantity": {"value": 85, "unit": unit_text, "system": "http://unitsofmeasure.org", "code": "ng/mL"},
    });
    Ok(json!({"resourceType": "Bundle", "id": "vendor-1", "type": "collection", "entry": [patient, {"resource": obs}]}))
}

async fn ferritin(env: &Env, conn: &mut PgConnection, display: &str) -> anyhow::Result<(Uuid, Vec<Obs>)> {
    let fam = family(&mut *conn).await?;
    let data = ferritin_bundle(display, "nanogram per millilitre")?;
    let doc = store_doc(
        &mut *conn,
        &env.blobs,
        &fam,
        "alex",
        data.to_string().as_bytes(),
        "v.json",
        JSON,
    )
    .await?;
    import_fhir(&mut *conn, &env.blobs, doc).await?;
    let p = fam.profile("alex");
    let rows = obs(&mut *conn, p).await?;
    Ok((
        p,
        rows.into_iter()
            .filter(|o| o.loinc.as_deref() == Some("2276-4") && o.status == "current")
            .collect(),
    ))
}

/// A valid received LOINC sets the alias group and a received UCUM code sets the unit, even when the
/// printed name and unit text are unknown to the tables.
#[tokio::test]
async fn received_loinc_and_ucum_drive_the_derived_layer() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let (_, rows) = ferritin(&env, &mut tx, "VNDR-FERR serum assay").await?;
    assert_eq!(rows.len(), 1);
    let o = &rows[0];
    assert_eq!(
        (o.alias_group.as_str(), o.unit_ucum.as_deref()),
        ("ferritin", Some("ng/mL"))
    );
    assert!(o.value_canonical.is_some() && o.unit_canonical.is_some());
    assert_eq!(o.printed_unit.as_deref(), Some("nanogram per millilitre")); // the printed text stays the truth layer
    Ok(())
}

#[tokio::test]
async fn rederive_keeps_the_received_ucum_code() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let (p, rows) = ferritin(&env, &mut tx, "Ferritin").await?;
    let o = rows.first().cloned().ok_or_else(|| anyhow::anyhow!("ferritin"))?;
    assert!(o.unit_ucum.as_deref() == Some("ng/mL") && o.value_canonical.is_some());
    ownhealth_normalize::rederive(&mut tx, Some(p), None).await?;
    let again: Vec<Obs> = obs(&mut tx, p)
        .await?
        .into_iter()
        .filter(|x| x.loinc.as_deref() == Some("2276-4") && x.status == "current")
        .collect();
    assert_eq!(again.len(), 1);
    assert_eq!(
        (&again[0].unit_ucum, again[0].value_canonical, &again[0].alias_group),
        (&o.unit_ucum, o.value_canonical, &o.alias_group)
    );
    let _ = o.id;
    Ok(())
}
