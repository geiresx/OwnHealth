//! Wording lint (`state/wording.py`): information, never advice. Finds imperative or prescriptive
//! phrases (en, es) in any output we write ourselves. Verbatim CPIC quotes (`quote`) and wrapped
//! untrusted text are not ours and are skipped by default; everything else must have 0 hits.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// Prescriptive anywhere in a sentence.
const ANYWHERE: [&str; 9] = [
    r"\byou (?:should|must|need to|have to|ought to)\b",
    r"\b(?:should|must)\b",
    r"\bdo not\b",
    r"\bdon'?t\b",
    r"\bnever (?:take|use|stop|start)\b",
    r"\bdeb(?:e|es|en|ería|erías|erían)\b",
    r"\b(?:tiene|tienes|tienen) que\b",
    r"\bhay que\b",
    r"\bno (?:tome|tomes|tomen|use|uses|deje|dejes)\b",
];

/// Imperative when a sentence starts with it.
const START_VERBS: [&str; 76] = [
    "take",
    "stop",
    "start",
    "avoid",
    "increase",
    "decrease",
    "reduce",
    "lower",
    "raise",
    "switch",
    "consider",
    "use",
    "skip",
    "discontinue",
    "ask",
    "see",
    "talk",
    "call",
    "discuss",
    "check",
    "recheck",
    "retest",
    "get",
    "keep",
    "continue",
    "try",
    "eat",
    "drink",
    "limit",
    "add",
    "remove",
    "double",
    "halve",
    "measure",
    "schedule",
    "book",
    "contact",
    "go",
    "tome",
    "toma",
    "tomen",
    "deje",
    "deja",
    "evite",
    "evita",
    "suspenda",
    "suspende",
    "empiece",
    "empieza",
    "aumente",
    "aumenta",
    "reduzca",
    "reduce",
    "baje",
    "cambie",
    "cambia",
    "use",
    "usa",
    "consulte",
    "consulta",
    "pregunte",
    "pregunta",
    "hable",
    "habla",
    "coméntalo",
    "comente",
    "comenta",
    "repita",
    "repite",
    "mida",
    "mide",
    "pida",
    "pide",
    "vaya",
    "acuda",
    "llame",
];

static ANY: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(&format!("(?i){}", ANYWHERE.join("|"))).ok());
static START: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)^(?:{})\b", START_VERBS.join("|"))).ok());
static SPLIT: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"(?:[.;:!?·()\n]|\s[-–—]\s)+").ok());

pub const SKIP_KEYS: [&str; 1] = ["quote"];

/// Every imperative phrase in one text.
pub fn imperative_phrases(text: &str) -> Vec<String> {
    let (Some(any), Some(start), Some(split)) = (ANY.as_ref(), START.as_ref(), SPLIT.as_ref()) else {
        return vec!["<wording lint regex failed to compile>".into()];
    };
    let mut hits: Vec<String> = any.find_iter(text).map(|m| m.as_str().to_string()).collect();
    for sentence in split.split(text) {
        let s = sentence
            .trim()
            .trim_start_matches(['"', '\'', '¿', '¡', '«', '“', ' '])
            .trim();
        if let Some(m) = start.find(s) {
            hits.push(m.as_str().to_string());
        }
    }
    hits
}

/// Every (json path, phrase) hit in the strings of `obj`; `skip_keys` empty also reads untrusted text.
pub fn lint(obj: &Value, skip_keys: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(obj, skip_keys, "$", &mut out);
    out
}

fn walk(obj: &Value, skip: &[&str], path: &str, out: &mut Vec<(String, String)>) {
    match obj {
        Value::String(s) => out.extend(imperative_phrases(s).into_iter().map(|h| (path.to_string(), h))),
        Value::Object(m) => {
            if m.get("trust").and_then(Value::as_str) == Some("untrusted_text") && !skip.is_empty() {
                return;
            }
            for (k, v) in m {
                if !skip.contains(&k.as_str()) {
                    walk(v, skip, &format!("{path}.{k}"), out);
                }
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                walk(v, skip, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}
