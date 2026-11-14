//! Build the FHIR R4-shaped JSON we store in `fhir` from the typed columns (`core/fhir.py`).
//!
//! Imported resources keep the received JSON instead; these builders are for rows we create
//! (document extraction, manual entry, demo seed). Keys come out in the order the Python builders
//! wrote them; Postgres jsonb reorders them anyway.

use rust_decimal::Decimal;
use serde_json::{Map, Value, json};

use crate::observations::ObservationIn;
use crate::pyjson::{py_date, py_isoformat, py_num};
use crate::records::{AllergyIn, ConditionIn, ImmunizationIn, MedicationIn, ProcedureIn, VisitIn};

pub const UCUM: &str = "http://unitsofmeasure.org";

/// `_drop_none`: leave out None, [] and {}.
fn drop_none(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        let empty = match &v {
            Value::Null => true,
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        };
        if !empty {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

fn s(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |x| Value::String(x.to_string()))
}

fn num(v: Option<&Decimal>) -> Value {
    v.map_or(Value::Null, py_num)
}

fn date(v: Option<&chrono::NaiveDate>) -> Value {
    v.map_or(Value::Null, |d| Value::String(py_date(d)))
}

fn code(system_url: Option<&str>, code: Option<&str>, display: &str) -> Value {
    let coding = match code.filter(|c| !c.is_empty()) {
        Some(c) => vec![drop_none(vec![
            ("system", s(system_url)),
            ("code", s(Some(c))),
            ("display", s(Some(display))),
        ])],
        None => vec![],
    };
    drop_none(vec![("coding", Value::Array(coding)), ("text", s(Some(display)))])
}

/// The code systems we name by a short key in `code_system`.
pub fn system_url(code_system: Option<&str>) -> Option<String> {
    let cs = code_system?;
    Some(
        match cs.to_lowercase().as_str() {
            "loinc" => "http://loinc.org",
            "snomed" => "http://snomed.info/sct",
            "icd10" => "http://hl7.org/fhir/sid/icd-10",
            "rxnorm" => "http://www.nlm.nih.gov/research/umls/rxnorm",
            "cvx" => "http://hl7.org/fhir/sid/cvx",
            "atc" => "http://www.whocc.no/atc",
            _ => cs,
        }
        .to_string(),
    )
}

/// `status` is the row status (`provisional` gives `preliminary`, anything else `final`).
pub fn observation(o: &ObservationIn, status: &str) -> Value {
    let unit = o.printed_unit.as_deref();
    let mut value: Vec<(&str, Value)> = Vec::new();
    if o.value_num.is_some() {
        value.push((
            "valueQuantity",
            drop_none(vec![
                ("value", num(o.value_num.as_ref())),
                ("comparator", s(o.value_cmp.as_deref())),
                ("unit", s(unit)),
                (
                    "system",
                    if o.unit_ucum.as_deref().is_some_and(|u| !u.is_empty()) {
                        s(Some(UCUM))
                    } else {
                        Value::Null
                    },
                ),
                ("code", s(o.unit_ucum.as_deref())),
            ]),
        ));
    } else if let Some(c) = &o.value_code {
        value.push(("valueCodeableConcept", json!({"text": c})));
    } else {
        let text = o
            .value_text
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(&o.printed_value);
        value.push(("valueString", s(Some(text))));
    }
    let reference = drop_none(vec![
        (
            "low",
            drop_none(vec![("value", num(o.ref_low.as_ref())), ("unit", s(unit))]),
        ),
        (
            "high",
            drop_none(vec![("value", num(o.ref_high.as_ref())), ("unit", s(unit))]),
        ),
        ("text", s(o.printed_ref_range.as_deref())),
    ]);
    let empty_ref = reference.as_object().is_none_or(Map::is_empty);
    let mut pairs: Vec<(&str, Value)> = vec![
        ("resourceType", json!("Observation")),
        (
            "status",
            json!(if status == "provisional" {
                "preliminary"
            } else {
                "final"
            }),
        ),
        (
            "category",
            json!([{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": o.category}]}]),
        ),
        (
            "code",
            code(Some("http://loinc.org"), o.loinc.as_deref(), &o.printed_name),
        ),
        ("effectiveDateTime", Value::String(py_isoformat(&o.effective_at))),
    ];
    pairs.extend(value);
    pairs.push((
        "interpretation",
        match o.interpretation.as_deref().filter(|i| !i.is_empty()) {
            Some(i) => json!([{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation", "code": i}]}]),
            None => Value::Null,
        },
    ));
    pairs.push((
        "referenceRange",
        if empty_ref {
            Value::Null
        } else {
            Value::Array(vec![reference])
        },
    ));
    pairs.push((
        "method",
        o.method
            .as_deref()
            .filter(|m| !m.is_empty())
            .map_or(Value::Null, |m| json!({"text": m})),
    ));
    drop_none(pairs)
}

pub fn condition(c: &ConditionIn) -> Value {
    drop_none(vec![
        ("resourceType", json!("Condition")),
        (
            "clinicalStatus",
            json!({"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": c.clinical_status}]}),
        ),
        (
            "verificationStatus",
            json!({"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-ver-status", "code": c.verification_status}]}),
        ),
        (
            "code",
            code(
                system_url(c.code_system.as_deref()).as_deref(),
                c.code.as_deref(),
                &c.text,
            ),
        ),
        ("onsetDateTime", date(c.onset.as_ref())),
        ("abatementDateTime", date(c.abatement.as_ref())),
    ])
}

pub fn medication_statement(m: &MedicationIn) -> Value {
    let status = match m.med_status.as_str() {
        "stopped" => "stopped",
        "on_hold" => "on-hold",
        _ => "active",
    };
    let display: Vec<&str> = [Some(m.name.as_str()), m.strength.as_deref()]
        .into_iter()
        .flatten()
        .filter(|x| !x.is_empty())
        .collect();
    let primary_system = system_url(m.code_system.as_deref());
    let mut concept = code(primary_system.as_deref(), m.code.as_deref(), &display.join(" "));
    // Every other coding the source sent, after the primary one, as received.
    let extra: Vec<Value> = m
        .codes
        .iter()
        .flatten()
        .filter(|c| !(Some(c.code.as_str()) == m.code.as_deref() && c.system == primary_system))
        .map(|c| {
            drop_none(vec![
                ("system", s(c.system.as_deref())),
                ("code", s(Some(&c.code))),
                ("display", s(c.display.as_deref())),
            ])
        })
        .collect();
    if !extra.is_empty()
        && let Some(obj) = concept.as_object_mut()
    {
        let mut coding = obj.get("coding").and_then(Value::as_array).cloned().unwrap_or_default();
        coding.extend(extra);
        let text = obj.shift_remove("text");
        obj.insert("coding".into(), Value::Array(coding));
        if let Some(t) = text {
            obj.insert("text".into(), t);
        }
    }
    let text = |v: Option<&str>| v.filter(|x| !x.is_empty()).map(|x| x.to_string());
    let dosage = drop_none(vec![
        ("text", s(text(m.dose_text.as_deref()).as_deref())),
        (
            "timing",
            text(m.schedule_text.as_deref()).map_or(Value::Null, |t| json!({"code": {"text": t}})),
        ),
    ]);
    drop_none(vec![
        ("resourceType", json!("MedicationStatement")),
        ("status", json!(status)),
        ("medicationCodeableConcept", concept),
        (
            "effectivePeriod",
            drop_none(vec![
                ("start", date(m.started.as_ref())),
                ("end", date(m.stopped.as_ref())),
            ]),
        ),
        (
            "dosage",
            if dosage.as_object().is_none_or(Map::is_empty) {
                Value::Null
            } else {
                Value::Array(vec![dosage])
            },
        ),
        (
            "informationSource",
            text(m.prescriber.as_deref()).map_or(Value::Null, |p| json!({"display": p})),
        ),
        (
            "reasonCode",
            text(m.reason_text.as_deref()).map_or(Value::Null, |r| json!([{"text": r}])),
        ),
        (
            "category",
            if m.kind == "supplement" {
                json!({"text": "supplement"})
            } else {
                Value::Null
            },
        ),
    ])
}

/// The extension that carries the person's recorded decision on a medicine or supplement (migration
/// 1101): `code` (keep | stop | do_not_repurchase), `reason` and `date`.
pub const DECISION_EXTENSION: &str = "urn:ownhealth:decision";

/// Put the person's recorded decision on a stored MedicationStatement, so the export keeps it. Any
/// decision extension already in the resource (a re-imported export) is replaced by the columns'.
pub fn set_medication_decision(resource: &mut Value, m: &MedicationIn) {
    let Some(obj) = resource.as_object_mut() else {
        return;
    };
    let had = obj.get("extension").and_then(Value::as_array).is_some_and(|a| {
        a.iter()
            .any(|e| e.get("url").and_then(Value::as_str) == Some(DECISION_EXTENSION))
    });
    if m.decision.is_none() && !had {
        // nothing to say and nothing stale: the resource stays exactly as built or received
        return;
    }
    let mut ext: Vec<Value> = obj
        .get("extension")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|e| e.get("url").and_then(Value::as_str) != Some(DECISION_EXTENSION))
        .cloned()
        .collect();
    if let Some(code) = m.decision.as_deref() {
        let mut parts = vec![json!({"url": "code", "valueCode": code})];
        if let Some(r) = m.decision_reason.as_deref().filter(|r| !r.is_empty()) {
            parts.push(json!({"url": "reason", "valueString": r}));
        }
        if let Some(d) = m.decision_on.as_ref() {
            parts.push(json!({"url": "date", "valueDate": d.format("%Y-%m-%d").to_string()}));
        }
        ext.push(json!({"url": DECISION_EXTENSION, "extension": parts}));
    }
    if ext.is_empty() {
        obj.shift_remove("extension");
    } else {
        obj.insert("extension".into(), Value::Array(ext));
    }
}

pub const ALLERGY_VERIFICATION: [&str; 5] = ["unconfirmed", "presumed", "confirmed", "refuted", "entered-in-error"];

pub fn allergy(a: &AllergyIn) -> Value {
    let ver = if a.verification_status.is_empty() {
        "confirmed"
    } else {
        a.verification_status.as_str()
    };
    // a free string: an unknown code is left out
    let ver = ALLERGY_VERIFICATION.contains(&ver).then_some(ver);
    drop_none(vec![
        ("resourceType", json!("AllergyIntolerance")),
        (
            // FHIR ait-1: no clinicalStatus on an entered-in-error allergy.
            "clinicalStatus",
            if ver == Some("entered-in-error") {
                Value::Null
            } else {
                json!({"coding": [{"system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-clinical", "code": a.clinical_status}]})
            },
        ),
        (
            "verificationStatus",
            ver.map_or(Value::Null, |v| {
                json!({"coding": [{"system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-verification", "code": v}]})
            }),
        ),
        ("code", code(system_url(a.code_system.as_deref()).as_deref(), a.code.as_deref(), &a.substance)),
        ("criticality", s(a.criticality.as_deref())),
        ("reaction", a.reaction_text.as_deref().filter(|r| !r.is_empty()).map_or(Value::Null, |r| json!([{"description": r}]))),
    ])
}

pub fn immunization(i: &ImmunizationIn) -> Value {
    drop_none(vec![
        ("resourceType", json!("Immunization")),
        ("status", json!("completed")),
        (
            "vaccineCode",
            code(
                system_url(i.code_system.as_deref()).as_deref(),
                i.code.as_deref(),
                &i.vaccine,
            ),
        ),
        ("occurrenceDateTime", date(i.occurred.as_ref())),
        ("lotNumber", s(i.lot.as_deref())),
        (
            "protocolApplied",
            i.dose_number
                .filter(|n| *n != 0)
                .map_or(Value::Null, |n| json!([{"doseNumberPositiveInt": n}])),
        ),
    ])
}

pub fn procedure(p: &ProcedureIn) -> Value {
    drop_none(vec![
        ("resourceType", json!("Procedure")),
        ("status", json!("completed")),
        (
            "code",
            code(
                system_url(p.code_system.as_deref()).as_deref(),
                p.code.as_deref(),
                &p.text,
            ),
        ),
        ("performedDateTime", date(p.performed.as_ref())),
    ])
}

/// A visit as a FHIR R4 Encounter. R4 needs a class: a visit typed by hand is ambulatory (AMB).
pub fn encounter(v: &VisitIn) -> Value {
    let class = v.class.as_deref().filter(|c| !c.is_empty()).unwrap_or("AMB");
    drop_none(vec![
        ("resourceType", json!("Encounter")),
        ("status", json!("finished")),
        (
            "class",
            json!({"system": "http://terminology.hl7.org/CodeSystem/v3-ActCode", "code": class}),
        ),
        (
            "period",
            drop_none(vec![
                (
                    "start",
                    v.period_start.as_ref().map_or(Value::Null, |d| json!(py_isoformat(d))),
                ),
                (
                    "end",
                    v.period_end.as_ref().map_or(Value::Null, |d| json!(py_isoformat(d))),
                ),
            ]),
        ),
        (
            "serviceProvider",
            v.provider_name
                .as_deref()
                .filter(|p| !p.is_empty())
                .map_or(Value::Null, |p| json!({"display": p})),
        ),
        (
            "reasonCode",
            v.reason_text
                .as_deref()
                .filter(|r| !r.is_empty())
                .map_or(Value::Null, |r| json!([{"text": r}])),
        ),
    ])
}

/// The LOINC code of a FHIR CodeableConcept (`ingest.fhir.mapping.loinc_of`).
pub fn loinc_of(concept: Option<&Value>) -> Option<String> {
    let codings = concept?.get("coding")?.as_array()?;
    codings.iter().find_map(|c| {
        let system = c.get("system")?.as_str()?;
        let code = c.get("code")?.as_str().filter(|v| !v.is_empty())?;
        (system == "http://loinc.org").then(|| code.to_string())
    })
}

/// The UCUM code a stored Observation sent (valueQuantity.code with the UCUM system): on the
/// resource, or on the component coded `loinc`. Truth like the received LOINC, so `rederive` keeps
/// it. (Lane C's `received_ucum`, here because lane A needs it too.)
pub fn received_ucum(resource: Option<&Value>, loinc: Option<&str>) -> Option<String> {
    let resource = resource?.as_object()?;
    let mut parts: Vec<&Map<String, Value>> = vec![resource];
    if let Some(components) = resource.get("component").and_then(Value::as_array) {
        parts.extend(components.iter().filter_map(Value::as_object));
    }
    let mut found: Vec<(Option<String>, String)> = Vec::new();
    for part in parts {
        let Some(q) = part.get("valueQuantity").and_then(Value::as_object) else {
            continue;
        };
        let is_ucum = q.get("system").and_then(Value::as_str) == Some(UCUM);
        let unit = q.get("code").and_then(Value::as_str).filter(|c| !c.is_empty());
        let has_value = q.get("value").is_some_and(|v| !v.is_null());
        if let (true, Some(u), true) = (is_ucum, unit, has_value) {
            found.push((loinc_of(part.get("code")), u.to_string()));
        }
    }
    if let Some(want) = loinc
        && let Some((_, u)) = found.iter().find(|(c, _)| c.as_deref() == Some(want))
    {
        return Some(u.clone());
    }
    if found.len() == 1 {
        found.pop().map(|(_, u)| u)
    } else {
        None
    }
}
