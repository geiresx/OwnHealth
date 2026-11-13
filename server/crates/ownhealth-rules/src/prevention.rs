//! Prevention (`data/prevention/`): which screenings and vaccines the country's guideline lists for a
//! person's age and sex, when each was last done, and when it is next due. The result is the
//! ownhealth.prevention/v1 document, computed on read and never stored:
//!
//! ```text
//! {schema, profile, as_of, rules: "<file> <version>", country, items, hidden, unmatched, prevention_hash}
//! item = {key, rule, area, kind, status, due, last: {date, src}, source, cite, src: [..], labs, reason_code,
//!         answer, decision: {kind, until | evidence: {table, id}}}
//! ```
//!
//! The person's inputs (migration 1130) are inputs of the evaluation like the records: answers
//! (`prevention_answer`) decide whether a row with `needs` or `unless` applies, and decisions
//! (`prevention_decision`) move an item to `hidden` or mark it. `hidden` is absent when empty, so a
//! profile without decisions keeps the hash it had before they existed.
//!
//! Items come from two places, each with one stable key:
//! - a row of the country file (`es.yaml`, `us.yaml`): key = the row id;
//! - the rule pack's lab gaps already in the state body: a never-measured analyte keeps its rule id,
//!   a stale series is `stale:<rule>:<group>`. The pack stays their only home: the loader refuses a
//!   country row whose evidence the pack already checks.
//!
//! The document holds codes, dates and ids only: language and the caller are not inputs, so the
//! hash is the same for every token. `render` adds the words in one language and drops what the
//! caller's scopes do not cover.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use chrono::{Datelike, Months, NaiveDate};
use ownhealth_core::Result;
use ownhealth_core::access::Scopes;
use ownhealth_core::canonical::canonical_json;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;

use crate::cited::{self, CitedError};
use crate::engine::{self, Evaluation, casefold, iso};
use crate::pack::{Pack, int, text};

pub const SCHEMA: &str = "ownhealth.prevention/v1";
/// A flu season runs this many months from its start; after it the next season is what is due.
const SEASON_MONTHS: u32 = 6;
const KINDS: [&str; 4] = ["once", "every_n_months", "series", "seasonal"];
const AREAS: [&str; 2] = ["screening", "vaccine"];
/// The pack rule kinds whose analytes a country row may not evaluate again.
const PACK_OWNED: [&str; 2] = ["never_measured", "recency"];
pub const STATUSES: [&str; 5] = ["overdue", "due", "upcoming", "up_to_date", "not_assessed"];

macro_rules! shipped {
    ($($name:literal),* $(,)?) => {
        [$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/prevention/", $name)))),*]
    };
}

/// The shipped files, compiled in.
const SHIPPED: [(&str, &str); 6] = shipped!(
    "pack.yaml",
    "evidence.yaml",
    "templates.yaml",
    "vaccines.tsv",
    "es.yaml",
    "us.yaml",
);

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub kind: String,
    pub area: String,
    pub sex: Option<String>,
    pub from: i32,
    pub to: Option<i32>,
    pub months: Option<u32>,
    pub interval_for: HashMap<String, u32>,
    pub evidence: Vec<String>,
    pub count_from: Option<i32>,
    pub doses: Option<usize>,
    pub gap_days: Option<i64>,
    pub season_start: Option<(u32, u32)>,
    /// The row applies only when the person gave this answer (unanswered: not_assessed, `no_answer`).
    pub needs: Option<(String, String)>,
    /// The row does not apply when the person gave this answer (unanswered: it applies).
    pub unless: Option<(String, String)>,
    pub source: String,
    pub cite: String,
    pub name: Value,
}

#[derive(Debug, Clone)]
pub struct CountryRules {
    pub file: String,
    pub version: String,
    pub rows: Vec<Row>,
    /// Rows that name a rule-pack rule instead of evaluating it: (row id, pack rule id).
    pub pack_rules: Vec<(String, String)>,
    /// The country's (source, cite) for each pack rule it names, by pack rule id.
    pub pack_cites: HashMap<String, (String, String)>,
    pub disclaimer: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    pub id: String,
    pub vaccine: bool,
    pub aliases: Vec<Vec<String>>,
    pub snomed: Vec<String>,
    pub cvx: Vec<String>,
    /// CVX codes that never count for the group, even when the record's name matches.
    pub cvx_not: Vec<String>,
    /// Names that keep a record with no listed CVX code out of the group.
    pub aliases_not: Vec<Vec<String>>,
    pub loinc: Vec<String>,
    pub alias_groups: Vec<String>,
    pub name: Value,
}

#[derive(Debug, Clone)]
pub struct Prevention {
    pub version: String,
    pub upcoming_days: i64,
    pub countries: BTreeMap<String, CountryRules>,
    pub groups: BTreeMap<String, Group>,
    pub templates: Value,
    /// The closed list of answers a person may give about themselves (`pack.yaml` answer_keys).
    pub answer_keys: Vec<String>,
}

fn err(file: &str, what: impl std::fmt::Display) -> CitedError {
    CitedError(format!("{file}: {what}"))
}

/// Accents and case ignored; words are runs of letters and digits.
pub fn words(s: &str) -> Vec<String> {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::canonical_combining_class;
    let folded: String = casefold(s)
        .nfkd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .collect();
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn holds(text: &[String], alias: &[String]) -> bool {
    !alias.is_empty() && text.windows(alias.len()).any(|w| w == alias)
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().map(text).collect())
        .unwrap_or_default()
}

fn date_field(doc: &Value, key: &str) -> Option<NaiveDate> {
    doc.get(key)
        .and_then(Value::as_str)
        .and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok())
}

fn parse_vaccines(file: &str, tsv: &str) -> std::result::Result<Vec<Group>, CitedError> {
    let mut out = Vec::new();
    for (i, line) in tsv.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let [id, cvx, en, es, label_en, label_es] = cols[..] else {
            return Err(err(file, format!("line {}: 6 tab-separated columns expected", i + 1)));
        };
        let split = |s: &str| -> Vec<String> {
            s.split('|')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect()
        };
        if label_en.trim().is_empty() || label_es.trim().is_empty() {
            return Err(err(file, format!("line {}: a label is empty", i + 1)));
        }
        let names: Vec<String> = split(en).into_iter().chain(split(es)).collect();
        out.push(Group {
            id: id.trim().to_string(),
            vaccine: true,
            aliases: names.iter().filter(|a| !a.starts_with('!')).map(|a| words(a)).collect(),
            aliases_not: names.iter().filter_map(|a| a.strip_prefix('!')).map(words).collect(),
            cvx: split(cvx).into_iter().filter(|c| !c.starts_with('!')).collect(),
            cvx_not: split(cvx)
                .iter()
                .filter_map(|c| c.strip_prefix('!'))
                .map(str::to_string)
                .collect(),
            name: json!({"en": label_en.trim(), "es": label_es.trim()}),
            ..Group::default()
        });
    }
    Ok(out)
}

fn parse_groups(file: &str, doc: &Value) -> std::result::Result<Vec<Group>, CitedError> {
    let groups = doc
        .get("groups")
        .and_then(Value::as_object)
        .ok_or_else(|| err(file, "no `groups`"))?;
    let mut out = Vec::new();
    for (id, g) in groups {
        let name = g
            .get("name")
            .cloned()
            .ok_or_else(|| err(file, format!("{id} has no name")))?;
        out.push(Group {
            id: id.clone(),
            vaccine: false,
            aliases: strings(g.get("aliases")).iter().map(|a| words(a)).collect(),
            snomed: strings(g.get("snomed")),
            loinc: strings(g.get("loinc")),
            alias_groups: strings(g.get("alias_groups")),
            name,
            ..Group::default()
        });
    }
    Ok(out)
}

fn age(v: Option<&Value>) -> Option<i32> {
    int(v).and_then(|n| i32::try_from(n).ok())
}

/// `needs` or `unless`: one `{answer_key: "yes" | "no"}`.
fn answer_condition(
    r: &Map<String, Value>,
    field: &str,
    keys: &[String],
) -> std::result::Result<Option<(String, String)>, String> {
    let Some(v) = r.get(field).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let m = v
        .as_object()
        .filter(|m| m.len() == 1)
        .ok_or_else(|| format!("{field} is one {{answer_key: yes | no}}"))?;
    let Some((k, v)) = m.iter().next() else {
        return Ok(None);
    };
    if !keys.contains(k) {
        return Err(format!(
            "{field} names {k}, which is not one of the answer_keys {keys:?}"
        ));
    }
    let v = v
        .as_str()
        .filter(|v| ["yes", "no"].contains(v))
        .ok_or_else(|| format!("{field} {k} is \"yes\" or \"no\""))?;
    Ok(Some((k.clone(), v.to_string())))
}

fn parse_row(
    file: &str,
    r: &Map<String, Value>,
    groups: &BTreeMap<String, Group>,
    answer_keys: &[String],
) -> std::result::Result<Row, CitedError> {
    let id = r
        .get("id")
        .map(text)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(file, "a row has no id"))?;
    let e = |what: &str| err(file, format!("{id}: {what}"));
    let kind = r.get("kind").map(text).unwrap_or_default();
    if !KINDS.contains(&kind.as_str()) {
        return Err(e(&format!("kind {kind:?} is not one of {KINDS:?}")));
    }
    let area = r.get("area").map(text).unwrap_or_default();
    if !AREAS.contains(&area.as_str()) {
        return Err(e(&format!("area {area:?} is not one of {AREAS:?}")));
    }
    let sex = r.get("sex").and_then(Value::as_str).map(str::to_string);
    if sex.as_deref().is_some_and(|s| !["female", "male"].contains(&s)) {
        return Err(e("sex is female or male"));
    }
    let ages = r
        .get("ages")
        .and_then(Value::as_array)
        .ok_or_else(|| e("no ages [from, to]"))?;
    let from = ages
        .first()
        .and_then(|v| age(Some(v)))
        .ok_or_else(|| e("no start age"))?;
    let to = match ages.get(1) {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            age(Some(v))
                .filter(|t| *t >= from)
                .ok_or_else(|| e("end age is not a whole year from the start age on"))?,
        ),
    };
    let evidence = strings(r.get("evidence"));
    if evidence.is_empty() {
        return Err(e("no evidence groups"));
    }
    if let Some(g) = evidence.iter().find(|g| !groups.contains_key(*g)) {
        return Err(e(&format!("unknown evidence group {g}")));
    }
    let positive = |v: &Value| {
        (!v.is_boolean())
            .then(|| int(Some(v)))
            .flatten()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let months = match r.get("months") {
        None | Some(Value::Null) => None,
        Some(v) => Some(positive(v).ok_or_else(|| e("months is not a whole number above 0"))?),
    };
    let mut interval_for = HashMap::new();
    if let Some(v) = r.get("interval_for").filter(|v| !v.is_null()) {
        let m = v.as_object().ok_or_else(|| e("interval_for is not a map"))?;
        for (k, v) in m {
            if !evidence.contains(k) {
                return Err(e(&format!("interval_for names {k}, which is not in its evidence")));
            }
            let n = positive(v).ok_or_else(|| e(&format!("interval_for {k} is not a whole number above 0")))?;
            interval_for.insert(k.clone(), n);
        }
    }
    let doses = int(r.get("doses")).and_then(|n| usize::try_from(n).ok());
    let gap_days = int(r.get("gap_days"));
    let season_start = r.get("season_start").and_then(Value::as_str).and_then(|s| {
        let (m, d) = s.split_once('-')?;
        let (m, d) = (m.parse().ok()?, d.parse().ok()?);
        NaiveDate::from_ymd_opt(2000, m, d).map(|_| (m, d))
    });
    match kind.as_str() {
        "every_n_months" if months.is_none() => return Err(e("every_n_months needs months")),
        "series" if doses.is_none() || gap_days.is_none() => return Err(e("series needs doses and gap_days")),
        "seasonal" if season_start.is_none() => return Err(e("seasonal needs season_start MM-DD")),
        _ => {}
    }
    let name = r.get("name").cloned().ok_or_else(|| e("no name"))?;
    let needs = answer_condition(r, "needs", answer_keys).map_err(|m| e(&m))?;
    let unless = answer_condition(r, "unless", answer_keys).map_err(|m| e(&m))?;
    Ok(Row {
        id: id.clone(),
        kind,
        area,
        sex,
        from,
        to,
        months,
        interval_for,
        evidence,
        count_from: age(r.get("count_from")),
        doses,
        gap_days,
        season_start,
        needs,
        unless,
        source: r.get("source").map(text).ok_or_else(|| e("no source"))?,
        cite: r.get("cite").map(text).unwrap_or_default(),
        name,
    })
}

/// The groups of alias groups a pack never_measured or recency rule already checks.
fn pack_owned(pack: &Pack) -> HashSet<String> {
    PACK_OWNED
        .iter()
        .flat_map(|k| pack.rules_of(k))
        .flat_map(|r| strings(r.get("match")))
        .collect()
}

/// Read and check every prevention file; `read` returns a file's bytes by name.
pub fn build(
    pack: &Pack,
    read: &dyn Fn(&str) -> std::result::Result<Vec<u8>, CitedError>,
) -> std::result::Result<Prevention, CitedError> {
    let at = |name: &str| Path::new("data/prevention").join(name);
    let head = cited::load_bytes(&at("pack.yaml"), &read("pack.yaml")?)?;
    let upcoming_days = int(head.doc.get("upcoming_days")).ok_or_else(|| err("pack.yaml", "no upcoming_days"))?;
    let statuses = strings(head.doc.get("statuses"));
    if statuses != STATUSES {
        return Err(err("pack.yaml", format!("statuses are {STATUSES:?}")));
    }
    let evidence = cited::load_bytes(&at("evidence.yaml"), &read("evidence.yaml")?)?;
    let templates = cited::load_bytes(&at("templates.yaml"), &read("templates.yaml")?)?;
    let answer_keys = strings(head.doc.get("answer_keys"));
    for k in &answer_keys {
        let q = templates
            .doc
            .get("answers")
            .and_then(|a| a.get(k))
            .and_then(|a| a.get("question"));
        if ["en", "es"].iter().any(|l| {
            q.and_then(|q| q.get(*l))
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
        }) {
            return Err(err(
                "templates.yaml",
                format!("answer {k} has no question in en and es"),
            ));
        }
    }
    let tsv = String::from_utf8(read("vaccines.tsv")?).map_err(|e| err("vaccines.tsv", e))?;
    let mut groups = BTreeMap::new();
    for g in parse_groups("evidence.yaml", &evidence.doc)?
        .into_iter()
        .chain(parse_vaccines("vaccines.tsv", &tsv)?)
    {
        if groups.insert(g.id.clone(), g).is_some() {
            return Err(err("evidence.yaml", "a group id is used twice"));
        }
    }
    let owned = pack_owned(pack);
    for g in groups.values() {
        let mut hit: Vec<String> = g.alias_groups.iter().filter(|a| owned.contains(*a)).cloned().collect();
        hit.extend(
            g.loinc
                .iter()
                .filter_map(|l| pack.loinc_to_group.get(l))
                .filter(|a| owned.contains(*a))
                .cloned(),
        );
        if let Some(a) = hit.first() {
            return Err(err(
                "evidence.yaml",
                format!(
                    "group {} covers {a}, which the rule pack already checks; name the pack rule instead",
                    g.id
                ),
            ));
        }
    }
    let mut countries = BTreeMap::new();
    let files = head
        .doc
        .get("countries")
        .and_then(Value::as_object)
        .ok_or_else(|| err("pack.yaml", "no countries"))?;
    let mut ids = HashSet::new();
    for (code, file) in files {
        let file = text(file);
        let f = cited::load_bytes(&at(&file), &read(&file)?)?;
        if !f
            .doc
            .get("reviewer")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            return Err(err(&file, "no `reviewer`"));
        }
        if date_field(&f.doc, "reviewed").is_none() {
            return Err(err(&file, "no `reviewed` date (YYYY-MM-DD)"));
        }
        let mut rows = Vec::new();
        let mut pack_rules = Vec::new();
        let mut pack_cites = HashMap::new();
        for r in &f.rows {
            if let Some(rule) = r.get("pack_rule").map(text) {
                let id = r.get("id").map(text).unwrap_or_default();
                let known = pack
                    .rule(&rule)
                    .and_then(|p| p.get("kind"))
                    .and_then(Value::as_str)
                    .is_some_and(|k| PACK_OWNED.contains(&k));
                if !known {
                    return Err(err(
                        &file,
                        format!("{id}: pack_rule {rule} is not a never_measured or recency rule"),
                    ));
                }
                let source = r.get("source").map(text).unwrap_or_default();
                let cite = r.get("cite").map(text).unwrap_or_default();
                pack_cites.insert(rule.clone(), (source, cite));
                pack_rules.push((id, rule));
                continue;
            }
            rows.push(parse_row(&file, r, &groups, &answer_keys)?);
        }
        for id in rows.iter().map(|r| &r.id).chain(pack_rules.iter().map(|(id, _)| id)) {
            if !ids.insert(id.clone()) {
                return Err(err(&file, format!("row id {id} is used twice")));
            }
        }
        countries.insert(
            code.clone(),
            CountryRules {
                version: format!("{file} {}", f.version),
                file,
                rows,
                pack_rules,
                pack_cites,
                disclaimer: f.doc.get("disclaimer").cloned(),
            },
        );
    }
    Ok(Prevention {
        version: head.version,
        upcoming_days,
        countries,
        groups,
        templates: templates.doc,
        answer_keys,
    })
}

/// The files of one directory (tests point it at a mutated copy).
pub fn load_dir(dir: &Path, pack: &Pack) -> std::result::Result<Prevention, CitedError> {
    build(pack, &|name| std::fs::read(dir.join(name)).map_err(|e| err(name, e)))
}

/// The shipped files, read once.
pub fn load(pack: &Pack) -> Result<&'static Prevention> {
    static LOADED: OnceLock<std::result::Result<Prevention, String>> = OnceLock::new();
    let loaded = LOADED.get_or_init(|| {
        build(pack, &|name| {
            SHIPPED
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, body)| body.as_bytes().to_vec())
                .ok_or_else(|| err(name, "not shipped"))
        })
        .map_err(|e| e.0)
    });
    loaded
        .as_ref()
        .map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("prevention pack: {e}")))
}

// ---------------------------------------------------------------- evaluation ----

/// One fact that may count as done.
#[derive(Debug, Clone)]
struct Fact {
    date: NaiveDate,
    src: String,
    group: String,
    lab: bool,
}

fn add_years(d: NaiveDate, years: i32) -> NaiveDate {
    let m = u32::try_from(years.max(0) * 12).unwrap_or(0);
    d.checked_add_months(Months::new(m)).unwrap_or(d)
}

fn add_months(d: NaiveDate, months: u32) -> NaiveDate {
    d.checked_add_months(Months::new(months)).unwrap_or(NaiveDate::MAX)
}

/// (text, code system, code, date, src id) of an immunization or a procedure.
type CodedRow = (String, Option<String>, Option<String>, NaiveDate, String);

/// The facts that may count as done. A record tagged sensitive is left out, as the state engine
/// leaves it out: the document is the same for every caller, so it never holds what only
/// read:sensitive may see.
async fn facts(
    conn: &mut PgConnection,
    profile_id: uuid::Uuid,
    as_of: NaiveDate,
    prev: &Prevention,
) -> Result<(Vec<Fact>, Vec<Value>)> {
    let mut out = Vec::new();
    let mut unmatched = Vec::new();
    let vaccines: Vec<&Group> = prev.groups.values().filter(|g| g.vaccine).collect();
    let others: Vec<&Group> = prev.groups.values().filter(|g| !g.vaccine).collect();
    let imms: Vec<CodedRow> = sqlx::query_as(
        "select vaccine, code_system, code, coalesce(occurred, effective_at::date), src_id from immunization \
         where profile_id = $1 and status = 'current' and cardinality(sensitivity) = 0 \
         and coalesce(occurred, effective_at::date) <= $2 \
         order by 4, src_id",
    )
    .bind(profile_id)
    .bind(as_of)
    .fetch_all(&mut *conn)
    .await?;
    for (name, system, code, date, src) in imms {
        let w = words(&name);
        let cvx = system
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("cvx") || s.ends_with("/cvx"));
        let coded = |list: &Vec<String>| cvx && code.as_ref().is_some_and(|c| list.contains(c));
        // A code some group lists decides on its own; the name counts only for a record without one.
        let known = vaccines.iter().any(|g| coded(&g.cvx) || coded(&g.cvx_not));
        let hits: Vec<&&Group> = vaccines
            .iter()
            .filter(|g| {
                if known {
                    coded(&g.cvx)
                } else {
                    g.aliases.iter().any(|a| holds(&w, a)) && !g.aliases_not.iter().any(|a| holds(&w, a))
                }
            })
            .collect();
        if hits.is_empty() {
            unmatched.push(json!({"name": name, "src": src}));
        }
        for g in hits {
            out.push(Fact {
                date,
                src: src.clone(),
                group: g.id.clone(),
                lab: false,
            });
        }
    }
    let procs: Vec<CodedRow> = sqlx::query_as(
        "select text, code_system, code, coalesce(performed, effective_at::date), src_id from procedure \
         where profile_id = $1 and status = 'current' and cardinality(sensitivity) = 0 \
         and coalesce(performed, effective_at::date) <= $2 \
         order by 4, src_id",
    )
    .bind(profile_id)
    .bind(as_of)
    .fetch_all(&mut *conn)
    .await?;
    for (name, system, code, date, src) in procs {
        let w = words(&name);
        let snomed = system.as_deref().is_some_and(|s| s.to_lowercase().contains("snomed"));
        for g in &others {
            if (snomed && code.as_ref().is_some_and(|c| g.snomed.contains(c))) || g.aliases.iter().any(|a| holds(&w, a))
            {
                out.push(Fact {
                    date,
                    src: src.clone(),
                    group: g.id.clone(),
                    lab: false,
                });
            }
        }
    }
    let loincs: Vec<String> = others.iter().flat_map(|g| g.loinc.clone()).collect();
    let alias_groups: Vec<String> = others.iter().flat_map(|g| g.alias_groups.clone()).collect();
    let obs: Vec<(Option<String>, String, NaiveDate, String)> = sqlx::query_as(
        "select loinc, alias_group, effective_at::date, src_id from observation where profile_id = $1 \
         and status = 'current' and cardinality(sensitivity) = 0 and effective_at < $2 \
         and (loinc = any($3) or alias_group = any($4)) \
         order by 3, src_id",
    )
    .bind(profile_id)
    .bind(engine::day_end(as_of))
    .bind(&loincs)
    .bind(&alias_groups)
    .fetch_all(&mut *conn)
    .await?;
    for (loinc, group, date, src) in obs {
        for g in &others {
            if loinc.as_ref().is_some_and(|l| g.loinc.contains(l)) || g.alias_groups.contains(&group) {
                out.push(Fact {
                    date,
                    src: src.clone(),
                    group: g.id.clone(),
                    lab: true,
                });
            }
        }
    }
    Ok((out, unmatched))
}

/// The most recent season start on or before `day`.
fn season_on(day: NaiveDate, (m, d): (u32, u32)) -> NaiveDate {
    let this = NaiveDate::from_ymd_opt(day.year(), m, d).unwrap_or(day);
    if this <= day {
        this
    } else {
        NaiveDate::from_ymd_opt(day.year() - 1, m, d).unwrap_or(day)
    }
}

fn next_season(after: NaiveDate, start: (u32, u32)) -> NaiveDate {
    let s = season_on(after, start);
    if s >= after { s } else { add_months(s, 12) }
}

fn item(row: &Row, prev: &Prevention, birth: NaiveDate, as_of: NaiveDate, facts: &[Fact]) -> Option<Value> {
    let start = add_years(birth, row.from);
    let end = row.to.map(|t| add_years(birth, t + 1));
    if end.is_some_and(|e| as_of >= e) {
        return None;
    }
    let mine: Vec<&Fact> = facts.iter().filter(|f| row.evidence.contains(&f.group)).collect();
    let floor = row.count_from.map(|a| add_years(birth, a));
    let counted: Vec<&Fact> = mine
        .iter()
        .copied()
        .filter(|f| floor.is_none_or(|fl| f.date >= fl))
        .collect();
    // before its window a row shows only when the record already holds a matching fact
    if mine.is_empty() && as_of < start && (start - as_of).num_days() > prev.upcoming_days {
        return None;
    }
    let (next, can_overdue) = match row.kind.as_str() {
        "once" => (if counted.is_empty() { Some(start) } else { None }, false),
        "every_n_months" => {
            let base = row.months.unwrap_or(12);
            let next = counted
                .iter()
                .map(|f| add_months(f.date, row.interval_for.get(&f.group).copied().unwrap_or(base)))
                .max()
                .map_or(start, |n| n.max(start));
            (Some(next), !counted.is_empty())
        }
        "series" => {
            // a dose counts only when it comes at least gap_days after the previous counted one
            let gap = chrono::Duration::days(row.gap_days.unwrap_or(0));
            let mut dated: Vec<NaiveDate> = counted.iter().map(|f| f.date).collect();
            dated.sort();
            let mut days: Vec<NaiveDate> = Vec::new();
            for d in dated {
                if days.last().is_none_or(|l| d - *l >= gap) {
                    days.push(d);
                }
            }
            let doses = row.doses.unwrap_or(1);
            match days.last() {
                _ if days.len() >= doses => (None, false),
                None => (Some(start), false),
                Some(last) => (Some((*last + gap).max(start)), true),
            }
        }
        _ => {
            let s = row.season_start.unwrap_or((10, 1));
            let current = season_on(as_of, s);
            let in_season = as_of < add_months(current, SEASON_MONTHS);
            let done = counted.iter().any(|f| f.date >= current);
            let next = if in_season && !done {
                current
            } else {
                next_season(as_of + chrono::Duration::days(1), s)
            };
            let next = next.max(if start > as_of { next_season(start, s) } else { start });
            // off season with no dose this season, the row claims nothing until the next season is
            // near: up to date needs a dose, and due needs a season that is running. Before the
            // start age this holds too when no dose is on record at all.
            if (start <= as_of || mine.is_empty())
                && !done
                && !in_season
                && (next - as_of).num_days() > prev.upcoming_days
            {
                return None;
            }
            (Some(next), false)
        }
    };
    // past the age window the guideline lists nothing, so there is no next date
    let next = next.filter(|n| end.is_none_or(|e| *n < e));
    let status = match next {
        None => "up_to_date",
        Some(n) if (n - as_of).num_days() > prev.upcoming_days => "up_to_date",
        Some(n) if n > as_of => "upcoming",
        Some(_) if can_overdue => "overdue",
        Some(_) => "due",
    };
    let last = mine.iter().max_by_key(|f| (f.date, f.src.clone()));
    // a fact a document proves (a `done` decision) has no src id: its decision names the document
    let mut src: Vec<String> = counted
        .iter()
        .map(|f| f.src.clone())
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(l) = last
        && !l.src.is_empty()
        && !src.contains(&l.src)
    {
        src.push(l.src.clone());
    }
    src.dedup();
    // an item shows a lab value when any fact it used is one, not only the latest
    let labs = counted.iter().any(|f| f.lab) || last.is_some_and(|f| f.lab);
    Some(json!({
        "key": row.id,
        "rule": row.id,
        "area": row.area,
        "kind": row.kind,
        "status": status,
        "due": next.map(iso),
        "last": last.map(|f| json!({"date": iso(f.date), "src": (!f.src.is_empty()).then_some(&f.src)})),
        "source": row.source,
        "cite": row.cite,
        "src": src,
        "labs": if labs { json!(true) } else { Value::Null },
    }))
}

fn not_assessed(row: &Row, reason: &str) -> Value {
    json!({
        "key": row.id, "rule": row.id, "area": row.area, "kind": row.kind, "status": "not_assessed",
        "source": row.source, "cite": row.cite, "src": [], "reason_code": reason,
    })
}

// ------------------------------------------------------------- person inputs ----

/// The record tables a `done` decision may point at; a document is the other kind of evidence.
pub const EVIDENCE_TABLES: [&str; 3] = ["immunization", "procedure", "encounter"];

/// What proves a `done` decision, while it still holds.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// One of EVIDENCE_TABLES, or `document`.
    pub table: String,
    pub id: uuid::Uuid,
    /// The day it was done, when the record or document has one.
    pub day: Option<NaiveDate>,
    /// The record's src id (none for a document).
    pub src: Option<String>,
}

/// The evidence a `done` decision points at, when it still holds: a record of this profile that is
/// current now (following its corrections to the version that replaced it) and not tagged sensitive,
/// or an active document of this profile. None when it was retracted, never existed, belongs to
/// another profile or is sensitive.
pub async fn evidence(
    conn: &mut PgConnection,
    profile_id: uuid::Uuid,
    table: &str,
    id: uuid::Uuid,
) -> Result<Option<Evidence>> {
    let found: Option<(Option<NaiveDate>, Option<String>)> = if table == "document" {
        sqlx::query_as(
            "select coalesce(collected_at, reported_at)::date, null::text from document \
             where id = $1 and profile_id = $2 and status = 'active'",
        )
        .bind(id)
        .bind(profile_id)
        .fetch_optional(&mut *conn)
        .await?
    } else {
        let Some(t) = EVIDENCE_TABLES.iter().find(|t| **t == table) else {
            return Ok(None);
        };
        let day = |a: &str| match *t {
            "immunization" => format!("coalesce({a}.occurred, {a}.effective_at::date)"),
            "procedure" => format!("coalesce({a}.performed, {a}.effective_at::date)"),
            _ => format!("coalesce({a}.period_start::date, {a}.effective_at::date)"),
        };
        let sql = format!(
            "with recursive chain as ( \
               select r.id, r.status, r.src_id, r.sensitivity, {} as day from {t} r \
               where r.id = $1 and r.profile_id = $2 \
               union all \
               select n.id, n.status, n.src_id, n.sensitivity, {} from {t} n join chain c on n.supersedes = c.id) \
             select day, src_id from chain where status = 'current' and cardinality(sensitivity) = 0 limit 1",
            day("r"),
            day("n"),
        );
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .bind(profile_id)
            .fetch_optional(&mut *conn)
            .await?
    };
    Ok(found.map(|(day, src)| Evidence {
        table: table.to_string(),
        id,
        day,
        src,
    }))
}

/// One current decision about one item.
#[derive(Debug, Clone)]
struct Decision {
    kind: String,
    until: Option<NaiveDate>,
    /// For `done`: the evidence while it holds; None when it no longer does, which reopens the item.
    evidence: Option<Evidence>,
}

type DecisionRow = (
    String,
    String,
    Option<NaiveDate>,
    Option<String>,
    Option<uuid::Uuid>,
    Option<uuid::Uuid>,
);

/// The person's current answers and decisions of one profile.
async fn person_inputs(
    conn: &mut PgConnection,
    profile_id: uuid::Uuid,
) -> Result<(HashMap<String, String>, BTreeMap<String, Decision>)> {
    let answers: Vec<(String, String)> =
        sqlx::query_as("select key, value from prevention_answer where profile_id = $1 and status = 'current'")
            .bind(profile_id)
            .fetch_all(&mut *conn)
            .await?;
    let rows: Vec<DecisionRow> = sqlx::query_as(
        "select item_key, kind, until, evidence_table, evidence_id, document_id from prevention_decision \
         where profile_id = $1 and status = 'current' order by item_key",
    )
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut decisions = BTreeMap::new();
    for (key, kind, until, table, record, document) in rows {
        let found = match (table, record, document) {
            (Some(t), Some(id), _) => evidence(&mut *conn, profile_id, &t, id).await?,
            (_, _, Some(id)) => evidence(&mut *conn, profile_id, "document", id).await?,
            _ => None,
        };
        decisions.insert(
            key,
            Decision {
                kind,
                until,
                evidence: found,
            },
        );
    }
    Ok((answers.into_iter().collect(), decisions))
}

/// Whether a `done` decision still covers its item on `as_of`. The evidence needs a day, on or
/// before `as_of`. A guideline row needs nothing more (the day counted as a fact). A lab gap never
/// measured is covered by any evidence. A stale lab (`last`, `due`) is covered when the evidence is
/// not older than the last value it reports, up to that day plus the rule's window: then the next
/// retest is due again, and a later stale cycle (a newer `last`) is not covered either.
fn done_holds(item: &Value, e: &Evidence, row: bool, as_of: NaiveDate) -> bool {
    let Some(day) = e.day.filter(|d| *d <= as_of) else {
        return false;
    };
    if row {
        return true;
    }
    let Some(last) = item.get("last").and_then(|l| date_field(l, "date")) else {
        return true;
    };
    let window = date_field(item, "due").map_or(0, |due| (due - last).num_days());
    day >= last && as_of <= day + chrono::Duration::days(window)
}

/// Split the items by the person's decisions: (shown, hidden). Each item with a decision that
/// counts carries it as `decision` {kind, until | evidence}:
/// - snooze: hidden up to and including `until`, shown again from the day after;
/// - dismiss: hidden;
/// - done: a guideline row stays shown (its evidence already counted as a fact); a lab gap is
///   hidden while the evidence covers it (`done_holds`). Evidence that no longer holds or no longer
///   covers the item reopens it: it shows as if there were no decision.
fn apply_decisions(
    items: Vec<Value>,
    decisions: &BTreeMap<String, Decision>,
    rows: &HashSet<&str>,
    as_of: NaiveDate,
) -> (Vec<Value>, Vec<Value>) {
    let mut shown = Vec::new();
    let mut hidden = Vec::new();
    for mut it in items {
        let key = text(&it["key"]);
        let Some(d) = decisions.get(&key) else {
            shown.push(it);
            continue;
        };
        let (hide, mark) = match (d.kind.as_str(), &d.evidence) {
            ("snooze", _) => (
                d.until.is_none_or(|u| as_of <= u),
                json!({"kind": "snooze", "until": d.until.map(iso)}),
            ),
            ("dismiss", _) => (true, json!({"kind": "dismiss"})),
            ("done", Some(e)) if done_holds(&it, e, rows.contains(key.as_str()), as_of) => (
                !rows.contains(key.as_str()),
                json!({"kind": "done", "evidence": {"table": e.table, "id": e.id}}),
            ),
            _ => {
                shown.push(it);
                continue;
            }
        };
        if let Some(m) = it.as_object_mut() {
            m.insert("decision".into(), mark);
        }
        if hide {
            hidden.push(it);
        } else {
            shown.push(it);
        }
    }
    (shown, hidden)
}

/// The answer keys some guideline row of this country uses for a person of this sex and age (every
/// row when the sex or age is not recorded; a row counts from a year before its start age to its end),
/// in the pack's order.
pub fn answer_keys_for(prev: &Prevention, country: Option<&str>, sex: Option<&str>, age: Option<i32>) -> Vec<String> {
    let rows: Vec<&Row> = country
        .and_then(|c| prev.countries.get(c))
        .map(|r| {
            r.rows
                .iter()
                .filter(|r| r.sex.is_none() || sex.is_none() || r.sex.as_deref() == sex)
                .filter(|r| age.is_none_or(|a| a + 1 >= r.from && r.to.is_none_or(|t| a <= t)))
                .collect()
        })
        .unwrap_or_default();
    prev.answer_keys
        .iter()
        .filter(|k| {
            rows.iter().any(|r| {
                [&r.needs, &r.unless]
                    .iter()
                    .any(|c| c.as_ref().is_some_and(|(n, _)| n == *k))
            })
        })
        .cloned()
        .collect()
}

/// The question of one answer key in one language.
pub fn question(prev: &Prevention, key: &str, lang: &str) -> Option<String> {
    pick(
        prev.templates
            .get("answers")
            .and_then(|a| a.get(key))
            .and_then(|a| a.get("question")),
        if lang == "es" { "es" } else { "en" },
    )
}

/// The rule pack's lab gaps of the state body, as items.
fn pack_items(body: &Value, pack: &Pack) -> Vec<Value> {
    let cite = |rule: &str| {
        pack.rule(rule)
            .and_then(|r| r.get("cite"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let list = |k: &str| body.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for n in list("never_measured") {
        let rule = n.get("rule").map(text).unwrap_or_default();
        out.push(json!({
            "key": rule, "rule": rule, "area": "lab", "kind": "never_measured", "status": "due",
            "group": n.get("group"), "cite": cite(&rule), "src": [], "labs": true,
        }));
    }
    for s in list("stale") {
        let rule = s.get("rule").map(text).unwrap_or_default();
        let group = s.get("group").map(text).unwrap_or_default();
        let last = s.get("last").and_then(Value::as_str).and_then(engine::parse_date);
        let window = pack.rule(&rule).and_then(|r| int(r.get("window_days"))).unwrap_or(0);
        out.push(json!({
            "key": format!("stale:{rule}:{group}"), "rule": rule, "area": "lab", "kind": "recency",
            "status": "overdue", "group": group,
            "due": last.map(|d| iso(d + chrono::Duration::days(window))),
            "last": last.map(|d| json!({"date": iso(d), "src": s.get("src")})),
            "cite": cite(&rule), "src": s.get("src").into_iter().cloned().collect::<Vec<_>>(), "labs": true,
        }));
    }
    out
}

fn drop_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, drop_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(drop_nulls).collect()),
        other => other,
    }
}

/// The hash of a prevention document, as `state_hash` hashes the state body.
pub fn prevention_hash(doc: &Value) -> Result<String> {
    let mut clean = doc.clone();
    if let Some(m) = clean.as_object_mut() {
        m.shift_remove("prevention_hash");
    }
    let bytes = canonical_json(&clean).map_err(|e| ownhealth_core::Error::Other(anyhow::anyhow!("{e}")))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

/// The document for one evaluation (the dashboard already holds one) and its state body.
pub async fn from_evaluation(conn: &mut PgConnection, ev: &Evaluation, body: &Value, pack: &Pack) -> Result<Value> {
    document(conn, &ev.profile, ev.as_of, body, pack).await
}

/// The document of one profile on one day, given its state body (for the pack's lab gaps).
pub async fn document(
    conn: &mut PgConnection,
    profile: &ownhealth_core::access::Profile,
    as_of: NaiveDate,
    body: &Value,
    pack: &Pack,
) -> Result<Value> {
    let prev = load(pack)?;
    let country = profile.country.as_deref().filter(|c| !c.is_empty());
    let rules = country.and_then(|c| prev.countries.get(c));
    let mut items = Vec::new();
    let mut unmatched = Vec::new();
    let (answers, decisions) = person_inputs(&mut *conn, profile.id).await?;
    if let Some(rules) = rules {
        let (facts, unknown) = facts(&mut *conn, profile.id, as_of, prev).await?;
        unmatched = unknown;
        let sex = profile
            .sex_at_birth
            .as_deref()
            .filter(|s| ["female", "male"].contains(s));
        for row in &rules.rows {
            if row.unless.as_ref().is_some_and(|(k, v)| answers.get(k) == Some(v)) {
                continue;
            }
            let unanswered = match &row.needs {
                None => None,
                Some((k, v)) => match answers.get(k) {
                    None => Some(k),
                    Some(a) if a == v => None,
                    Some(_) => continue,
                },
            };
            // a record or document a `done` decision points at counts as a fact of the row, on its own
            // day. A record already matched to one of the row's groups counts as that group alone;
            // anything else counts as the row's group with the shortest interval, so it never claims
            // longer cover than the row allows.
            let done = decisions
                .get(&row.id)
                .filter(|d| d.kind == "done")
                .and_then(|d| d.evidence.as_ref())
                .and_then(|e| e.day.filter(|d| *d <= as_of).map(|day| (day, e.src.clone())));
            let mut own = facts.clone();
            if let Some((date, src)) = done {
                // one source may hold several records, so the record is the fact of its source on its day
                let matched = src.as_ref().is_some_and(|s| {
                    facts
                        .iter()
                        .any(|f| &f.src == s && f.date == date && row.evidence.contains(&f.group))
                });
                let base = row.months.unwrap_or(12);
                let shortest = row
                    .evidence
                    .iter()
                    .min_by_key(|g| row.interval_for.get(*g).copied().unwrap_or(base));
                if let (false, Some(group)) = (matched, shortest) {
                    own.push(Fact {
                        date,
                        src: src.unwrap_or_default(),
                        group: group.clone(),
                        lab: false,
                    });
                }
            }
            match (profile.birth_date, &row.sex, sex) {
                (_, Some(want), Some(have)) if want != have => {}
                (None, _, _) => items.push(not_assessed(row, "no_birth_date")),
                (Some(birth), Some(_), None) => {
                    if item(row, prev, birth, as_of, &[]).is_some() {
                        items.push(not_assessed(row, "no_sex"));
                    }
                }
                (Some(birth), _, _) if unanswered.is_some() => {
                    if item(row, prev, birth, as_of, &[]).is_some() {
                        let mut na = not_assessed(row, "no_answer");
                        na["answer"] = json!(unanswered);
                        items.push(na);
                    }
                }
                (Some(birth), _, _) => items.extend(item(row, prev, birth, as_of, &own)),
            }
        }
    }
    // a pack rule the country's file names keeps the pack as its evaluator and cites the country's
    // guideline
    for mut it in pack_items(body, pack) {
        if let Some((source, cite)) = rules.and_then(|r| r.pack_cites.get(&text(&it["rule"])))
            && let Some(m) = it.as_object_mut()
        {
            m.insert("source".into(), json!(source));
            m.insert("cite".into(), json!(cite));
        }
        items.push(it);
    }
    let rank = |v: &Value| {
        let s = v.get("status").and_then(Value::as_str).unwrap_or_default();
        STATUSES.iter().position(|x| *x == s).unwrap_or(STATUSES.len())
    };
    let due_of = |v: &Value| {
        v.get("due")
            .and_then(Value::as_str)
            .map_or_else(|| "9999".to_string(), str::to_string)
    };
    items.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| due_of(a).cmp(&due_of(b)))
            .then_with(|| text(&a["key"]).cmp(&text(&b["key"])))
    });
    let reason = match (country, rules) {
        (None, _) => Some("no_country"),
        (Some(_), None) => Some("country_not_covered"),
        _ => None,
    };
    let row_ids: HashSet<&str> = rules
        .map(|r| r.rows.iter().map(|row| row.id.as_str()).collect())
        .unwrap_or_default();
    let (items, hidden) = apply_decisions(items, &decisions, &row_ids, as_of);
    let mut doc = drop_nulls(json!({
        "schema": SCHEMA,
        "profile": profile.slug,
        "as_of": iso(as_of),
        "rules": rules.map(|r| r.version.clone()),
        "country": country,
        "reason_code": reason,
        "items": items,
        // what the person's decisions hide; the key is absent when nothing is, so a profile without
        // decisions keeps the document (and its hash) it had before decisions existed
        "hidden": if hidden.is_empty() { Value::Null } else { json!(hidden) },
        "unmatched": unmatched,
    }));
    let hash = prevention_hash(&doc)?;
    if let Some(m) = doc.as_object_mut() {
        m.insert("prevention_hash".into(), json!(hash));
    }
    Ok(doc)
}

/// The document for one profile on one day.
pub async fn build_document(conn: &mut PgConnection, profile_id: uuid::Uuid, as_of: NaiveDate) -> Result<Value> {
    let (body, ev) = crate::build_with_evaluation(&mut *conn, profile_id, as_of, None).await?;
    let pack = crate::load_pack()?;
    from_evaluation(conn, &ev, &body, &pack).await
}

/// Whether the caller may see one item: every item is a record (read:records), and one that a lab
/// value shows also needs read:labs.
pub fn visible(item: &Value, scopes: &Scopes) -> bool {
    needs(item, scopes).is_empty()
}

/// The scopes the caller lacks to see one item.
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

fn fill(template: &str, params: &[(&str, String)]) -> String {
    params
        .iter()
        .fold(template.to_string(), |t, (k, v)| t.replace(&format!("{{{k}}}"), v))
}

fn pick(v: Option<&Value>, lang: &str) -> Option<String> {
    let v = v?;
    v.get(lang)
        .or_else(|| v.get("en"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The document in one language for one caller: every item (shown or `hidden`) gains `title` and
/// `why` (or `reason` for not_assessed), and a guideline row `record` {kind, text} for a form that
/// records it. Items the scopes do not cover are left out, counted in `withheld`, and the scopes they
/// need are named in `withheld_scopes`. Without read:records `prevention_hash` is left out too.
pub fn render(doc: &Value, pack: &Pack, lang: &str, scopes: &Scopes) -> Result<Value> {
    let prev = load(pack)?;
    let lang = if lang == "es" { "es" } else { "en" };
    let rules = doc
        .get("country")
        .and_then(Value::as_str)
        .and_then(|c| prev.countries.get(c));
    let rows: HashMap<&str, &Row> = rules
        .map(|r| r.rows.iter().map(|row| (row.id.as_str(), row)).collect())
        .unwrap_or_default();
    let t = |path: &[&str]| -> Option<String> {
        let mut v = &prev.templates;
        for p in path {
            v = v.get(*p)?;
        }
        pick(Some(v), lang)
    };
    let mut withheld = 0;
    let mut lacking: Vec<&str> = Vec::new();
    let mut whole = doc.as_object().cloned().unwrap_or_default();
    for list in ["items", "hidden"] {
        let mut items = Vec::new();
        for it in doc.get(list).and_then(Value::as_array).into_iter().flatten() {
            if !visible(it, scopes) {
                withheld += 1;
                for s in needs(it, scopes) {
                    if !lacking.contains(&s) {
                        lacking.push(s);
                    }
                }
                continue;
            }
            let mut out = it.as_object().cloned().unwrap_or_default();
            let rule = it.get("rule").map(text).unwrap_or_default();
            let (title, why) = if let Some(row) = rows.get(rule.as_str()) {
                let open = row.to.is_none();
                let key = match row.kind.as_str() {
                    "every_n_months" | "once" if open => format!("{}_open", row.kind),
                    k => k.to_string(),
                };
                let months = row.months.unwrap_or(0).to_string();
                let why = t(&["why", &key]).map(|w| {
                    fill(
                        &w,
                        &[
                            ("months", months),
                            ("from", row.from.to_string()),
                            ("to", row.to.map(|n| n.to_string()).unwrap_or_default()),
                            ("doses", row.doses.unwrap_or(1).to_string()),
                        ],
                    )
                });
                // what the Due screen's "record it" form fills in: the record kind and a name the
                // evidence join matches, so the new record counts for this row
                if let Some(g) = row.evidence.first().and_then(|g| prev.groups.get(g)) {
                    let kind = if g.vaccine { "immunization" } else { "procedure" };
                    out.insert(
                        "record".into(),
                        json!({"kind": kind, "text": pick(Some(&g.name), lang)}),
                    );
                }
                (pick(Some(&row.name), lang), why)
            } else {
                let group = it.get("group").map(text).unwrap_or_default();
                let why = if it.get("kind").and_then(Value::as_str) == Some("never_measured") {
                    pack.text("due.never", lang, &[("rule", crate::pack::P::from(&rule))])?
                } else {
                    pack.text(
                        "due.stale",
                        lang,
                        &[
                            ("rule", crate::pack::P::from(&rule)),
                            (
                                "window_months",
                                crate::pack::P::I(crate::rendering::window_months(pack, &rule)),
                            ),
                        ],
                    )?
                };
                (Some(pack.name(&group, lang)), Some(why))
            };
            out.insert("title".into(), json!(title));
            out.insert("why".into(), json!(why));
            if let Some(code) = it.get("reason_code").and_then(Value::as_str) {
                let question = it
                    .get("answer")
                    .and_then(Value::as_str)
                    .and_then(|k| question(prev, k, lang))
                    .unwrap_or_default();
                out.insert(
                    "reason".into(),
                    json!(t(&["reasons", code]).map(|r| fill(&r, &[("question", question)]))),
                );
            }
            items.push(Value::Object(out));
        }
        if list == "items" || !items.is_empty() {
            whole.insert(list.into(), Value::Array(items));
        } else {
            whole.shift_remove(list);
        }
    }
    whole.insert("lang".into(), json!(lang));
    if let Some(code) = doc.get("reason_code").and_then(Value::as_str) {
        whole.insert("reason".into(), json!(t(&["reasons", code])));
    }
    if let Some(d) = rules.and_then(|r| r.disclaimer.as_ref()) {
        whole.insert("disclaimer".into(), json!(pick(Some(d), lang)));
    }
    // the hash covers immunizations, procedures, encounters and the person's decisions and answers,
    // which the state body does not hold: without read:records the same hash twice would still say
    // whether one of them changed. (A lab change shows in the state_hash every token gets already.)
    if !scopes.contains("read:records") {
        whole.shift_remove("unmatched");
        whole.shift_remove("prevention_hash");
    }
    whole.insert("withheld".into(), json!(withheld));
    lacking.sort_unstable();
    whole.insert("withheld_scopes".into(), json!(lacking));
    Ok(Value::Object(whole))
}

/// How many items the `due` section of get_health_status lists; the rest are counted in `due_total`.
pub const STATUS_TOP: usize = 3;
/// The statuses that count as due in the status section.
const OPEN: [&str; 3] = ["overdue", "due", "upcoming"];

/// The `due` section of get_health_status: the first open screenings and vaccines as
/// `{key, status, due, rule}` (rule only when it is not the key), their count `due_total` and the
/// document's hash. An item due or overdue that was never recorded has no `due`: its date is where
/// the age window opened, not a deadline. Rule ids, no URLs: the cites stay in the document. The lab gaps are left out, because the
/// status lists them already under never_measured and stale; an item a lab value shows needs
/// read:labs. Open items the person's decisions hide are left out and counted in `due_hidden`. None
/// when nothing is open or hidden (the status page has 2 KB and spends none of it on an empty list)
/// or the profile is not in the store.
pub async fn status_section(
    conn: &mut PgConnection,
    profile_id: uuid::Uuid,
    as_of: NaiveDate,
    body: &Value,
    pack: &Pack,
    scopes: &Scopes,
) -> Result<Option<Value>> {
    let Some(profile) = ownhealth_core::access::get_profile(&mut *conn, profile_id).await? else {
        return Ok(None);
    };
    let doc = document(conn, &profile, as_of, body, pack).await?;
    let open_in = |list: &str| -> Vec<Value> {
        doc[list]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["area"] != "lab" && visible(i, scopes))
            .filter(|i| i["status"].as_str().is_some_and(|s| OPEN.contains(&s)))
            .cloned()
            .collect()
    };
    let open = open_in("items");
    // open items the person's decisions hide (snoozed, dismissed, done): counted, never listed
    let hidden = open_in("hidden").len();
    let items: Vec<Value> = open
        .iter()
        .take(STATUS_TOP)
        .map(|i| {
            let mut m = Map::new();
            for k in ["key", "status"] {
                if let Some(v) = i.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            if let Some(d) = i.get("due")
                && (i.get("last").is_some() || i["status"] == "upcoming")
            {
                m.insert("due".into(), d.clone());
            }
            if i["rule"] != i["key"] {
                m.insert("rule".into(), i["rule"].clone());
            }
            Value::Object(m)
        })
        .collect();
    if open.is_empty() && hidden == 0 {
        return Ok(None);
    }
    // the scalars first and named for the section, so the text form does not read them as fields
    // of the whole status
    let mut section = Map::new();
    section.insert("due_total".into(), json!(open.len()));
    if hidden > 0 {
        section.insert("due_hidden".into(), json!(hidden));
    }
    section.insert("prevention_hash".into(), doc["prevention_hash"].clone());
    if !items.is_empty() {
        section.insert("items".into(), json!(items));
    }
    Ok(Some(Value::Object(section)))
}
