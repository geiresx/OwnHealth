//! Which argument error a tool reports, and its words, as 0.1.1 did: `jsonschema.validate` raises
//! `best_match(iter_errors(...))` with messages built from Python `repr`s. The jsonschema crate
//! decides validity; this walks the keywords the tool schemas use, in schema order as Python does,
//! to pick and word the same error. `None` when a schema uses a keyword it does not walk.

use ownhealth_normalize::text::py_repr;
use serde_json::{Map, Value};

use crate::envelope::py_float;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Seg {
    // a path never mixes the two at one depth in these schemas; Python would raise comparing them
    Index(usize),
    Key(String),
}

impl std::fmt::Display for Seg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Seg::Index(i) => write!(f, "{i}"),
            Seg::Key(k) => f.write_str(k),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PyError {
    /// Relative to the instance the error list was made for (a context error: the oneOf's instance).
    pub path: Vec<Seg>,
    keyword: &'static str,
    matches_type: bool,
    pub message: String,
    context: Vec<PyError>,
}

/// Python's `relevance` key; `max` keeps the first of equal keys, as Python's does.
type Key<'a> = (isize, &'a [Seg], bool, bool);

fn key(e: &PyError) -> Key<'_> {
    let weak = matches!(e.keyword, "anyOf" | "oneOf");
    (-(e.path.len() as isize), &e.path, !weak, !e.matches_type)
}

/// `jsonschema.exceptions.best_match`, with the absolute path of the error it picks.
pub fn best_match(errors: Vec<PyError>) -> Option<(Vec<Seg>, String)> {
    let mut best: Option<PyError> = None;
    for e in errors {
        if best.as_ref().is_none_or(|b| key(&e) > key(b)) {
            best = Some(e);
        }
    }
    let mut best = best?;
    let mut base: Vec<Seg> = Vec::new();
    while !best.context.is_empty() {
        let mut order: Vec<usize> = (0..best.context.len()).collect();
        // a stable sort: equal keys keep their order, as heapq.nsmallest does
        order.sort_by(|a, b| key(&best.context[*a]).cmp(&key(&best.context[*b])));
        if order.len() >= 2 && key(&best.context[order[0]]) == key(&best.context[order[1]]) {
            break;
        }
        base.extend(best.path.iter().cloned());
        let mut ctx = std::mem::take(&mut best.context);
        best = ctx.swap_remove(order[0]);
    }
    base.extend(best.path);
    Some((base, best.message))
}

/// Python's `repr` of a JSON value.
pub fn repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) => py_float(f),
            _ => n.to_string(),
        },
        Value::String(s) => py_repr(s),
        Value::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", py_repr(k), repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn is_type(v: &Value, t: &str) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => false,
    }
}

fn types_of(t: &Value) -> Vec<&str> {
    match t {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Keys that never yield an error under Python's validator with no format checker.
const ANNOTATIONS: [&str; 7] = [
    "description",
    "default",
    "title",
    "format",
    "examples",
    "$schema",
    "$id",
];

/// `Validator.iter_errors(instance)` for `schema`, or `None` on a keyword this does not walk.
pub fn iter_errors(instance: &Value, schema: &Value) -> Option<Vec<PyError>> {
    let Some(schema) = schema.as_object() else {
        // true accepts everything; false is not used by these schemas
        return schema.as_bool().filter(|b| *b).map(|_| Vec::new());
    };
    let matches_type = schema
        .get("type")
        .is_some_and(|t| types_of(t).iter().any(|t| is_type(instance, t)));
    let mut out = Vec::new();
    let err = |keyword: &'static str, message: String, context: Vec<PyError>| PyError {
        path: Vec::new(),
        keyword,
        matches_type,
        message,
        context,
    };
    for (k, v) in schema {
        match k.as_str() {
            "type" => {
                let types = types_of(v);
                if !types.iter().any(|t| is_type(instance, t)) {
                    let reprs: Vec<String> = types.iter().map(|t| py_repr(t)).collect();
                    out.push(err(
                        "type",
                        format!("{} is not of type {}", repr(instance), reprs.join(", ")),
                        vec![],
                    ));
                }
            }
            "enum" => {
                let enums = v.as_array()?;
                if !enums.contains(instance) {
                    out.push(err(
                        "enum",
                        format!("{} is not one of {}", repr(instance), repr(v)),
                        vec![],
                    ));
                }
            }
            "pattern" => {
                if let Some(s) = instance.as_str() {
                    let re = regex::Regex::new(v.as_str()?).ok()?;
                    if !re.is_match(s) {
                        out.push(err(
                            "pattern",
                            format!("{} does not match {}", repr(instance), repr(v)),
                            vec![],
                        ));
                    }
                }
            }
            "minLength" | "maxLength" => {
                let n = usize::try_from(v.as_u64()?).ok()?;
                if let Some(s) = instance.as_str() {
                    let len = s.chars().count();
                    let words = match k.as_str() {
                        "minLength" if len < n => Some(if n == 1 { "should be non-empty" } else { "is too short" }),
                        "maxLength" if len > n => Some(if n == 0 {
                            "is expected to be empty"
                        } else {
                            "is too long"
                        }),
                        _ => None,
                    };
                    if let Some(w) = words {
                        let kw = if k == "minLength" { "minLength" } else { "maxLength" };
                        out.push(err(kw, format!("{} {w}", repr(instance)), vec![]));
                    }
                }
            }
            "minItems" | "maxItems" => {
                let n = usize::try_from(v.as_u64()?).ok()?;
                if let Some(a) = instance.as_array() {
                    let words = match k.as_str() {
                        "minItems" if a.len() < n => Some(if n == 1 { "should be non-empty" } else { "is too short" }),
                        "maxItems" if a.len() > n => Some(if n == 0 {
                            "is expected to be empty"
                        } else {
                            "is too long"
                        }),
                        _ => None,
                    };
                    if let Some(w) = words {
                        let kw = if k == "minItems" { "minItems" } else { "maxItems" };
                        out.push(err(kw, format!("{} {w}", repr(instance)), vec![]));
                    }
                }
            }
            "required" => {
                if let Some(m) = instance.as_object() {
                    for p in v.as_array()? {
                        let p = p.as_str()?;
                        if !m.contains_key(p) {
                            out.push(err(
                                "required",
                                format!("{} is a required property", py_repr(p)),
                                vec![],
                            ));
                        }
                    }
                }
            }
            "properties" => {
                if let Some(m) = instance.as_object() {
                    for (p, sub) in v.as_object()? {
                        if let Some(child) = m.get(p) {
                            out.extend(descend(child, sub, Seg::Key(p.clone()))?);
                        }
                    }
                }
            }
            "additionalProperties" => {
                if let Some(m) = instance.as_object() {
                    if schema.contains_key("patternProperties") {
                        return None;
                    }
                    let known = schema.get("properties").and_then(Value::as_object);
                    let mut extras: Vec<&String> = m
                        .keys()
                        .filter(|p| !known.is_some_and(|k| k.contains_key(*p)))
                        .collect();
                    match v {
                        Value::Bool(false) if !extras.is_empty() => {
                            extras.sort();
                            let verb = if extras.len() == 1 { "was" } else { "were" };
                            let names: Vec<String> = extras.iter().map(|p| py_repr(p)).collect();
                            out.push(err(
                                "additionalProperties",
                                format!(
                                    "Additional properties are not allowed ({} {verb} unexpected)",
                                    names.join(", ")
                                ),
                                vec![],
                            ));
                        }
                        Value::Bool(_) => {}
                        Value::Object(_) => {
                            for p in extras {
                                out.extend(descend(&m[p], v, Seg::Key(p.clone()))?);
                            }
                        }
                        _ => return None,
                    }
                }
            }
            "items" => {
                if let Some(a) = instance.as_array() {
                    if !v.is_object() || schema.contains_key("prefixItems") {
                        return None;
                    }
                    for (i, item) in a.iter().enumerate() {
                        out.extend(descend(item, v, Seg::Index(i))?);
                    }
                }
            }
            "anyOf" => {
                let mut all = Vec::new();
                let mut matched = false;
                for sub in v.as_array()? {
                    let errs = iter_errors(instance, sub)?;
                    if errs.is_empty() {
                        matched = true;
                        break;
                    }
                    all.extend(errs);
                }
                if !matched {
                    let msg = format!("{} is not valid under any of the given schemas", repr(instance));
                    out.push(err("anyOf", msg, all));
                }
            }
            "oneOf" => {
                let subs = v.as_array()?;
                let mut all = Vec::new();
                let mut first_valid = None;
                for (i, sub) in subs.iter().enumerate() {
                    let errs = iter_errors(instance, sub)?;
                    if errs.is_empty() {
                        first_valid = Some(i);
                        break;
                    }
                    all.extend(errs);
                }
                match first_valid {
                    None => {
                        let msg = format!("{} is not valid under any of the given schemas", repr(instance));
                        out.push(err("oneOf", msg, all));
                    }
                    Some(i) => {
                        let mut more = Vec::new();
                        for sub in &subs[i + 1..] {
                            if iter_errors(instance, sub)?.is_empty() {
                                more.push(sub);
                            }
                        }
                        if !more.is_empty() {
                            more.push(&subs[i]);
                            let reprs: Vec<String> = more.iter().map(|s| repr(s)).collect();
                            let msg = format!("{} is valid under each of {}", repr(instance), reprs.join(", "));
                            out.push(err("oneOf", msg, vec![]));
                        }
                    }
                }
            }
            a if ANNOTATIONS.contains(&a) => {}
            _ => return None,
        }
    }
    Some(out)
}

fn descend(instance: &Value, schema: &Value, seg: Seg) -> Option<Vec<PyError>> {
    let mut errs = iter_errors(instance, schema)?;
    for e in &mut errs {
        e.path.insert(0, seg.clone());
    }
    Some(errs)
}

/// The error 0.1.1 reports for `args` against `schema`: (place, message).
pub fn python_error(args: &Map<String, Value>, schema: &Value) -> Option<(String, String)> {
    let (path, message) = best_match(iter_errors(&Value::Object(args.clone()), schema)?)?;
    let place = path.iter().map(ToString::to_string).collect::<Vec<_>>().join(".");
    Some((if place.is_empty() { "arguments".into() } else { place }, message))
}
