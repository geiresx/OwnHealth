//! The rule pack (`state/pack.py`): the shipped YAML of `data/rules/pack/`, read
//! unchanged, plus an optional override directory (`OWNHEALTH_RULES_OVERRIDE_DIR`).
//!
//! An override file replaces the shipped file with the same name (or adds a new body system). With any
//! override the version becomes `ownhealth-rules 0.1.0+local.<sha8>`, so a changed pack never passes
//! for the shipped one. `sha256` is the hash of the canonical JSON of every loaded file, the same bytes
//! Python hashed (it keys `state_snapshot`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use ownhealth_core::canonical::canonical_json;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const OVERRIDE_ENV: &str = "OWNHEALTH_RULES_OVERRIDE_DIR";
pub const RULE_KINDS: [&str; 7] = [
    "lab_range",
    "recency",
    "rcv",
    "trend",
    "never_measured",
    "gene_drug",
    "gene_lab",
];
const NON_SYSTEM_FILES: [&str; 8] = [
    "pack.yaml",
    "analytes.yaml",
    "screening.yaml",
    "pgx.yaml",
    "pgx_phenoconversion.yaml",
    "drugs.yaml",
    "drug_labs.yaml",
    "templates.yaml",
];

macro_rules! shipped {
    ($($name:literal),* $(,)?) => {
        [$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/rules/pack/", $name)))),*]
    };
}

/// The shipped pack files, sorted by name as Python's `sorted(path.glob("*.yaml"))`.
pub const SHIPPED: [(&str, &str); 17] = shipped!(
    "analytes.yaml",
    "blood_count.yaml",
    "cardiometabolic.yaml",
    "drug_labs.yaml",
    "drugs.yaml",
    "fitness.yaml",
    "inflammation.yaml",
    "kidney.yaml",
    "liver.yaml",
    "pack.yaml",
    "pgx.yaml",
    "pgx_phenoconversion.yaml",
    "screening.yaml",
    "sleep.yaml",
    "templates.yaml",
    "thyroid.yaml",
    "vitamins.yaml",
);

const CPIC_SNAPSHOT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/rules/cpic/snapshot.json"
));

/// A broken pack (Python's `PackError`, a ValueError): HTTP 500.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PackError(pub String);

impl From<PackError> for ownhealth_core::Error {
    fn from(e: PackError) -> Self {
        ownhealth_core::Error::Other(anyhow::Error::new(e))
    }
}

type PackResult<T> = Result<T, PackError>;

/// One body system (a YAML file with `system:`).
#[derive(Debug, Clone)]
pub struct System {
    pub key: String,
    pub file: String,
    pub order: i64,
    pub names: Map<String, Value>,
    /// labs | wearables
    pub source: String,
    /// alias groups (strings, labs) or marker objects (wearables)
    pub markers: Vec<Value>,
}

impl System {
    /// The marker's key: the alias group, or the wearable `metric`.
    pub fn marker_key(spec: &Value) -> String {
        match spec {
            Value::Object(m) => m.get("metric").map(text).unwrap_or_default(),
            other => text(other),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Pack {
    pub version: String,
    pub sha256: String,
    /// Every loaded file, name -> document (the `ownhealth://rules/...` resource serves them).
    pub files: Map<String, Value>,
    pub systems: Vec<System>,
    /// Every rule of any file, with `file` (and `system`) set.
    pub rules: Vec<Map<String, Value>>,
    pub analytes: Map<String, Value>,
    pub templates: Map<String, Value>,
    pub drugs: Map<String, Value>,
    pub drug_labs: Map<String, Value>,
    pub phenoconversion: Map<String, Value>,
    pub dashboard: Map<String, Value>,
    pub limitations: Vec<String>,
    pub order: Vec<String>,
    pub loinc_to_group: HashMap<String, String>,
}

/// A template parameter: Python's `str.format` prints None as `None`.
#[derive(Debug, Clone, PartialEq)]
pub enum P {
    S(String),
    I(i64),
    None,
}

impl From<&str> for P {
    fn from(s: &str) -> P {
        P::S(s.to_string())
    }
}
impl From<String> for P {
    fn from(s: String) -> P {
        P::S(s)
    }
}
impl From<&String> for P {
    fn from(s: &String) -> P {
        P::S(s.clone())
    }
}
impl From<i64> for P {
    fn from(n: i64) -> P {
        P::I(n)
    }
}
impl From<Option<&str>> for P {
    fn from(s: Option<&str>) -> P {
        s.map_or(P::None, P::from)
    }
}
impl From<Option<String>> for P {
    fn from(s: Option<String>) -> P {
        s.map_or(P::None, P::S)
    }
}

impl std::fmt::Display for P {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            P::S(s) => f.write_str(s),
            P::I(n) => write!(f, "{n}"),
            P::None => f.write_str("None"),
        }
    }
}

impl Pack {
    pub fn rules_of<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Map<String, Value>> + 'a {
        self.rules
            .iter()
            .filter(move |r| r.get("kind").and_then(Value::as_str) == Some(kind))
    }

    pub fn rule(&self, id: &str) -> Option<&Map<String, Value>> {
        self.rules
            .iter()
            .find(|r| r.get("id").and_then(Value::as_str) == Some(id))
    }

    /// The display name of an alias group in `lang` (en as the fallback), or the group itself.
    pub fn name(&self, group: &str, lang: &str) -> String {
        match self.analytes.get(group).and_then(Value::as_object) {
            Some(a) if !a.is_empty() => {
                let pick = |k: &str| a.get(k).filter(|v| truthy(v)).map(text);
                pick(lang).or_else(|| pick("en")).unwrap_or_else(|| "None".into())
            }
            _ => group.to_string(),
        }
    }

    /// The analyte's first LOINC code, as `_default_loinc`.
    pub fn default_loinc(&self, group: &str) -> Option<String> {
        self.analytes
            .get(group)
            .and_then(|a| a.get("loinc"))
            .and_then(Value::as_array)
            .and_then(|codes| codes.first())
            .map(text)
    }

    /// `pack.text(key, lang, **params)`: a count of one takes the `.one` template when there is one.
    pub fn text(&self, key: &str, lang: &str, params: &[(&str, P)]) -> PackResult<String> {
        let one = params.iter().any(|(k, v)| *k == "n" && *v == P::I(1));
        let t = one
            .then(|| self.templates.get(&format!("{key}.one")))
            .flatten()
            .filter(|t| truthy(t))
            .or_else(|| self.templates.get(key))
            .ok_or_else(|| PackError(format!("no template {key}")))?;
        let s = t
            .get(lang)
            .filter(|v| truthy(v))
            .or_else(|| t.get("en"))
            .ok_or_else(|| PackError(format!("template {key} has no en text")))?;
        py_format(&text(s), params)
    }
}

/// `str.format(**params)` for the plain `{name}` fields our templates use.
fn py_format(template: &str, params: &[(&str, P)]) -> PackResult<String> {
    let mut out = String::with_capacity(template.len() + 32);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut field = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    field.push(c);
                }
                let value = params
                    .iter()
                    .find(|(k, _)| *k == field)
                    .ok_or_else(|| PackError(format!("template field {field} has no value")))?;
                out.push_str(&value.1.to_string());
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(value)` for the scalars a pack holds.
pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

/// `int(value)` for a number or a numeric string.
pub fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

fn yaml_doc(name: &str, src: &str) -> PackResult<Value> {
    let v: Value = serde_yaml::from_str(src).map_err(|e| PackError(format!("{name}: {e}")))?;
    // `yaml.safe_load(...) or {}`
    Ok(if truthy(&v) { v } else { Value::Object(Map::new()) })
}

/// Read every `*.yaml` of a directory, sorted by name (`_read_dir`).
pub fn read_dir(path: &Path) -> PackResult<BTreeMap<String, Value>> {
    let entries = std::fs::read_dir(path).map_err(|e| PackError(format!("{}: {e}", path.display())))?;
    let mut out = BTreeMap::new();
    for entry in entries {
        let p = entry.map_err(|e| PackError(e.to_string()))?.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !name.ends_with(".yaml") || !p.is_file() {
            continue;
        }
        let src = std::fs::read_to_string(&p).map_err(|e| PackError(format!("{name}: {e}")))?;
        out.insert(name.clone(), yaml_doc(&name, &src)?);
    }
    Ok(out)
}

/// The shipped files as documents.
pub fn shipped_files() -> PackResult<BTreeMap<String, Value>> {
    SHIPPED
        .iter()
        .map(|(n, s)| Ok((n.to_string(), yaml_doc(n, s)?)))
        .collect()
}

fn obj(v: Option<&Value>) -> Map<String, Value> {
    v.and_then(Value::as_object).cloned().unwrap_or_default()
}

fn with_file(r: &Value, file: &str, system: Option<&Value>) -> Map<String, Value> {
    let mut m = r.as_object().cloned().unwrap_or_default();
    m.insert("file".into(), Value::String(file.into()));
    if let Some(s) = system {
        m.insert("system".into(), s.clone());
    }
    m
}

fn rules_in(doc: &Value) -> Vec<Value> {
    doc.get("rules").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn check(pack: &Pack) -> PackResult<()> {
    let mut ids = std::collections::HashSet::new();
    for r in &pack.rules {
        let id = r.get("id").filter(|v| truthy(v)).map(text);
        let Some(id) = id.filter(|id| !ids.contains(id)) else {
            let shown = r.get("id").map_or("None".to_string(), py_repr);
            let file = r.get("file").map(text).unwrap_or_default();
            return Err(PackError(format!("rule id missing or repeated: {shown} in {file}")));
        };
        ids.insert(id.clone());
        let kind = r.get("kind").and_then(Value::as_str);
        if !kind.is_some_and(|k| RULE_KINDS.contains(&k)) {
            let shown = r.get("kind").map_or("None".to_string(), py_repr);
            return Err(PackError(format!("rule {id}: unknown kind {shown}")));
        }
        let cite = r.get("cite").map(text).unwrap_or_default();
        if !cite.starts_with("https://") {
            return Err(PackError(format!("rule {id}: every rule needs an https cite")));
        }
    }
    for (key, t) in &pack.templates {
        if t.get("en").is_none() || t.get("es").is_none() {
            return Err(PackError(format!("template {key} needs en and es")));
        }
    }
    Ok(())
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{s}'"),
        other => text(other),
    }
}

/// `build_pack(files, overridden)`; `files` sorted by name.
pub fn build_pack(files: &BTreeMap<String, Value>, overridden: bool) -> PackResult<Pack> {
    let head = files.get("pack.yaml").cloned().unwrap_or(Value::Object(Map::new()));
    if head.get("pack").and_then(Value::as_str) != Some("ownhealth-rules") || !head.get("version").is_some_and(truthy) {
        return Err(PackError(
            "pack.yaml must name pack ownhealth-rules and a version".into(),
        ));
    }
    for (name, doc) in files {
        canonical_json(doc).map_err(|_| {
            PackError(format!(
                "{name}: write decimal numbers as quoted strings (cv_i: \"5.6\"), not floats"
            ))
        })?;
    }
    let all: Map<String, Value> = files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let bytes = canonical_json(&Value::Object(all.clone())).map_err(|e| PackError(e.to_string()))?;
    let sha = hex::encode(Sha256::digest(&bytes));
    let mut version = format!("ownhealth-rules {}", text(&head["version"]));
    if overridden {
        version.push_str(&format!("+local.{}", &sha[..8]));
    }
    let mut systems = Vec::new();
    let mut rules: Vec<Map<String, Value>> = rules_in(&head)
        .iter()
        .map(|r| with_file(r, "pack.yaml", None))
        .collect();
    for (name, doc) in files {
        if NON_SYSTEM_FILES.contains(&name.as_str()) {
            if name == "screening.yaml" || name == "pgx.yaml" {
                rules.extend(rules_in(doc).iter().map(|r| with_file(r, name, None)));
            }
            continue;
        }
        let Some(system) = doc.get("system") else {
            return Err(PackError(format!("{name}: not a known pack file and has no `system`")));
        };
        systems.push(System {
            key: text(system),
            file: name.clone(),
            order: int(doc.get("order")).unwrap_or(99),
            names: obj(doc.get("names")),
            source: doc.get("source").map_or("labs".into(), text),
            markers: doc
                .get("markers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        });
        rules.extend(rules_in(doc).iter().map(|r| with_file(r, name, Some(system))));
    }
    systems.sort_by(|a, b| (a.order, &a.key).cmp(&(b.order, &b.key)));
    let part = |file: &str, key: &str| obj(files.get(file).and_then(|d| d.get(key)));
    let analytes = part("analytes.yaml", "analytes");
    let mut loinc_to_group = HashMap::new();
    for (group, a) in &analytes {
        for code in a.get("loinc").and_then(Value::as_array).into_iter().flatten() {
            loinc_to_group.entry(text(code)).or_insert_with(|| group.clone());
        }
    }
    let list = |key: &str| -> Vec<String> {
        head.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().map(text).collect())
            .unwrap_or_default()
    };
    let pack = Pack {
        version,
        sha256: sha,
        files: all,
        systems,
        rules,
        analytes,
        templates: part("templates.yaml", "templates"),
        drugs: part("drugs.yaml", "drugs"),
        drug_labs: part("drug_labs.yaml", "drug_labs"),
        phenoconversion: obj(files.get("pgx_phenoconversion.yaml")),
        dashboard: obj(head.get("dashboard")),
        limitations: list("limitations"),
        order: list("order"),
        loinc_to_group,
    };
    check(&pack)?;
    Ok(pack)
}

/// The shipped files plus the `*.yaml` of `override_dir` (uncached; `load_pack` caches it).
pub fn load(override_dir: Option<&str>) -> PackResult<Pack> {
    let mut files = shipped_files()?;
    let mut overridden = false;
    if let Some(dir) = override_dir {
        let extra = read_dir(Path::new(dir))?;
        overridden = !extra.is_empty();
        files.extend(extra);
    }
    build_pack(&files, overridden)
}

/// The pack in force (shipped, plus `OWNHEALTH_RULES_OVERRIDE_DIR`), loaded once per directory.
pub fn load_pack() -> PackResult<Arc<Pack>> {
    let dir = std::env::var(OVERRIDE_ENV).ok().filter(|d| !d.is_empty());
    static CACHE: OnceLock<Mutex<HashMap<Option<String>, Arc<Pack>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some(p) = cache
        .lock()
        .map_err(|_| PackError("pack cache poisoned".into()))?
        .get(&dir)
    {
        return Ok(p.clone());
    }
    let pack = Arc::new(load(dir.as_deref())?);
    cache
        .lock()
        .map_err(|_| PackError("pack cache poisoned".into()))?
        .insert(dir, pack.clone());
    Ok(pack)
}

/// The bundled CPIC snapshot (`rules/cpic/snapshot.json`, CC0).
pub fn cpic_snapshot() -> PackResult<&'static Value> {
    static SNAP: OnceLock<Result<Value, String>> = OnceLock::new();
    SNAP.get_or_init(|| serde_json::from_str(CPIC_SNAPSHOT).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| PackError(format!("cpic snapshot: {e}")))
}
