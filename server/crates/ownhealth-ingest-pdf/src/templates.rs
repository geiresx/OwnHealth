//! Vendor templates (`ingest/pdf/templates.py`): YAML files in
//! `data/vendors/<country>/<provider>.yaml`, reviewed like code and read
//! unchanged (embedded at build time; a new file is one more line in `VENDOR_FILES`).
//!
//! A template names the provider, its fingerprint (words that must and may appear on the first page),
//! the header words of each column, the value locale, and optional flag, stop and skip words. It holds
//! anchors only: never values, never a patient block. A template whose fingerprint scores >= 0.8 reads
//! the document with prior 1.0; otherwise the generic rules read it with prior 0.95.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use ownhealth_core::{Error, Result};
use serde_yaml::Value as Yaml;
use sha2::{Digest, Sha256};

use crate::classify::{fold, has_word};
use crate::tables::{FLAG_TOKENS, SKIP_PREFIXES, STOP_PREFIXES, TableSpec};

pub const MIN_SCORE: f64 = 0.8;

/// `vendors/*/*.yaml`, sorted by path as Python's glob was.
const VENDOR_FILES: [(&str, &str); 1] = [(
    "es/labnorte.yaml",
    include_str!("../../../../data/vendors/es/labnorte.yaml"),
)];

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub id: String,
    pub version: String,
    pub provider_name: String,
    pub locale: String,
    pub all_words: Vec<String>,
    pub any_words: Vec<String>,
    pub spec: TableSpec,
    pub sha256: String,
}

impl Template {
    pub fn score(&self, text: &str) -> f64 {
        let f = fold(text);
        if !self.all_words.iter().all(|w| has_word(&f, w)) {
            return 0.0;
        }
        if self.any_words.is_empty() {
            return 1.0;
        }
        self.any_words.iter().filter(|w| has_word(&f, w)).count() as f64 / self.any_words.len() as f64
    }
}

/// Python `str(x)` of a YAML scalar.
fn scalar(v: &Yaml) -> Option<String> {
    match v {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Number(n) => Some(n.to_string()),
        Yaml::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
        _ => None,
    }
}

fn list(v: Option<&Yaml>) -> Vec<String> {
    v.and_then(Yaml::as_sequence)
        .map(|s| s.iter().filter_map(scalar).collect())
        .unwrap_or_default()
}

pub fn parse_template(raw: &str) -> Result<Template> {
    let d: Yaml = serde_yaml::from_str(raw).map_err(|e| Error::Invalid(format!("template is not YAML: {e}")))?;
    for key in ["id", "version", "provider_name", "locale", "fingerprint", "columns"] {
        if d.get(key).is_none() {
            return Err(Error::Invalid(format!("template missing {key}")));
        }
    }
    let text = |k: &str| d.get(k).and_then(scalar).unwrap_or_default();
    let columns: Vec<(String, Vec<String>)> = d
        .get("columns")
        .and_then(Yaml::as_mapping)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((scalar(k)?, list(Some(v)).iter().map(|x| fold(x)).collect())))
                .collect()
        })
        .unwrap_or_default();
    let has = |k: &str| columns.iter().any(|(c, v)| c == k && !v.is_empty());
    if !has("name") || !has("value") {
        return Err(Error::Invalid("a template needs name and value header words".into()));
    }
    let id = text("id");
    let flag_tokens: BTreeSet<String> = match list(d.get("flag_tokens")) {
        t if t.is_empty() => FLAG_TOKENS.iter().map(|s| s.to_string()).collect(),
        t => t.into_iter().collect(),
    };
    let mut stop: Vec<String> = list(d.get("stop")).iter().map(|x| fold(x)).collect();
    stop.extend(STOP_PREFIXES.iter().map(|s| s.to_string()));
    let mut skip: Vec<String> = list(d.get("skip")).iter().map(|x| fold(x)).collect();
    skip.extend(SKIP_PREFIXES.iter().map(|s| s.to_string()));
    let fp = d.get("fingerprint");
    Ok(Template {
        spec: TableSpec {
            extractor: format!("template:{id}"),
            version: text("version"),
            prior: 1.0,
            headers: columns,
            flag_tokens,
            stop,
            skip,
            locale: Some(text("locale")),
        },
        version: text("version"),
        provider_name: text("provider_name"),
        locale: text("locale"),
        all_words: list(fp.and_then(|f| f.get("all"))),
        any_words: list(fp.and_then(|f| f.get("any"))),
        sha256: hex::encode(Sha256::digest(raw.as_bytes())),
        id,
    })
}

static TEMPLATES: LazyLock<std::result::Result<Vec<Template>, String>> = LazyLock::new(|| {
    let out: Vec<Template> = VENDOR_FILES
        .iter()
        .map(|(path, raw)| parse_template(raw).map_err(|e| format!("{path}: {e}")))
        .collect::<std::result::Result<_, _>>()?;
    let ids: BTreeSet<&str> = out.iter().map(|t| t.id.as_str()).collect();
    if ids.len() != out.len() {
        return Err("duplicate template ids in vendors/".into());
    }
    Ok(out)
});

pub fn load_templates() -> Result<&'static [Template]> {
    TEMPLATES
        .as_ref()
        .map(Vec::as_slice)
        .map_err(|e| Error::Invalid(e.clone()))
}

/// The best template for a document's first pages, when it scores >= 0.8; always the best score.
pub fn match_template<'t>(first_page_text: &str, templates: &'t [Template]) -> (Option<&'t Template>, f64) {
    let mut best: Option<&Template> = None;
    let mut score = 0.0;
    for t in templates {
        let s = t.score(first_page_text);
        if s > score {
            best = Some(t);
            score = s;
        }
    }
    if score >= MIN_SCORE {
        (best, score)
    } else {
        (None, score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_fingerprint_needs_the_provider_and_its_headers() {
        let tpl = load_templates().expect("templates");
        assert_eq!(tpl.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["es/labnorte"]);
        assert!(
            match_template(
                "LabNorte\nPrueba Resultado Unidades Valores de referencia\nFecha de toma: 1/1/2026",
                tpl
            )
            .0
            .is_some()
        );
        assert!(
            match_template("Example Lab\nTest Result Units Reference range", tpl)
                .0
                .is_none()
        );
        // provider named, no table headers
        assert!(match_template("LabNorte newsletter", tpl).0.is_none());
    }

    #[test]
    fn test_template_without_name_or_value_headers_is_refused() {
        let raw = "id: x\nversion: '1'\nprovider_name: X\nlocale: es\nfingerprint: {all: [x]}\ncolumns: {unit: [u]}";
        assert!(matches!(parse_template(raw), Err(Error::Invalid(_))));
    }

    #[test]
    fn labnorte_template_reads_as_python_did() {
        let t = &load_templates().expect("templates")[0];
        assert_eq!(t.spec.extractor, "template:es/labnorte");
        assert_eq!(t.spec.version, "1");
        assert_eq!(t.spec.header("ref_range"), ["valores de referencia"]);
        assert_eq!(
            t.spec.stop[..2],
            ["documento sintetico".to_string(), "validado por".to_string()]
        );
        assert_eq!(t.spec.flag_tokens.len(), 3);
    }
}
