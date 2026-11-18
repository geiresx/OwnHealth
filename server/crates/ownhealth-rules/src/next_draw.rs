//! The next-draw planner: before a blood draw, one list of every analyte worth asking for, each with
//! its LOINC code, why it is listed, the rule and the src ids behind it, and the preparation the cited
//! rows of `data/prevention/draw_prep.yaml` describe. Computed on read, never stored:
//!
//! ```text
//! {schema, profile, as_of, rules: ["<file> <version>", ..], state_hash, prevention_hash, count, sections}
//! section = {prep: [prep ids], items}
//! item    = {group, loinc, prep, reasons: [{kind, rule, cite, src: [..], last | value, unit, ref, flag, date}]}
//! ```
//!
//! An analyte is listed for three reasons, once per alias group with every reason it has:
//! - `recency` and `never_measured`: the lab items of the ownhealth.prevention/v1 document, which are
//!   the state body's `stale` and `never_measured` with their cite (the country's when its file names
//!   the pack rule). A gap the person snoozed or dismissed is in the document's `hidden` list and stays
//!   off the request;
//! - `lab_range`: a flag of the state body whose rule is a lab_range rule (the last value outside the
//!   lab's printed range).
//!
//! Items are grouped by their set of preparations, in the order of the file's rows; the items with
//! none come last. The plan holds codes, dates and ids only; `render` adds the words in one language.

use std::sync::OnceLock;

use chrono::NaiveDate;
use ownhealth_core::Result;
use ownhealth_core::access::Profile;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::cited::{self, CitedError};
use crate::pack::{Pack, text};
use crate::rendering::window_months;

pub const SCHEMA: &str = "ownhealth.next_draw/v1";
pub const FILE: &str = "draw_prep.yaml";
const SHIPPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/prevention/draw_prep.yaml"
));

/// One cited preparation row.
#[derive(Debug, Clone)]
pub struct Prep {
    pub id: String,
    pub groups: Vec<String>,
    pub source: Value,
    pub cite: String,
    pub name: Value,
    pub text: Value,
}

#[derive(Debug, Clone)]
pub struct DrawPrep {
    pub version: String,
    pub rows: Vec<Prep>,
    pub templates: Value,
}

/// Check and read the file: the cited loader's rules, and every group a pack analyte.
pub fn parse(bytes: &[u8], pack: &Pack) -> std::result::Result<DrawPrep, CitedError> {
    let f = cited::load_bytes(std::path::Path::new(FILE), bytes)?;
    let mut rows = Vec::new();
    for r in &f.rows {
        let s = |k: &str| r.get(k).map(text).unwrap_or_default();
        let id = s("id");
        let groups: Vec<String> = r
            .get("groups")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(text).collect())
            .unwrap_or_default();
        if id.is_empty() || groups.is_empty() {
            return Err(CitedError(format!("{FILE}: a row needs an `id` and `groups`")));
        }
        if let Some(g) = groups.iter().find(|g| !pack.analytes.contains_key(*g)) {
            return Err(CitedError(format!(
                "{FILE}: {id} names {g}, which is not a pack analyte"
            )));
        }
        rows.push(Prep {
            id,
            groups,
            source: r.get("source").cloned().unwrap_or(Value::Null),
            cite: s("cite"),
            name: r.get("name").cloned().unwrap_or(Value::Null),
            text: r.get("text").cloned().unwrap_or(Value::Null),
        });
    }
    Ok(DrawPrep {
        version: f.version,
        rows,
        templates: f.doc.get("templates").cloned().unwrap_or(Value::Null),
    })
}

/// The shipped file, read once.
pub fn load() -> Result<&'static DrawPrep> {
    static LOADED: OnceLock<std::result::Result<DrawPrep, String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let pack = crate::load_pack().map_err(|e| e.to_string())?;
            parse(SHIPPED.as_bytes(), &pack).map_err(|e| e.0)
        })
        .as_ref()
        .map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("{e}")))
}

fn srcs(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::String(s)) => vec![json!(s)],
        Some(Value::Array(a)) => a.iter().filter(|s| s.is_string()).cloned().collect(),
        _ => Vec::new(),
    }
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// The plan of one profile from its state body and its prevention document (both of one day).
pub fn plan(body: &Value, prevention: &Value, pack: &Pack, prep: &DrawPrep) -> Value {
    // (group, explicit LOINC from a value, reasons), in the order first seen
    let mut found: Vec<(String, Option<String>, Vec<Value>)> = Vec::new();
    let mut add = |group: String, loinc: Option<String>, reason: Value| match found.iter_mut().find(|f| f.0 == group) {
        Some(f) => {
            f.2.push(reason);
            if f.1.is_none() {
                f.1 = loinc;
            }
        }
        None => found.push((group, loinc, vec![reason])),
    };
    for it in list(prevention, "items") {
        let group = it.get("group").map(text).unwrap_or_default();
        if it["area"] != "lab" || group.is_empty() {
            continue;
        }
        let mut r = json!({"kind": it["kind"], "rule": it["rule"], "cite": it["cite"], "src": it["src"]});
        if let Some(last) = it["last"].get("date") {
            r["last"] = last.clone();
        }
        add(group, None, r);
    }
    for f in list(body, "flags") {
        let rule = f.get("rule").map(text).unwrap_or_default();
        let group = f.get("group").map(text).unwrap_or_default();
        let def = pack.rule(&rule);
        if group.is_empty() || def.and_then(|d| d.get("kind")).and_then(Value::as_str) != Some("lab_range") {
            continue;
        }
        let mut r = json!({
            "kind": "lab_range", "rule": rule, "cite": def.and_then(|d| d.get("cite")),
            "src": srcs(f.get("src")),
        });
        for k in ["value", "unit", "ref", "flag", "date"] {
            if let Some(v) = f.get(k) {
                r[k] = v.clone();
            }
        }
        let loinc = f.get("loinc").and_then(Value::as_str).map(str::to_string);
        add(group, loinc, r);
    }
    let count = found.len();
    // the sections: one per set of preparations, in the order of the file's rows, none last
    let mut sections: Vec<(Vec<usize>, Vec<Value>)> = Vec::new();
    for (group, loinc, reasons) in found {
        let idx: Vec<usize> = (0..prep.rows.len())
            .filter(|i| prep.rows[*i].groups.contains(&group))
            .collect();
        let ids: Vec<&str> = idx.iter().map(|i| prep.rows[*i].id.as_str()).collect();
        let item = json!({
            "group": group,
            "loinc": loinc.or_else(|| pack.default_loinc(&group)),
            "prep": ids,
            "reasons": reasons,
        });
        match sections.iter_mut().find(|s| s.0 == idx) {
            Some(s) => s.1.push(item),
            None => sections.push((idx, vec![item])),
        }
    }
    sections.sort_by_key(|s| s.0.first().copied().unwrap_or(usize::MAX));
    let sections: Vec<Value> = sections
        .into_iter()
        .map(|(idx, items)| {
            let ids: Vec<&str> = idx.iter().map(|i| prep.rows[*i].id.as_str()).collect();
            json!({"prep": ids, "items": items})
        })
        .collect();
    let mut rules = vec![json!(pack.version)];
    rules.extend(prevention.get("rules").filter(|r| r.is_string()).cloned());
    rules.push(json!(format!("{FILE} {}", prep.version)));
    json!({
        "schema": SCHEMA,
        "profile": body["profile"],
        "as_of": body["as_of"],
        "rules": rules,
        "state_hash": body["state_hash"],
        "prevention_hash": prevention["prevention_hash"],
        "count": count,
        "sections": sections,
    })
}

/// The plan of one profile on one day.
pub async fn build(conn: &mut PgConnection, profile_id: Uuid, as_of: NaiveDate) -> Result<Value> {
    let (body, ev) = crate::build_with_evaluation(&mut *conn, profile_id, as_of, None).await?;
    let pack = crate::load_pack()?;
    let prevention = crate::prevention::from_evaluation(conn, &ev, &body, &pack).await?;
    Ok(plan(&body, &prevention, &pack, load()?))
}

fn fill(template: &str, params: &[(&str, String)]) -> String {
    params
        .iter()
        .fold(template.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), v))
}

fn pick(v: &Value, lang: &str) -> String {
    v.get(lang)
        .or_else(|| v.get("en"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The template `key` of the file in `lang`.
pub fn label(key: &str, lang: &str) -> Result<String> {
    Ok(pick(&load()?.templates[key], lang))
}

/// The plan in one language: every section gains `title` and `notes` [{id, name, text, source, cite}],
/// every item `name`, every reason `text`; `labels` holds the words of the screen and the sheet.
pub fn render(plan: &Value, pack: &Pack, lang: &str) -> Result<Value> {
    let prep = load()?;
    let lang = if lang == "es" { "es" } else { "en" };
    let t = |k: &str| pick(&prep.templates[k], lang);
    let mut out = plan.clone();
    for sec in out["sections"].as_array_mut().into_iter().flatten() {
        let rows: Vec<&Prep> = list(sec, "prep")
            .iter()
            .filter_map(|id| prep.rows.iter().find(|r| Some(r.id.as_str()) == id.as_str()))
            .collect();
        let title = if rows.is_empty() {
            t("prep_none")
        } else {
            rows.iter().map(|r| pick(&r.name, lang)).collect::<Vec<_>>().join(" · ")
        };
        sec["title"] = json!(title);
        sec["notes"] = json!(
            rows.iter()
                .map(
                    |r| json!({"id": r.id, "name": pick(&r.name, lang), "text": pick(&r.text, lang),
                                "source": pick(&r.source, lang), "cite": r.cite})
                )
                .collect::<Vec<_>>()
        );
        for it in sec["items"].as_array_mut().into_iter().flatten() {
            let group = it.get("group").map(text).unwrap_or_default();
            it["name"] = json!(pack.name(&group, lang));
            for r in it["reasons"].as_array_mut().into_iter().flatten() {
                let s = |k: &str| r.get(k).map(text).unwrap_or_default();
                let kind = s("kind");
                let words = match kind.as_str() {
                    "recency" => fill(
                        &t("reason.recency"),
                        &[
                            ("last", s("last")),
                            ("months", window_months(pack, &s("rule")).to_string()),
                        ],
                    ),
                    "lab_range" => fill(
                        &t("reason.lab_range"),
                        &[
                            ("value", s("value")),
                            ("unit", s("unit")),
                            ("date", s("date")),
                            ("ref", s("ref")),
                        ],
                    ),
                    _ => t("reason.never_measured"),
                };
                r["text"] = json!(words.replace("  ", " "));
            }
        }
    }
    let mut labels = Map::new();
    for (k, v) in prep.templates.as_object().into_iter().flatten() {
        if !k.starts_with("reason.") {
            labels.insert(k.clone(), json!(pick(v, lang)));
        }
    }
    labels.insert(
        "as_of".into(),
        json!(fill(
            &t("as_of"),
            &[("as_of", plan["as_of"].as_str().unwrap_or_default().into())]
        )),
    );
    out["labels"] = Value::Object(labels);
    out["lang"] = json!(lang);
    Ok(out)
}

/// Who the sheet is for, in words: `{name, sex, age, born}`, without the name and birth date when the
/// person asked for a copy that hides them (`name` is then the blank to write it on).
pub fn person(profile: &Profile, as_of: NaiveDate, lang: &str, hide_name: bool) -> Result<Value> {
    let lang = if lang == "es" { "es" } else { "en" };
    let mut out = Map::new();
    if hide_name {
        out.insert(
            "name".into(),
            json!(format!("{} ____________________", label("name_blank", lang)?)),
        );
    } else {
        out.insert("name".into(), json!(profile.display_name));
    }
    if let Some(sex) = profile
        .sex_at_birth
        .as_deref()
        .filter(|s| ["female", "male"].contains(s))
    {
        out.insert("sex".into(), json!(label(&format!("sex.{sex}"), lang)?));
    }
    if let Some(n) = profile.age_on(as_of) {
        out.insert("age".into(), json!(fill(&label("age", lang)?, &[("n", n.to_string())])));
    }
    if let (Some(b), false) = (profile.birth_date, hide_name) {
        out.insert(
            "born".into(),
            json!(fill(&label("born", lang)?, &[("date", crate::engine::iso(b))])),
        );
    }
    Ok(Value::Object(out))
}
