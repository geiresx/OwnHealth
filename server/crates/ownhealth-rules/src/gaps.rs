//! Document gaps (`data/timeline/gaps.yaml`): what a provider holds that the record lacks. The result
//! is the ownhealth.gaps/v1 document, computed on read and never stored:
//!
//! ```text
//! {schema, profile, as_of, rules: "gaps <version>", country, items, gaps_hash}
//! item = {key, rule, source, cite, src: [..], documents?: [..], labs?,
//!         date, provider, letter?           (visit_without_report)
//!         from, to, days}                   (records_hole)
//! ```
//!
//! Two rules:
//! - `visit_without_report`: a visit with a provider and no document on file from that provider,
//!   dated from the visit day to `window_days` after it, once that window has passed. Key
//!   `visit:<day>:<hash of the provider>`.
//! - `records_hole`: `min_days` or more between two dated facts with nothing dated in between. Key
//!   `hole:<from>:<to>`. A dated fact is a lab value, a diagnosis, a medicine start or stop, a vaccine,
//!   a procedure, a visit or a dated document.
//!
//! Only current, non-sensitive rows count, and nothing after `as_of`. `letter` names the access
//! request letter of the profile's country (`spain`, `eu`); a country in no list gets no letter. Each gap
//! is also a reminder: `due_items` gives the prevention document its items `gap:<key>` (kind
//! `document_request`), so a person can snooze, mark done or dismiss it on the Due list.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use chrono::NaiveDate;
use ownhealth_core::Result;
use ownhealth_core::access::{Profile, Scopes};
use ownhealth_core::canonical::canonical_json;
use ownhealth_core::documents::sensitive_document_sql;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::cited::{self, CitedError};
use crate::engine::iso;
use crate::pack::{int, text};
use crate::prevention::words;

pub const SCHEMA: &str = "ownhealth.gaps/v1";
pub const RULES: [&str; 2] = ["visit_without_report", "records_hole"];
/// The letter languages.
pub const LANGS: [&str; 2] = ["es", "en"];
/// The prefix of a gap's key on the Due list.
pub const DUE_PREFIX: &str = "gap:";

const FILE: &str = "gaps.yaml";
const SHIPPED: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/timeline/gaps.yaml"));

/// One law a letter cites.
#[derive(Debug, Clone)]
pub struct Law {
    pub id: String,
    pub cite: String,
    pub name: Value,
}

/// The access-request letter of some countries.
#[derive(Debug, Clone)]
pub struct Letter {
    pub id: String,
    pub countries: Vec<String>,
    pub laws: Vec<Law>,
    pub basis: Value,
}

#[derive(Debug, Clone)]
pub struct Gaps {
    pub version: String,
    pub window_days: i64,
    pub min_days: i64,
    /// (rule id, source, cite), in RULES order.
    pub rows: Vec<(String, String, String)>,
    pub templates: Value,
    pub letters: Vec<Letter>,
    /// The words every letter shares (`letters.text`).
    pub letter_text: Value,
}

fn err(what: impl std::fmt::Display) -> CitedError {
    CitedError(format!("{FILE}: {what}"))
}

/// Check one file's bytes: the cited loader's rules, then the rows the code evaluates, a template for
/// each of them, and a cite and a basis for each letter.
pub fn build(bytes: &[u8]) -> std::result::Result<Gaps, CitedError> {
    let f = cited::load_bytes(Path::new(FILE), bytes)?;
    let mut rows = Vec::new();
    let (mut window_days, mut min_days) = (None, None);
    for id in RULES {
        let row = f
            .rows
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| err(format!("no row `{id}`")))?;
        let source = row.get("source").map(text).filter(|s| !s.trim().is_empty());
        let source = source.ok_or_else(|| err(format!("{id}: no `source`")))?;
        rows.push((id.to_string(), source, row.get("cite").map(text).unwrap_or_default()));
        match id {
            "visit_without_report" => window_days = int(row.get("window_days")),
            _ => min_days = int(row.get("min_days")),
        }
        for t in ["title", "due_title", "why"] {
            if f.doc["templates"][t].get(id).is_none() {
                return Err(err(format!("no {t} template for `{id}`")));
            }
        }
    }
    let window_days = window_days
        .filter(|d| *d > 0)
        .ok_or_else(|| err("visit_without_report: `window_days` is not a positive number"))?;
    let min_days = min_days
        .filter(|d| *d > 0)
        .ok_or_else(|| err("records_hole: `min_days` is not a positive number"))?;
    let mut letters = Vec::new();
    for (id, l) in f.doc["letters"].as_object().into_iter().flatten() {
        if id == "text" {
            continue;
        }
        let laws = l["laws"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| err(format!("letter {id}: no `laws`")))?
            .iter()
            .map(|law| {
                let cite = law.get("cite").map(text).unwrap_or_default();
                if !cite.starts_with("https://") || law.get("name").is_none() {
                    return Err(err(format!("letter {id}: a law without an https `cite` or a `name`")));
                }
                Ok(Law {
                    id: law.get("id").map(text).unwrap_or_default(),
                    cite,
                    name: law["name"].clone(),
                })
            })
            .collect::<std::result::Result<_, _>>()?;
        if l.get("basis").is_none() {
            return Err(err(format!("letter {id}: no `basis`")));
        }
        letters.push(Letter {
            id: id.clone(),
            countries: l["countries"]
                .as_array()
                .map(|a| a.iter().map(text).collect())
                .unwrap_or_default(),
            laws,
            basis: l["basis"].clone(),
        });
    }
    let letter_text = f.doc["letters"]["text"].clone();
    if letter_text.get("title").is_none() {
        return Err(err("no `letters.text`"));
    }
    Ok(Gaps {
        version: f.version,
        window_days,
        min_days,
        rows,
        templates: f.doc["templates"].clone(),
        letters,
        letter_text,
    })
}

/// Check one file on disk (tests point it at a mutated copy).
pub fn load_path(path: &Path) -> std::result::Result<Gaps, CitedError> {
    build(&std::fs::read(path).map_err(err)?)
}

/// The shipped file, read once.
pub fn load() -> Result<&'static Gaps> {
    static LOADED: OnceLock<std::result::Result<Gaps, String>> = OnceLock::new();
    LOADED
        .get_or_init(|| build(SHIPPED.as_bytes()).map_err(|e| e.0))
        .as_ref()
        .map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("gaps: {e}")))
}

impl Gaps {
    /// The letter of one country, if it has one.
    pub fn letter_for(&self, country: Option<&str>) -> Option<&Letter> {
        let c = country?;
        self.letters.iter().find(|l| l.countries.iter().any(|x| x == c))
    }

    pub fn letter(&self, id: &str) -> Option<&Letter> {
        self.letters.iter().find(|l| l.id == id)
    }

    fn row(&self, id: &str) -> (&str, &str) {
        self.rows
            .iter()
            .find(|(r, _, _)| r == id)
            .map_or(("", ""), |(_, s, c)| (s.as_str(), c.as_str()))
    }
}

/// The text of one template in one language (en when the language has none).
pub fn pick(v: &Value, lang: &str) -> String {
    v.get(lang)
        .or_else(|| v.get("en"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn fill(template: &str, params: &[(&str, String)]) -> String {
    params
        .iter()
        .fold(template.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), v))
}

/// Whether two provider names name the same provider: case and accents folded, the words of the
/// shorter name appear in order in the longer one.
pub fn same_provider(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    !short.is_empty() && long.windows(short.len()).any(|w| w == short.as_slice())
}

/// The short hash of a provider name in a visit gap's key: the folded words, so a change of case or
/// accents keeps the key (and a person's decision about it).
fn provider_key(provider: &str) -> String {
    let h = Sha256::digest(words(provider).join(" ").as_bytes());
    hex::encode(&h[..4])
}

// --------------------------------------------------------------- the record ----

/// One dated fact: its day and its src id, or the document it is.
#[derive(Debug, Clone)]
struct Fact {
    date: NaiveDate,
    src: Option<String>,
    document: Option<Uuid>,
    lab: bool,
}

/// What is true now on a clinical table aliased `a`, and not sensitive.
fn live(a: &str) -> String {
    format!("{a}.status = 'current' and {a}.superseded_at is null and cardinality({a}.sensitivity) = 0")
}

async fn facts(conn: &mut PgConnection, pid: Uuid, as_of: NaiveDate) -> Result<Vec<Fact>> {
    let sql = format!(
        "select day, src, null::uuid as doc, lab from ( \
           select o.effective_at::date as day, o.src_id as src, true as lab from observation o \
             where o.profile_id = $1 and o.category = 'laboratory' and {o} \
           union all select coalesce(c.onset, c.effective_at::date), c.src_id, false from condition c \
             where c.profile_id = $1 and {c} \
           union all select m.started, m.src_id, false from medication_statement m where m.profile_id = $1 and {m} \
           union all select m.stopped, m.src_id, false from medication_statement m where m.profile_id = $1 and {m} \
           union all select coalesce(i.occurred, i.effective_at::date), i.src_id, false from immunization i \
             where i.profile_id = $1 and {i} \
           union all select coalesce(p.performed, p.effective_at::date), p.src_id, false from procedure p \
             where p.profile_id = $1 and {p} \
           union all select coalesce(e.period_start, e.effective_at)::date, e.src_id, false from encounter e \
             where e.profile_id = $1 and {e} \
         ) f where day is not null and day <= $2 \
         union all \
         select coalesce(d.collected_at, d.reported_at)::date, null, d.id, false from document d \
           where d.profile_id = $1 and d.status = 'active' and coalesce(d.collected_at, d.reported_at)::date <= $2 \
           and not {sens}",
        o = live("o"),
        c = live("c"),
        m = live("m"),
        i = live("i"),
        p = live("p"),
        e = live("e"),
        sens = sensitive_document_sql("d.id"),
    );
    let rows: Vec<(NaiveDate, Option<String>, Option<Uuid>, bool)> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(pid)
        .bind(as_of)
        .fetch_all(&mut *conn)
        .await?;
    let mut out: Vec<Fact> = rows
        .into_iter()
        .map(|(date, src, document, lab)| Fact {
            date,
            src,
            document,
            lab,
        })
        .collect();
    // one fact per day stands for it at a hole's edge: a record with a src id before a document, and
    // one that is not a lab value before one that is, so the edge needs the fewest scopes to show
    out.sort_by(|a, b| {
        (a.date, a.src.is_none(), a.lab, &a.src, a.document).cmp(&(b.date, b.src.is_none(), b.lab, &b.src, b.document))
    });
    out.dedup_by_key(|f| f.date);
    Ok(out)
}

/// Visits with a provider, by day: (day, provider, src).
async fn visits(conn: &mut PgConnection, pid: Uuid, as_of: NaiveDate) -> Result<Vec<(NaiveDate, String, String)>> {
    let sql = format!(
        "select coalesce(e.period_start, e.effective_at)::date as day, trim(e.provider_name), e.src_id \
         from encounter e where e.profile_id = $1 and {} and nullif(trim(e.provider_name), '') is not null \
         and coalesce(e.period_start, e.effective_at)::date <= $2 order by 1, 3",
        live("e")
    );
    Ok(sqlx::query_as(AssertSqlSafe(sql))
        .bind(pid)
        .bind(as_of)
        .fetch_all(&mut *conn)
        .await?)
}

/// Active documents with a provider and a day up to `as_of`: (day, provider).
async fn provider_documents(conn: &mut PgConnection, pid: Uuid, as_of: NaiveDate) -> Result<Vec<(NaiveDate, String)>> {
    Ok(sqlx::query_as(
        "select coalesce(collected_at, reported_at)::date, provider_name from document \
         where profile_id = $1 and status = 'active' and nullif(trim(provider_name), '') is not null \
         and coalesce(collected_at, reported_at)::date <= $2",
    )
    .bind(pid)
    .bind(as_of)
    .fetch_all(&mut *conn)
    .await?)
}

// ------------------------------------------------------------- the document ----

fn drop_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        other => other,
    }
}

/// The document of one profile on one day.
pub async fn document(conn: &mut PgConnection, profile: &Profile, as_of: NaiveDate) -> Result<Value> {
    let g = load()?;
    let country = profile.country.as_deref().filter(|c| !c.is_empty());
    let letter = g.letter_for(country).map(|l| l.id.clone());
    let mut items = Vec::new();

    let (source, cite) = g.row("visit_without_report");
    let docs = provider_documents(&mut *conn, profile.id, as_of).await?;
    for (day, provider, src) in visits(&mut *conn, profile.id, as_of).await? {
        let end = day + chrono::Duration::days(g.window_days);
        // while the window is open the report may still come: not a gap yet
        if end >= as_of {
            continue;
        }
        let reported = docs
            .iter()
            .any(|(d, p)| *d >= day && *d <= end && same_provider(p, &provider));
        if reported {
            continue;
        }
        let key = format!("visit:{}:{}", iso(day), provider_key(&provider));
        // two visits to one provider on one day are one gap: one report covers both
        if items.iter().any(|i: &Value| i["key"] == key.as_str()) {
            continue;
        }
        items.push(drop_nulls(json!({
            "key": key, "rule": "visit_without_report", "source": source, "cite": cite,
            "date": iso(day), "provider": provider, "src": [src], "letter": letter,
        })));
    }

    let (source, cite) = g.row("records_hole");
    let facts = facts(&mut *conn, profile.id, as_of).await?;
    for w in facts.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let days = (b.date - a.date).num_days();
        if days < g.min_days {
            continue;
        }
        // one medicine's start and stop share a src id: it is named once
        let mut src: Vec<&String> = [&a.src, &b.src].into_iter().flatten().collect();
        src.dedup();
        let documents: Vec<String> = [a.document, b.document]
            .into_iter()
            .flatten()
            .map(|d| d.to_string())
            .collect();
        items.push(drop_nulls(json!({
            "key": format!("hole:{}:{}", iso(a.date), iso(b.date)), "rule": "records_hole",
            "source": source, "cite": cite, "from": iso(a.date), "to": iso(b.date), "days": days,
            "src": src, "documents": if documents.is_empty() { Value::Null } else { json!(documents) },
            "labs": if a.lab || b.lab { json!(true) } else { Value::Null },
        })));
    }
    // newest first: the gap a provider can still answer for comes first
    items.sort_by(|x, y| {
        let day = |v: &Value| v.get("date").or_else(|| v.get("to")).map(text).unwrap_or_default();
        day(y).cmp(&day(x)).then_with(|| text(&x["key"]).cmp(&text(&y["key"])))
    });
    let mut doc = drop_nulls(json!({
        "schema": SCHEMA,
        "profile": profile.slug,
        "as_of": iso(as_of),
        "rules": g.version,
        "country": country,
        "items": items,
    }));
    let hash = gaps_hash(&doc)?;
    if let Some(m) = doc.as_object_mut() {
        m.insert("gaps_hash".into(), json!(hash));
    }
    Ok(doc)
}

/// The document for one profile on one day.
pub async fn build_document(conn: &mut PgConnection, profile_id: Uuid, as_of: NaiveDate) -> Result<Value> {
    let profile = ownhealth_core::access::get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| ownhealth_core::Error::Invalid("gaps: no such profile".into()))?;
    document(conn, &profile, as_of).await
}

/// The hash of a gaps document, as `state_hash` hashes the state body.
pub fn gaps_hash(doc: &Value) -> Result<String> {
    let mut clean = doc.clone();
    if let Some(m) = clean.as_object_mut() {
        m.shift_remove("gaps_hash");
    }
    let bytes = canonical_json(&clean).map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("{e}")))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// The scopes the caller lacks to see one gap: every gap is about records (read:records), and a hole
/// whose edge is a lab value also needs read:labs.
pub fn needs(item: &Value, scopes: &Scopes) -> Vec<&'static str> {
    let mut out = Vec::new();
    if !scopes.contains("read:records") {
        out.push("read:records");
    }
    if item.get("labs") == Some(&Value::Bool(true)) && !scopes.contains("read:labs") {
        out.push("read:labs");
    }
    out
}

fn params(item: &Value, g: &Gaps) -> Vec<(&'static str, String)> {
    let f = |k: &str| item.get(k).map(text).unwrap_or_default();
    let days = match item["rule"].as_str() {
        Some("visit_without_report") => g.window_days.to_string(),
        _ => f("days"),
    };
    vec![
        ("date", f("date")),
        ("provider", f("provider")),
        ("from", f("from")),
        ("to", f("to")),
        ("days", days),
    ]
}

fn template(g: &Gaps, kind: &str, rule: &str, lang: &str) -> String {
    pick(&g.templates[kind][rule], lang)
}

/// The document in one language for one caller: each item gains `title` and `why`. Items the scopes
/// do not cover are left out and counted in `withheld`, with the scopes they need in
/// `withheld_scopes`; without read:records `gaps_hash` is left out too.
pub fn render(doc: &Value, lang: &str, scopes: &Scopes) -> Result<Value> {
    let g = load()?;
    let lang = if lang == "es" { "es" } else { "en" };
    let mut whole = doc.as_object().cloned().unwrap_or_default();
    let mut withheld = 0;
    let mut lacking = BTreeSet::new();
    let mut items = Vec::new();
    for it in doc["items"].as_array().into_iter().flatten() {
        let need = needs(it, scopes);
        if !need.is_empty() {
            withheld += 1;
            lacking.extend(need);
            continue;
        }
        let rule = it["rule"].as_str().unwrap_or_default();
        let p = params(it, g);
        let mut out = it.as_object().cloned().unwrap_or_default();
        out.insert("title".into(), json!(fill(&template(g, "title", rule, lang), &p)));
        out.insert("why".into(), json!(fill(&template(g, "why", rule, lang), &p)));
        items.push(Value::Object(out));
    }
    whole.insert("items".into(), json!(items));
    whole.insert("lang".into(), json!(lang));
    if !scopes.contains("read:records") {
        whole.shift_remove("gaps_hash");
    }
    whole.insert("withheld".into(), json!(withheld));
    whole.insert("withheld_scopes".into(), json!(lacking));
    Ok(Value::Object(whole))
}

/// The gaps as items of the prevention document: key `gap:<key>`, area `document`, kind
/// `document_request`, status `due`. Dates and ids only, as the rest of that document: the provider's
/// name stays in this one.
pub fn due_items(doc: &Value) -> Vec<Value> {
    doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|it| {
            let mut m = serde_json::Map::new();
            m.insert("key".into(), json!(format!("{DUE_PREFIX}{}", text(&it["key"]))));
            m.insert("rule".into(), it["rule"].clone());
            m.insert("area".into(), json!("document"));
            m.insert("kind".into(), json!("document_request"));
            m.insert("status".into(), json!("due"));
            for k in ["date", "from", "to", "days", "source", "cite", "src", "labs", "letter"] {
                if let Some(v) = it.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            Value::Object(m)
        })
        .collect()
}

/// The title and the why of one prevention item of kind `document_request`.
pub fn due_text(item: &Value, lang: &str) -> Result<(String, String)> {
    let g = load()?;
    let rule = item["rule"].as_str().unwrap_or_default();
    let p = params(item, g);
    Ok((
        fill(&template(g, "due_title", rule, lang), &p),
        fill(&template(g, "why", rule, lang), &p),
    ))
}
