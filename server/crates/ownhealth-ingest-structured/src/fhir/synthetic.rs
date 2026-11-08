//! A synthetic International Patient Summary (IPS) bundle in the shape of the HL7 IPS examples
//! (`ingest/fhir/synthetic.py`): `Bundle.type = document`, a Composition first with its sections, then
//! the Patient and the entries the sections reference. Synthetic people only; the issuing organization
//! is the fictional LabNorte.

use chrono::NaiveDate;
use serde_json::{Value, json};

const SNOMED: &str = "http://snomed.info/sct";
const LOINC: &str = "http://loinc.org";

fn reference(kind: &str, rid: &str) -> Value {
    json!({"reference": format!("urn:uuid:{}-{rid}", kind.to_lowercase())})
}

fn rtype(r: &Value) -> &str {
    r["resourceType"].as_str().unwrap_or_default()
}

pub fn ips_bundle(
    bundle_id: &str,
    given: &str,
    family: &str,
    birth: NaiveDate,
    with_observations: bool,
    extra: &[Value],
) -> Value {
    let pid = format!("{}-{}", given.to_lowercase(), family.to_lowercase());
    let patient = json!({
        "resourceType": "Patient",
        "id": pid,
        "name": [{"family": family, "given": [given]}],
        "birthDate": birth.format("%Y-%m-%d").to_string(),
    });
    let subject = json!({"reference": format!("Patient/{pid}")});
    let mut resources = vec![
        json!({
            "resourceType": "Condition",
            "id": format!("{pid}-htn"),
            "clinicalStatus": {
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "active"}]
            },
            "verificationStatus": {
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-ver-status", "code": "confirmed"}]
            },
            "code": {"coding": [{"system": SNOMED, "code": "38341003", "display": "Hypertensive disorder"}]},
            "subject": subject,
            "onsetDateTime": "2015-03",
        }),
        json!({
            "resourceType": "AllergyIntolerance",
            "id": format!("{pid}-pen"),
            "clinicalStatus": {
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-clinical", "code": "active"}]
            },
            "verificationStatus": {
                "coding": [{
                    "system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-verification",
                    "code": "confirmed",
                }]
            },
            "criticality": "high",
            "code": {"coding": [{"system": SNOMED, "code": "764146007", "display": "Penicillin"}]},
            "patient": subject,
            "reaction": [{"manifestation": [{"coding": [{"system": SNOMED, "code": "271807003", "display": "Rash"}]}]}],
        }),
        json!({
            "resourceType": "MedicationStatement",
            "id": format!("{pid}-enalapril"),
            "status": "active",
            "medicationReference": {"reference": format!("Medication/{pid}-med-enalapril")},
            "subject": subject,
            "effectivePeriod": {"start": "2015-04-02"},
            "dosage": [{"text": "10 mg once a day"}],
        }),
        json!({
            "resourceType": "Medication",
            "id": format!("{pid}-med-enalapril"),
            "code": {
                "coding": [{"system": "http://www.whocc.no/atc", "code": "C09AA02", "display": "enalapril"}],
                "text": "Enalapril 10 mg",
            },
        }),
        json!({
            "resourceType": "Immunization",
            "id": format!("{pid}-flu-2025"),
            "status": "completed",
            "vaccineCode": {
                "coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "197", "display": "Influenza, seasonal"}]
            },
            "patient": subject,
            "occurrenceDateTime": "2025-10-20",
            "lotNumber": "SYN-0425",
        }),
        json!({
            "resourceType": "Procedure",
            "id": format!("{pid}-cataract"),
            "status": "completed",
            "code": {"coding": [{"system": SNOMED, "code": "110473004", "display": "Cataract surgery"}]},
            "subject": subject,
            "performedDateTime": "2022-09-12",
        }),
    ];
    if with_observations {
        resources.push(json!({
            "resourceType": "Observation",
            "id": format!("{pid}-hba1c"),
            "status": "final",
            "category": [{
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "laboratory"}]
            }],
            "code": {"coding": [{"system": LOINC, "code": "4548-4", "display": "Hemoglobin A1c"}]},
            "subject": subject,
            "effectiveDateTime": "2026-05-12T09:00:00+02:00",
            "valueQuantity": {"value": 5.9, "unit": "%", "system": "http://unitsofmeasure.org", "code": "%"},
            "referenceRange": [{"low": {"value": 4.0}, "high": {"value": 5.6}}],
            "interpretation": [{
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation", "code": "H"}]
            }],
        }));
    }
    resources.extend(extra.iter().cloned());
    let sections = [
        ("11450-4", "Problem list", "Condition"),
        ("48765-2", "Allergies and intolerances", "AllergyIntolerance"),
        ("10160-0", "Medication summary", "MedicationStatement"),
        ("11369-6", "Immunizations", "Immunization"),
        ("47519-4", "History of procedures", "Procedure"),
        ("30954-2", "Results", "Observation"),
    ];
    let section: Vec<Value> = sections
        .iter()
        .filter(|(_, _, t)| resources.iter().any(|r| rtype(r) == *t))
        .map(|(code, title, t)| {
            let entry: Vec<Value> = resources
                .iter()
                .filter(|r| rtype(r) == *t)
                .map(|r| reference(rtype(r), r["id"].as_str().unwrap_or_default()))
                .collect();
            json!({"title": title, "code": {"coding": [{"system": LOINC, "code": code}]}, "entry": entry})
        })
        .collect();
    let composition = json!({
        "resourceType": "Composition",
        "id": format!("{bundle_id}-composition"),
        "status": "final",
        "type": {"coding": [{"system": LOINC, "code": "60591-5", "display": "Patient summary Document"}]},
        "subject": {"reference": format!("urn:uuid:patient-{pid}")},
        "date": "2026-11-01T10:00:00+01:00",
        "author": [{"reference": "urn:uuid:organization-labnorte"}],
        "title": format!("Patient Summary of {given} {family}"),
        "section": section,
    });
    let org = json!({"resourceType": "Organization", "id": "labnorte", "name": "LabNorte (synthetic)"});
    let mut entries = vec![composition, patient, org];
    entries.extend(resources);
    let entry: Vec<Value> = entries
        .into_iter()
        .map(|r| {
            let url = format!(
                "urn:uuid:{}-{}",
                rtype(&r).to_lowercase(),
                r["id"].as_str().unwrap_or_default()
            );
            json!({"fullUrl": url, "resource": r})
        })
        .collect();
    json!({
        "resourceType": "Bundle",
        "id": bundle_id,
        "identifier": {"system": "urn:oid:2.16.724.4.99", "value": bundle_id},
        "type": "document",
        "timestamp": "2026-11-01T10:00:00+01:00",
        "entry": entry,
    })
}
