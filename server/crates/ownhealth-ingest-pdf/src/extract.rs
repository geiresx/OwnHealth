//! PDF bytes -> classification and candidate rows, with no database (`ingest/pdf/extract.py`; the
//! eval uses this too).
//!
//! Order per page (ingestion.md 7): the vendor template when its fingerprint matched and the page has
//! its header, the generic table rules otherwise. A page without a usable text layer is `pending`:
//! no OCR, it waits for agent-assisted extraction or manual entry.

use ownhealth_core::Result;
use sha2::{Digest, Sha256};

use crate::classify::{Classification, classify};
use crate::py::py_round3;
use crate::tables::{GENERIC, Row, TableSpec, extract_page};
use crate::templates::{Template, load_templates, match_template};
use crate::textlayer::{ENGINE, ENGINE_VERSION, PdfModel, read_pdf};

/// Bump when extraction code changes what it reads; part of the run's config hash.
pub const CODE_VERSION: &str = "b-1";

#[derive(Debug, Clone, PartialEq)]
pub struct PageRows {
    pub page_no: i32,
    pub spec: TableSpec,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Extraction {
    pub pdf: PdfModel,
    pub cls: Classification,
    pub template: Option<Template>,
    pub template_score: f64,
    pub pages: Vec<PageRows>,
    /// no usable text layer
    pub pending_pages: Vec<i32>,
    /// text layer read, no lab table found
    pub no_table_pages: Vec<i32>,
}

impl Extraction {
    pub fn extractor(&self) -> &str {
        self.template
            .as_ref()
            .map_or(GENERIC.extractor.as_str(), |t| t.spec.extractor.as_str())
    }

    pub fn extractor_version(&self) -> &str {
        self.template
            .as_ref()
            .map_or(GENERIC.version.as_str(), |t| t.version.as_str())
    }

    pub fn config_hash(&self) -> Vec<u8> {
        let tpl = self.template.as_ref().map_or("generic", |t| t.sha256.as_str());
        let key = format!("{CODE_VERSION}|{ENGINE}{ENGINE_VERSION}|{tpl}");
        Sha256::digest(key.as_bytes()).to_vec()
    }

    pub fn locale(&self) -> &str {
        if let Some(t) = &self.template {
            return &t.locale;
        }
        match self.cls.language.as_deref() {
            Some(l @ ("es" | "en" | "de" | "fr")) => l,
            _ => "en",
        }
    }

    pub fn results(&self) -> Vec<(&TableSpec, &Row)> {
        self.pages
            .iter()
            .flat_map(|p| p.rows.iter().filter(|r| r.kind == "result").map(move |r| (&p.spec, r)))
            .collect()
    }

    pub fn pending(&self) -> Vec<&Row> {
        self.pages
            .iter()
            .flat_map(|p| p.rows.iter().filter(|r| r.kind == "pending"))
            .collect()
    }
}

pub fn extract(data: &[u8]) -> Result<Extraction> {
    let pdf = read_pdf(data)?;
    let head_lines: Vec<String> = pdf
        .pages
        .iter()
        .take(2)
        .flat_map(|p| p.text(true).lines().map(String::from).collect::<Vec<_>>())
        .collect();
    let mut cls = classify(&head_lines);
    let (template, score) = match_template(&head_lines.join("\n"), load_templates()?);
    if let Some(t) = template
        && cls.language.is_none()
    {
        cls.language = Some(t.locale.clone());
    }
    let mut ext = Extraction {
        pdf,
        cls,
        template: template.cloned(),
        template_score: py_round3(score),
        pages: Vec::new(),
        pending_pages: Vec::new(),
        no_table_pages: Vec::new(),
    };
    for page in &ext.pdf.pages {
        if !page.text_layer_ok {
            ext.pending_pages.push(page.page_no);
            continue;
        }
        let mut spec = ext.template.as_ref().map_or(&*GENERIC, |t| &t.spec);
        let mut result = extract_page(page, spec);
        if ext.template.is_some() && result.headers == 0 {
            spec = &GENERIC;
            result = extract_page(page, spec);
        }
        if result.headers == 0 {
            ext.no_table_pages.push(page.page_no);
        }
        ext.pages.push(PageRows {
            page_no: page.page_no,
            spec: spec.clone(),
            rows: result.rows,
        });
    }
    if ext.pages.iter().any(|p| !p.rows.is_empty()) && ext.cls.doc_type == "other" {
        ext.cls.doc_type = "lab_report".into();
    }
    Ok(ext)
}
