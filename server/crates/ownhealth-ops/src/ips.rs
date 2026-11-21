//! The International Patient Summary (IPS) as a FHIR R4 document Bundle: one file another health
//! system reads. It carries the rows of the printed doctor summary's `ips` view
//! (`ownhealth_rules::summary`, specialty `ips`), so the file and the sheet say the same thing.
//!
//! ```text
//! Bundle (type document, identifier and id from a content hash, timestamp = the data's date)
//!   Composition  LOINC 60591-5, subject and author the Patient, one section per IPS heading:
//!                 Problems 11450-4, Allergies 48765-2, Medications 10160-0   (required)
//!                 Immunizations 11369-6, Results 30954-2, Procedures 47519-4 (recommended)
//!   Patient      the profile
//!   one entry per row the sheet shows: the row's stored FHIR, codes as received (a panel sent as
//!                one Observation goes in once, though each component is its own row, and keeps only
//!                the components whose rows the sheet shows)
//! ```
//!
//! - every entry's `id` is its row id, its `meta.source` is `urn:ownhealth:src:<src id>` (a panel's
//!   is its first shown row's src; the narrative lists the src of every row), and it
//!   points at the Patient; a reference to something outside the Bundle keeps only its words, so
//!   the document resolves in itself (a medicine named through a Medication resource gets its
//!   received codes back as `medicationCodeableConcept`);
//! - IPS 1.1 rules for empty sections: a required one (problems, allergies, medicines) carries the IPS
//!   "no information about ..." entry (code system `absent-unknown-uv-ips`; nothing on record is not
//!   "none known"), and an empty recommended one is left out; the narrative says why it is empty
//!   (nothing on record, or the caller's scopes hide it). The importer skips those entries;
//! - the narratives are the sheet's own words (`data/summary/templates.yaml`) in the chosen language,
//!   so the wording lint covers them; the section codes are `data/summary/ips_sections.tsv`;
//! - the same data on the same day gives the same bytes.

use std::collections::HashSet;

use chrono::NaiveDate;
use ownhealth_core::access::Scopes;
use ownhealth_core::{Error, Result};
use ownhealth_rules::summary::{self, Options};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

/// The summary view whose rows the Bundle carries.
pub const SPECIALTY: &str = "ips";
pub const LOINC: &str = "http://loinc.org";
/// IPS codes that say a required section has nothing to show.
pub const ABSENT_UNKNOWN: &str = "http://hl7.org/fhir/uv/ips/CodeSystem/absent-unknown-uv-ips";
const XHTML: &str = "http://www.w3.org/1999/xhtml";
const SRC_URN: &str = "urn:ownhealth:src:";
const DATA_ABSENT: &str = "http://hl7.org/fhir/StructureDefinition/data-absent-reason";
/// The document and section codes, compiled in.
pub const SECTIONS_TSV: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/summary/ips_sections.tsv"
));

/// One IPS section: its key in ips_sections.tsv, the summary model list it shows, the table that list's
/// rows live in, its title in the templates, and the domain whose scope hides it.
struct Section {
    key: &'static str,
    list: &'static str,
    table: &'static str,
    title: &'static str,
    domain: &'static str,
    /// For the three sections IPS requires: the absent-unknown code and display an empty one carries.
    no_info: Option<(&'static str, &'static str)>,
}

const SECTIONS: [Section; 6] = [
    Section {
        key: "problems",
        list: "conditions",
        table: "condition",
        title: "sec.conditions",
        domain: "records",
        no_info: Some(("no-problem-info", "No information about problems")),
    },
    Section {
        key: "allergies",
        list: "allergies",
        table: "allergy",
        title: "sec.allergies",
        domain: "records",
        no_info: Some(("no-allergy-info", "No information about allergies")),
    },
    Section {
        key: "medications",
        list: "medications",
        table: "medication_statement",
        title: "sec.medications",
        domain: "records",
        no_info: Some(("no-medication-info", "No information about medications")),
    },
    Section {
        key: "immunizations",
        list: "vaccines",
        table: "immunization",
        title: "sec.vaccines",
        domain: "records",
        no_info: None,
    },
    Section {
        key: "results",
        list: "markers",
        table: "observation",
        title: "sec.markers",
        domain: "labs",
        no_info: None,
    },
    Section {
        key: "procedures",
        list: "procedures",
        table: "procedure",
        title: "sec.procedures",
        domain: "records",
        no_info: None,
    },
];

/// The export: the Bundle, and what the audit row names.
#[derive(Debug, Clone)]
pub struct Ips {
    pub bundle: Value,
    /// The src ids of the rows in the Bundle, in entry order.
    pub srcs: Vec<String>,
    pub state_hash: String,
    /// `withheld.by_scope` of the summary model: the domains the caller cannot read.
    pub withheld: Value,
}

impl Ips {
    /// The file as served: pretty JSON with a final newline.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = serde_json::to_vec_pretty(&self.bundle)?;
        out.push(b'\n');
        Ok(out)
    }
}

/// section key -> (code, LOINC long name), from ips_sections.tsv.
pub fn section_code(key: &str) -> Option<(&'static str, &'static str)> {
    SECTIONS_TSV
        .lines()
        .filter(|l| !l.starts_with('#'))
        .skip(1)
        .find_map(|l| {
            let mut cols = l.split('\t');
            let (code, section, name) = (cols.next()?, cols.next()?, cols.next()?);
            (section == key).then_some((code, name))
        })
}

fn coded(key: &str) -> Result<Value> {
    let (code, display) =
        section_code(key).ok_or_else(|| Error::Invalid(format!("ips_sections.tsv has no {key} row")))?;
    Ok(json!({"coding": [{"system": LOINC, "code": code, "display": display}]}))
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn join(parts: &[&str], sep: &str) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A uuid from a hash, so ids follow the content.
fn uuid_of(bytes: &[u8]) -> Uuid {
    let h = Sha256::digest(bytes);
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    uuid::Builder::from_random_bytes(b).into_uuid()
}

/// The columns the sheet prints for a row of `list`, with their template keys.
fn columns(list: &str, row: &Value) -> Vec<(&'static str, String)> {
    let src = || ("col.src", st(row, "src").to_string());
    match list {
        "conditions" => vec![
            ("col.condition", st(row, "name").into()),
            ("col.since", st(row, "since").into()),
            src(),
        ],
        "allergies" => vec![
            ("col.substance", st(row, "substance").into()),
            ("col.criticality", st(row, "criticality").into()),
            ("col.reaction", st(row, "reaction").into()),
            src(),
        ],
        "medications" => vec![
            (
                "col.medicine",
                join(&[st(row, "name"), st(row, "strength"), st(row, "kind")], " · "),
            ),
            ("col.schedule", st(row, "schedule").into()),
            ("col.prescriber", st(row, "prescriber").into()),
            ("col.since", st(row, "since").into()),
            src(),
        ],
        "vaccines" => vec![
            ("col.vaccine", st(row, "vaccine").into()),
            ("col.date", st(row, "date").into()),
            (
                "col.dose",
                row.get("dose")
                    .filter(|d| !d.is_null())
                    .map(Value::to_string)
                    .unwrap_or_default(),
            ),
            src(),
        ],
        "markers" => vec![
            ("col.test", st(row, "name").into()),
            ("col.value", join(&[st(row, "value"), st(row, "unit")], " ")),
            ("col.range", st(row, "range").into()),
            ("col.date", st(row, "date").into()),
            ("col.status", st(row, "status_text").into()),
            src(),
        ],
        _ => vec![
            ("col.procedure", st(row, "procedure").into()),
            ("col.date", st(row, "date").into()),
            src(),
        ],
    }
}

fn label<'a>(labels: &'a Map<String, Value>, key: &'a str) -> &'a str {
    labels.get(key).and_then(Value::as_str).unwrap_or(key)
}

/// The section's narrative: the sheet's table for its rows, or the sentence that says why it is empty.
fn narrative(labels: &Map<String, Value>, list: &str, rows: &[&Value], empty: &str) -> Value {
    let body = if rows.is_empty() {
        format!("<p>{}</p>", escape(empty))
    } else {
        let head: String = columns(list, rows[0])
            .iter()
            .map(|(k, _)| format!("<th>{}</th>", escape(label(labels, k))))
            .collect();
        let cells: String = rows
            .iter()
            .map(|r| {
                let tds: String = columns(list, r)
                    .iter()
                    .map(|(_, v)| format!("<td>{}</td>", escape(v)))
                    .collect();
                format!("<tr>{tds}</tr>")
            })
            .collect();
        format!("<table><thead><tr>{head}</tr></thead><tbody>{cells}</tbody></table>")
    };
    json!({"status": "generated", "div": format!("<div xmlns=\"{XHTML}\">{body}</div>")})
}

/// Every reference that does not point inside the Bundle (or into the resource itself, `#id`)
/// keeps only its words: its display, else the reference as text.
fn localize(v: &mut Value, inside: &HashSet<String>) {
    match v {
        Value::Object(m) => {
            let outside = m
                .get("reference")
                .and_then(Value::as_str)
                .filter(|r| !r.starts_with('#') && !inside.contains(*r))
                .map(str::to_string);
            if let Some(r) = outside {
                m.shift_remove("reference");
                if !m.get("display").is_some_and(Value::is_string) {
                    m.insert("display".into(), json!(r));
                }
            }
            m.values_mut().for_each(|c| localize(c, inside));
        }
        Value::Array(a) => a.iter_mut().for_each(|c| localize(c, inside)),
        _ => {}
    }
}

/// A medicine named through a Medication resource the Bundle does not carry: its codes as the row
/// received them, and its name.
fn medication_concept(row: &Map<String, Value>) -> Value {
    let mut coding: Vec<Value> = row
        .get("codes")
        .and_then(Value::as_array)
        .map(|codes| {
            codes
                .iter()
                .filter(|c| c.get("code").and_then(Value::as_str).is_some())
                .map(|c| {
                    let mut out = Map::new();
                    for k in ["system", "code", "display"] {
                        if let Some(v) = c.get(k).filter(|v| v.is_string()) {
                            out.insert(k.into(), v.clone());
                        }
                    }
                    Value::Object(out)
                })
                .collect()
        })
        .unwrap_or_default();
    if coding.is_empty()
        && let Some(code) = row.get("code").and_then(Value::as_str)
    {
        let system = ownhealth_core::fhir::system_url(row.get("code_system").and_then(Value::as_str));
        coding.push(match system {
            Some(s) => json!({"system": s, "code": code}),
            None => json!({"code": code}),
        });
    }
    let mut concept = Map::new();
    if !coding.is_empty() {
        concept.insert("coding".into(), Value::Array(coding));
    }
    if let Some(name) = row.get("name").and_then(Value::as_str) {
        concept.insert("text".into(), json!(name));
    }
    Value::Object(concept)
}

/// A panel's parts the sheet shows (`component<n>`, or "" for the panel's own value): the others
/// leave its stored FHIR. False when no value is left, so the panel has no entry.
fn only_shown(fhir: &mut Map<String, Value>, parts: &[String]) -> bool {
    let Some(Value::Array(components)) = fhir.get_mut("component") else {
        return true; // one value, one row
    };
    let mut n = 0;
    components.retain(|_| {
        n += 1;
        parts.iter().any(|p| *p == format!("component{}", n - 1))
    });
    let left = !components.is_empty();
    if !left {
        fhir.shift_remove("component");
    }
    if !parts.iter().any(String::is_empty) {
        fhir.retain(|k, _| !k.starts_with("value") && k != "interpretation" && k != "referenceRange");
    }
    left || fhir.keys().any(|k| k.starts_with("value"))
}

/// The rows of one table, by id, each with its src's `import_ref`.
async fn rows(conn: &mut PgConnection, table: &str, ids: &[Uuid]) -> Result<Vec<Map<String, Value>>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "select to_jsonb(t.*) || jsonb_build_object('import_ref', s.import_ref) from {table} t \
         left join src s on s.id = t.src_id where t.id = any($1)"
    );
    let found: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(sql)).bind(ids).fetch_all(conn).await?;
    Ok(found
        .into_iter()
        .filter_map(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .collect())
}

/// The stored FHIR of one row, made to stand in the Bundle.
fn entry(row: &Map<String, Value>, patient_url: &str, inside: &HashSet<String>) -> Option<(String, Value)> {
    let id = row.get("id").and_then(Value::as_str)?.to_string();
    let Some(Value::Object(mut res)) = row.get("fhir").cloned() else {
        return None;
    };
    let rtype = res.get("resourceType").and_then(Value::as_str)?.to_string();
    // both kinds the importer stores as a medicine: a reference outside the resource becomes its codes
    if (rtype == "MedicationStatement" || rtype == "MedicationRequest")
        && res.get("medicationCodeableConcept").is_none()
        && res
            .get("medicationReference")
            .and_then(|r| r.get("reference"))
            .and_then(Value::as_str)
            .is_none_or(|r| !r.starts_with('#'))
    {
        res.shift_remove("medicationReference");
        res.insert("medicationCodeableConcept".into(), medication_concept(row));
    }
    let mut v = Value::Object(res);
    localize(&mut v, inside);
    let Value::Object(mut res) = v else { return None };
    required_elements(&rtype, &mut res, row);
    res.insert("id".into(), json!(id));
    let mut meta = match res.shift_remove("meta") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    if let Some(src) = row.get("src_id").and_then(Value::as_str) {
        meta.insert("source".into(), json!(format!("{SRC_URN}{src}")));
    }
    // id first, then meta, as FHIR writes them
    let mut out = Map::new();
    out.insert("resourceType".into(), json!(rtype));
    out.insert("id".into(), json!(id));
    if !meta.is_empty() {
        out.insert("meta".into(), Value::Object(meta));
    }
    for (k, val) in res {
        if !["resourceType", "id", "subject", "patient"].contains(&k.as_str()) {
            out.insert(k, val);
        }
    }
    let who = if matches!(rtype.as_str(), "AllergyIntolerance" | "Immunization") {
        "patient"
    } else {
        "subject"
    };
    out.insert(who.into(), json!({"reference": patient_url}));
    Some((id, Value::Object(out)))
}

/// The elements IPS requires (1..1) that a stored resource can lack: a vaccine, procedure or medicine
/// typed with no date, and a Condition imported without a clinicalStatus. A missing date is stated as
/// unknown with the data-absent-reason extension; a missing clinical status is the row's own column.
/// The Observation status follows the row as well: a confirmed preliminary value is final.
fn required_elements(rtype: &str, res: &mut Map<String, Value>, row: &Map<String, Value>) {
    let has =
        |res: &Map<String, Value>, prefix: &str| res.keys().any(|k| k.trim_start_matches('_').starts_with(prefix));
    let unknown = || json!({"extension": [{"url": DATA_ABSENT, "valueCode": "unknown"}]});
    match rtype {
        "Immunization" if !has(res, "occurrence") => {
            res.insert("_occurrenceDateTime".into(), unknown());
        }
        "Procedure" if !has(res, "performed") => {
            res.insert("_performedDateTime".into(), unknown());
        }
        "MedicationStatement" if !has(res, "effective") => {
            res.insert("effectivePeriod".into(), unknown());
        }
        // a value a final report confirmed: the row became current, but its stored FHIR (written at
        // insert, and immutable) still says "preliminary"
        "Observation"
            if row.get("status").and_then(Value::as_str) == Some("current")
                && res.get("status").and_then(Value::as_str) == Some("preliminary") =>
        {
            res.insert("status".into(), json!("final"));
        }
        "Condition" if !res.contains_key("clinicalStatus") => {
            let status = row.get("clinical_status").and_then(Value::as_str).unwrap_or("active");
            if ["active", "recurrence", "relapse", "inactive", "remission", "resolved"].contains(&status) {
                res.insert(
                    "clinicalStatus".into(),
                    json!({"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": status}]}),
                );
            }
        }
        _ => {}
    }
}

/// The entry of an empty required section, as the IPS examples write it: "no information about ...".
fn no_info(key: &str, id: Uuid, code: &str, display: &str, patient_url: &str) -> Value {
    let concept = json!({"coding": [{"system": ABSENT_UNKNOWN, "code": code, "display": display}]});
    let who = json!({"reference": patient_url});
    match key {
        "problems" => json!({
            "resourceType": "Condition",
            "id": id.to_string(),
            "clinicalStatus": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "active"}]},
            "code": concept,
            "subject": who,
        }),
        "allergies" => json!({
            "resourceType": "AllergyIntolerance",
            "id": id.to_string(),
            "clinicalStatus": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/allergyintolerance-clinical", "code": "active"}]},
            "code": concept,
            "patient": who,
        }),
        _ => json!({
            "resourceType": "MedicationStatement",
            "id": id.to_string(),
            "status": "unknown",
            "medicationCodeableConcept": concept,
            "subject": who,
            "effectivePeriod": {"extension": [{"url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason", "valueCode": "not-applicable"}]},
        }),
    }
}

fn patient(id: Uuid, name: &str, birth: Option<NaiveDate>, sex: Option<&str>) -> Value {
    let mut p = Map::new();
    p.insert("resourceType".into(), json!("Patient"));
    p.insert("id".into(), json!(id.to_string()));
    p.insert("name".into(), json!([{"text": name}]));
    if let Some(g) = sex.filter(|s| ["female", "male", "unknown"].contains(s)) {
        p.insert("gender".into(), json!(g));
    }
    // IPS requires a birth date (1..1): an unknown one is said with the data-absent-reason extension
    match birth {
        Some(b) => p.insert("birthDate".into(), json!(b.format("%Y-%m-%d").to_string())),
        None => p.insert(
            "_birthDate".into(),
            json!({"extension": [{"url": "http://hl7.org/fhir/StructureDefinition/data-absent-reason", "valueCode": "unknown"}]}),
        ),
    };
    Value::Object(p)
}

/// The IPS Bundle of one profile as the caller may read it, on `as_of`, in `lang` (es or en).
pub async fn build(
    conn: &mut PgConnection,
    profile_id: Uuid,
    scopes: &Scopes,
    lang: &str,
    as_of: NaiveDate,
) -> Result<Ips> {
    let opts = Options {
        lang: lang.into(),
        specialty: SPECIALTY.into(),
        sensitive_open: false,
        as_of,
    };
    let model = summary::build(&mut *conn, profile_id, scopes, &opts).await?;
    let empty = Map::new();
    let labels = model.get("labels").and_then(Value::as_object).unwrap_or(&empty);
    let (name, birth, sex): (String, Option<NaiveDate>, Option<String>) =
        sqlx::query_as("select display_name, birth_date, sex_at_birth from profile where id = $1")
            .bind(profile_id)
            .fetch_one(&mut *conn)
            .await?;
    let patient_url = format!("urn:uuid:{profile_id}");
    let withheld: Vec<&str> = model["withheld"]["by_scope"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // the rows each section shows, then their stored FHIR
    let mut chosen: Vec<Vec<&Value>> = Vec::new();
    let mut found: Vec<Vec<Map<String, Value>>> = Vec::new();
    let mut again: HashSet<String> = HashSet::new();
    for s in &SECTIONS {
        let list: Vec<&Value> = model[s.list]
            .as_array()
            .map(|a| a.iter().filter(|r| r.get("not_measured").is_none()).collect())
            .unwrap_or_default();
        let ids: Vec<Uuid> = list
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .filter_map(|i| Uuid::parse_str(i).ok())
            .collect();
        let mut found_here = rows(&mut *conn, s.table, &ids).await?;
        // a panel sent as one Observation is stored whole on each of its component rows: the first
        // row the list shows carries it, the others are only words in the narrative. The panel keeps
        // only the parts whose rows the list shows: a component rejected in review, or one the sheet
        // does not list, is not sent
        let mut panels: Vec<(usize, Vec<String>)> = Vec::new(); // (carrier row, shown parts)
        for r in &list {
            let Some(i) = found_here
                .iter()
                .position(|row| row.get("id").and_then(Value::as_str) == Some(st(r, "id")))
            else {
                continue;
            };
            let Some(fhir @ Value::Object(_)) = found_here[i].get("fhir") else {
                continue;
            };
            // the importer's src: `<bundle>/Observation/<id>#component<n>`, no `#` for the panel's own value
            let part = found_here[i]
                .get("import_ref")
                .and_then(Value::as_str)
                .and_then(|x| x.rsplit_once('#'))
                .map_or("", |(_, p)| p)
                .to_string();
            match panels
                .iter_mut()
                .find(|(c, _)| found_here[*c].get("fhir") == Some(fhir))
            {
                Some((_, parts)) => {
                    again.insert(st(r, "id").to_string());
                    parts.push(part);
                }
                None => panels.push((i, vec![part])),
            }
        }
        for (i, parts) in &panels {
            let row = &mut found_here[*i];
            if let Some(Value::Object(fhir)) = row.get_mut("fhir")
                && !only_shown(fhir, parts)
                && let Some(id) = row.get("id").and_then(Value::as_str)
            {
                again.insert(id.to_string());
            }
        }
        found.push(found_here);
        chosen.push(list);
    }
    let mut inside: HashSet<String> = HashSet::from([patient_url.clone()]);
    for f in &found {
        inside.extend(
            f.iter()
                .filter_map(|r| r.get("id").and_then(Value::as_str))
                .filter(|i| !again.contains(*i))
                .map(|i| format!("urn:uuid:{i}")),
        );
    }

    let mut entries = vec![patient(profile_id, &name, birth, sex.as_deref())];
    let mut srcs: Vec<String> = Vec::new();
    let mut sections = Vec::new();
    for ((s, list), rows) in SECTIONS.iter().zip(&chosen).zip(&found) {
        let mut shown: Vec<&Value> = Vec::new();
        let mut refs = Vec::new();
        for r in list {
            let id = st(r, "id");
            // a component row whose panel an earlier row carries: its src and words, no entry
            let res = if again.contains(id) {
                None
            } else {
                let Some(res) = rows
                    .iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
                    .and_then(|row| entry(row, &patient_url, &inside))
                else {
                    continue;
                };
                Some(res)
            };
            if let Some(src) = r.get("src").and_then(Value::as_str)
                && !srcs.iter().any(|x| x == src)
            {
                srcs.push(src.to_string());
            }
            let Some((rid, res)) = res else {
                shown.push(r);
                continue;
            };
            refs.push(json!({"reference": format!("urn:uuid:{rid}")}));
            entries.push(res);
            shown.push(r);
        }
        if refs.is_empty() {
            // IPS: an empty recommended section is left out; an empty required one carries the
            // entry that says there is no information (the profile forbids emptyReason there)
            let Some((code, display)) = s.no_info else { continue };
            let id = uuid_of(format!("{profile_id}/{}/no-info", s.key).as_bytes());
            refs.push(json!({"reference": format!("urn:uuid:{id}")}));
            entries.push(no_info(s.key, id, code, display, &patient_url));
        }
        let why = if withheld.contains(&s.domain) {
            label(
                labels,
                if s.domain == "labs" {
                    "withheld.labs"
                } else {
                    "withheld.records"
                },
            )
        } else {
            label(labels, "none")
        };
        sections.push(json!({
            "title": label(labels, s.title),
            "code": coded(s.key)?,
            "text": narrative(labels, s.list, &shown, why),
            "entry": refs,
        }));
    }

    let date = as_of.format("%Y-%m-%d").to_string();
    let doc_type = coded("document")?;
    let composition = |id: &str| {
        json!({
            "resourceType": "Composition",
            "id": id,
            "language": lang,
            "status": "final",
            "type": doc_type,
            "subject": {"reference": patient_url},
            "date": date,
            "author": [{"reference": patient_url}],
            "title": label(labels, "ips.title"),
            "section": sections,
        })
    };
    // ids follow the content: the same rows, words and date give the same document
    // (serialized in entry order: stored FHIR may hold floats, which the canonical form refuses)
    let hash = serde_json::to_vec(&json!([composition(""), entries]))?;
    let doc_id = uuid_of(&hash);
    let comp_id = uuid_of(&[hash.as_slice(), b"composition"].concat());
    let composition = composition(&comp_id.to_string());
    let mut entry: Vec<Value> = vec![json!({"fullUrl": format!("urn:uuid:{comp_id}"), "resource": composition})];
    entry.extend(
        entries
            .into_iter()
            .map(|r| json!({"fullUrl": format!("urn:uuid:{}", st(&r, "id")), "resource": r})),
    );
    let bundle = json!({
        "resourceType": "Bundle",
        "id": doc_id.to_string(),
        "language": lang,
        "identifier": {"system": "urn:ietf:rfc:3986", "value": format!("urn:uuid:{doc_id}")},
        "type": "document",
        "timestamp": format!("{date}T00:00:00Z"),
        "entry": entry,
    });
    Ok(Ips {
        bundle,
        srcs,
        state_hash: st(&model, "state_hash").to_string(),
        withheld: json!(withheld),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IPS requires a birth date, so an unknown one is stated as unknown, never left out.
    #[test]
    fn an_unknown_birth_date_is_stated_as_unknown() {
        let p = patient(Uuid::nil(), "Carmen Ortega", None, Some("female"));
        assert!(p.get("birthDate").is_none());
        assert_eq!(p["_birthDate"]["extension"][0]["valueCode"], "unknown");
        let d = NaiveDate::from_ymd_opt(1955, 3, 1);
        assert_eq!(
            patient(Uuid::nil(), "Carmen Ortega", d, None)["birthDate"],
            "1955-03-01"
        );
    }

    #[test]
    fn every_section_code_is_a_valid_loinc_code() {
        for key in ["document"].into_iter().chain(SECTIONS.iter().map(|s| s.key)) {
            let (code, name) = section_code(key).unwrap_or_default();
            assert!(!name.is_empty(), "{key}");
            // the LOINC mod-10 check digit
            let (body, check) = code.split_once('-').unwrap_or_default();
            let total: u32 = body
                .bytes()
                .rev()
                .enumerate()
                .map(|(i, b)| {
                    let d = u32::from(b - b'0') * if i % 2 == 0 { 2 } else { 1 };
                    if d > 9 { d - 9 } else { d }
                })
                .sum();
            assert_eq!(((10 - total % 10) % 10).to_string(), check, "{key} {code}");
        }
        assert_eq!(section_code("document").map(|c| c.0), Some("60591-5"));
    }

    /// A panel keeps the parts the sheet shows, and with none left it has no entry.
    #[test]
    fn a_panel_keeps_only_its_shown_parts() {
        let panel = json!({
            "resourceType": "Observation",
            "valueQuantity": {"value": 7},
            "interpretation": [{"text": "H"}],
            "component": [{"code": {"text": "a"}}, {"code": {"text": "b"}}, {"code": {"text": "c"}}],
        });
        let Value::Object(mut kept) = panel.clone() else { return };
        assert!(only_shown(&mut kept, &["component2".into(), "component0".into()]));
        assert_eq!(
            kept["component"],
            json!([{"code": {"text": "a"}}, {"code": {"text": "c"}}])
        );
        assert!(!kept.contains_key("valueQuantity") && !kept.contains_key("interpretation"));
        let Value::Object(mut own) = panel.clone() else { return };
        assert!(only_shown(&mut own, &[String::new()]));
        assert!(own.contains_key("valueQuantity") && !own.contains_key("component"));
        let Value::Object(mut none) = panel else { return };
        assert!(!only_shown(&mut none, &["component9".into()]));
    }

    #[test]
    fn a_reference_outside_the_bundle_keeps_only_its_words() {
        let inside = HashSet::from(["urn:uuid:a".to_string()]);
        let mut v = json!({
            "performer": [{"reference": "Organization/labnorte", "display": "LabNorte"}],
            "basedOn": [{"reference": "ServiceRequest/1"}],
            "hasMember": [{"reference": "urn:uuid:a"}],
            "medicationReference": {"reference": "#med"},
        });
        localize(&mut v, &inside);
        assert_eq!(
            v,
            json!({
                "performer": [{"display": "LabNorte"}],
                "basedOn": [{"display": "ServiceRequest/1"}],
                "hasMember": [{"reference": "urn:uuid:a"}],
                "medicationReference": {"reference": "#med"},
            })
        );
    }

    /// A prescription through an external Medication keeps the medicine's codes, as a statement does.
    #[test]
    fn a_referenced_medicine_keeps_its_codes_in_a_request_too() {
        for rtype in ["MedicationStatement", "MedicationRequest"] {
            let row = json!({
                "id": "00000000-0000-0000-0000-000000000001",
                "name": "Enalapril 10 mg",
                "codes": [{"system": "http://www.whocc.no/atc", "code": "C09AA02"}],
                "fhir": {"resourceType": rtype, "medicationReference": {"reference": "Medication/enalapril"}},
            });
            let row = row.as_object().cloned().unwrap_or_default();
            let (_, e) = entry(&row, "urn:uuid:p", &HashSet::new()).unwrap_or_default();
            let res = &e;
            assert!(res.get("medicationReference").is_none(), "{rtype} {e}");
            assert_eq!(
                res["medicationCodeableConcept"]["coding"][0]["code"], "C09AA02",
                "{rtype} {e}"
            );
        }
    }

    fn entry_of(row: Value) -> Value {
        let row = row.as_object().cloned().unwrap_or_default();
        entry(&row, "urn:uuid:p", &HashSet::new())
            .map(|e| e.1)
            .unwrap_or_default()
    }

    const DAR: &str = "http://hl7.org/fhir/StructureDefinition/data-absent-reason";

    /// IPS requires Immunization.occurrence[x] (1..1): a vaccine with no date says the date is unknown.
    #[test]
    fn an_undated_vaccine_states_its_date_as_unknown() {
        let e = entry_of(json!({"id": "00000000-0000-0000-0000-000000000001",
            "fhir": {"resourceType": "Immunization", "status": "completed", "vaccineCode": {"text": "Td"}}}));
        assert_eq!(e["_occurrenceDateTime"]["extension"][0]["url"], DAR, "{e}");
        assert_eq!(e["_occurrenceDateTime"]["extension"][0]["valueCode"], "unknown", "{e}");
        let dated = entry_of(json!({"id": "00000000-0000-0000-0000-000000000001",
            "fhir": {"resourceType": "Immunization", "occurrenceDateTime": "2016-05-10"}}));
        assert!(dated.get("_occurrenceDateTime").is_none(), "{dated}");
    }

    /// IPS requires Procedure.performed[x] (1..1).
    #[test]
    fn an_undated_procedure_states_its_date_as_unknown() {
        let e = entry_of(json!({"id": "00000000-0000-0000-0000-000000000001",
            "fhir": {"resourceType": "Procedure", "status": "completed", "code": {"text": "Appendectomy"}}}));
        assert_eq!(e["_performedDateTime"]["extension"][0]["valueCode"], "unknown", "{e}");
        let dated = entry_of(json!({"id": "00000000-0000-0000-0000-000000000001",
            "fhir": {"resourceType": "Procedure", "performedPeriod": {"start": "2020-01-01"}}}));
        assert!(dated.get("_performedDateTime").is_none(), "{dated}");
    }

    /// IPS requires MedicationStatement.effective[x] (1..1): a medicine with no start says so.
    #[test]
    fn an_undated_medicine_states_its_period_as_unknown() {
        let e = entry_of(json!({"id": "00000000-0000-0000-0000-000000000001",
            "fhir": {"resourceType": "MedicationStatement", "status": "active",
                     "medicationCodeableConcept": {"text": "Enalapril 10 mg"}}}));
        assert_eq!(e["effectivePeriod"]["extension"][0]["valueCode"], "unknown", "{e}");
        let dated = entry_of(json!({"id": "00000000-0000-0000-0000-000000000001",
            "fhir": {"resourceType": "MedicationStatement", "effectiveDateTime": "2026-01-01",
                     "medicationCodeableConcept": {"text": "Enalapril 10 mg"}}}));
        assert!(dated.get("effectivePeriod").is_none(), "{dated}");
    }

    /// IPS requires Condition.clinicalStatus (1..1): an imported Condition without one gets the row's.
    #[test]
    fn a_condition_without_a_clinical_status_gets_the_row_s() {
        let e = entry_of(
            json!({"id": "00000000-0000-0000-0000-000000000001", "clinical_status": "recurrence",
            "fhir": {"resourceType": "Condition", "code": {"text": "Hypertension"}}}),
        );
        assert_eq!(
            e["clinicalStatus"]["coding"][0],
            json!({"system": "http://terminology.hl7.org/CodeSystem/condition-clinical", "code": "recurrence"}),
            "{e}"
        );
        let kept = entry_of(
            json!({"id": "00000000-0000-0000-0000-000000000001", "clinical_status": "active",
            "fhir": {"resourceType": "Condition", "clinicalStatus": {"text": "as sent"}}}),
        );
        assert_eq!(kept["clinicalStatus"], json!({"text": "as sent"}));
    }

    /// A preliminary value that a final report confirmed keeps "preliminary" in its stored FHIR (rows are
    /// immutable but for their status); the summary carries only current rows, so it is final there.
    #[test]
    fn a_confirmed_value_is_final_in_the_summary() {
        let e = entry_of(
            json!({"id": "00000000-0000-0000-0000-000000000001", "status": "current",
            "fhir": {"resourceType": "Observation", "status": "preliminary", "code": {"text": "Glucosa"}}}),
        );
        assert_eq!(e["status"], "final", "{e}");
        let amended = entry_of(
            json!({"id": "00000000-0000-0000-0000-000000000001", "status": "current",
            "fhir": {"resourceType": "Observation", "status": "amended"}}),
        );
        assert_eq!(amended["status"], "amended");
    }

    #[test]
    fn a_narrative_escapes_what_it_quotes() {
        let labels = Map::new();
        let row = json!({"name": "A <b> & C", "since": "2020", "src": "src_x"});
        let n = narrative(&labels, "conditions", &[&row], "");
        assert!(
            n["div"].as_str().unwrap_or_default().contains("A &lt;b&gt; &amp; C"),
            "{n}"
        );
        let empty = narrative(&labels, "conditions", &[], "None on record.");
        assert_eq!(
            empty["div"],
            format!("<div xmlns=\"{XHTML}\"><p>None on record.</p></div>")
        );
    }
}
