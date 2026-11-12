//! The wording gate (CI step "Wording lint"): information, never advice. Every text we write for a
//! person (status, dashboard, drug check, and every es/en string under `data/`) has 0 imperative
//! phrases outside attributed quotes. A new text source adds one line to `SOURCES`; a new data file
//! or directory is read with no code change. The controls below prove the gate can go red.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use std::future::Future;
use std::pin::Pin;

use common::{Seeded, as_of, seeded};
use ownhealth_core::access::all_scopes;
use ownhealth_genome::clinvar::{self, Before, EventText};
use ownhealth_rules::cited;
use ownhealth_rules::wording::{SKIP_KEYS, imperative_phrases, lint};
use ownhealth_rules::{build_state, check_drug, dashboard, load_pack, render};
use serde_json::{Value, json};

type R = anyhow::Result<()>;
type Out<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Vec<Value>>> + 'a>>;
type Source = for<'a> fn(&'a Seeded) -> Out<'a>;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data");
const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];
const LANGS: [&str; 2] = ["en", "es"];

/// Every user-facing output we compute. One line per source.
const SOURCES: &[(&str, Source)] = &[
    ("status", |s| Box::pin(status(s))),
    ("dashboard", |s| Box::pin(dashboards(s))),
    ("check_drug", |s| Box::pin(drug_checks(s))),
    ("summary", |s| Box::pin(summaries(s))),
    ("timeline", |_| {
        Box::pin(async { Ok(vec![ownhealth_api::routes::timeline::phrases()]) })
    }),
    ("clinvar_alerts", |_| Box::pin(clinvar_alerts())),
    ("due", |s| Box::pin(due(s))),
    ("calculators", |s| Box::pin(calculators(s))),
];

async fn status(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        for lang in LANGS {
            for mode in ["full", "pseudonymous", "minimal"] {
                out.push(render(&body, lang, "as_reported", mode, None)?);
            }
        }
    }
    Ok(out)
}

async fn dashboards(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        for lang in LANGS {
            out.push(dashboard(&mut tx, s.pid(slug), as_of(), lang, &all_scopes()).await?);
        }
    }
    Ok(out)
}

async fn drug_checks(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let mut drugs: Vec<String> = load_pack()?.drugs.keys().cloned().collect();
    drugs.sort();
    let mut out = Vec::new();
    for slug in ["alex", "sam"] {
        for drug in &drugs {
            for lang in LANGS {
                out.push(check_drug(&mut tx, s.pid(slug), drug, &all_scopes(), lang, Some(as_of())).await?);
            }
        }
    }
    Ok(out)
}

/// Every ClinVar alert and review sentence: each class change, star count, gate and chip flag.
async fn clinvar_alerts() -> anyhow::Result<Vec<Value>> {
    const CLASSES: [&str; 5] = ["plp", "vus", "conflicting", "blb", "other"];
    let befores: Vec<Option<Before>> = std::iter::once(None)
        .chain(
            CLASSES
                .iter()
                .flat_map(|&class| [None, Some(0), Some(1), Some(3)].map(|stars| Some(Before { class, stars }))),
        )
        .collect();
    let mut out = Vec::new();
    for lang in LANGS {
        for (withheld, unconfirmed) in [(false, false), (false, true), (true, false), (true, true)] {
            out.push(json!({"summary": clinvar::review_summary(withheld, unconfirmed, lang)}));
            for gene in [None, Some("BRCA2")] {
                for old in &befores {
                    for new_class in CLASSES {
                        for new_stars in [0, 1, 2] {
                            for not_in_previous_release in [false, true] {
                                let t = EventText {
                                    gene,
                                    old: old.as_ref(),
                                    new_class,
                                    new_stars,
                                    withheld,
                                    unconfirmed,
                                    not_in_previous_release,
                                };
                                out.push(json!({"summary": clinvar::summary(&t, lang)}));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The Due screen (ownhealth.prevention/v1 rendered) of each person in both languages.
async fn due(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let pack = load_pack()?;
    let mut out = Vec::new();
    for slug in SLUGS {
        let doc = ownhealth_rules::prevention::build_document(&mut tx, s.pid(slug), as_of()).await?;
        for lang in LANGS {
            out.push(ownhealth_rules::prevention::render(&doc, &pack, lang, &all_scopes())?);
        }
    }
    Ok(out)
}

/// The calculators (ownhealth.calculators/v1 rendered) of each person in both languages.
async fn calculators(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        let doc = ownhealth_rules::calc::document(&mut tx, s.pid(slug), as_of()).await?;
        for lang in LANGS {
            out.push(ownhealth_rules::calc::render(&doc, lang)?);
        }
    }
    Ok(out)
}

#[tokio::test]
async fn every_output_source_is_information_not_advice() -> R {
    let s = seeded().await?;
    let mut hits = Vec::new();
    for (name, source) in SOURCES {
        let outputs = source(&s).await?;
        assert!(!outputs.is_empty(), "{name} produced nothing to lint");
        for out in &outputs {
            hits.extend(
                lint(out, &SKIP_KEYS)
                    .into_iter()
                    .map(|(p, h)| format!("{name} {p}: {h}")),
            );
        }
    }
    assert_eq!(hits, Vec::<String>::new());
    Ok(())
}

/// The 0 above is not a lint that cannot see: the drug check's CPIC quotes, once stripped of their
/// attribution, are imperatives and fail.
#[tokio::test]
async fn the_gate_goes_red_on_an_unattributed_quote() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let mut out = check_drug(&mut tx, s.pid("sam"), "codeine", &all_scopes(), "en", Some(as_of())).await?;
    assert_eq!(lint(&out, &SKIP_KEYS), Vec::new());
    for g in out["guidance"].as_array_mut().into_iter().flatten() {
        if let Some(m) = g.as_object_mut() {
            for k in ["source", "url", "cite"] {
                m.shift_remove(k);
            }
        }
    }
    assert_ne!(lint(&out, &SKIP_KEYS), Vec::new());
    Ok(())
}

#[test]
fn spanish_infinitive_instructions_are_imperatives() {
    for s in [
        "Evitar codeína.",
        "Suspender la estatina.",
        "Tomar con comida.",
        "Dejar de fumar.",
        "No tomar alcohol.",
        "Reducir la sal.",
        "Aumentar la dosis.",
        "Dosis: evitar el alcohol.",
    ] {
        assert!(!imperative_phrases(s).is_empty(), "{s}");
    }
    for ok in ["La dosis se tomó con comida.", "Valor reducido respecto al anterior."] {
        assert_eq!(imperative_phrases(ok), Vec::<String>::new(), "{ok}");
    }
}

#[test]
fn a_quote_is_exempt_only_with_its_attribution() {
    let quote = "Avoid codeine.";
    assert_ne!(lint(&json!({"quote": quote}), &SKIP_KEYS), Vec::new());
    assert_ne!(lint(&json!({"quote": quote, "source": " "}), &SKIP_KEYS), Vec::new());
    assert_ne!(lint(&json!({"quote": quote, "url": null}), &SKIP_KEYS), Vec::new());
    for attribution in ["cite", "url", "source"] {
        let v = json!({"quote": quote, attribution: "https://cpicpgx.org/guidelines/"});
        assert_eq!(lint(&v, &SKIP_KEYS), Vec::new(), "{attribution}");
    }
}

/// The fixed notices shown beside genome and record text are user-facing too.
#[test]
fn the_fixed_notices_are_information_not_advice() {
    for notice in [ownhealth_genome::NOTICE, ownhealth_core::text::NOTICE] {
        assert_eq!(imperative_phrases(notice), Vec::<String>::new(), "{notice}");
    }
}

/// Every es/en string in every YAML or JSON file under `data/`, so a new data file or directory is
/// linted the day it lands.
#[test]
fn every_es_en_string_under_data_is_information_not_advice() -> R {
    let (mut files, mut texts, mut hits) = (0, 0, Vec::new());
    let mut dirs = vec![std::path::PathBuf::from(DATA)];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
            if !["yaml", "yml", "json"].contains(&ext) {
                continue;
            }
            files += 1;
            for (at, text) in cited::localized(&cited::read_doc(&path)?) {
                texts += 1;
                hits.extend(
                    imperative_phrases(&text)
                        .into_iter()
                        .map(|h| format!("{} {at}: {h}", path.display())),
                );
            }
        }
    }
    assert_eq!(hits, Vec::<String>::new());
    // A sanity floor with headroom (20 files and 302 texts today): it proves the walk read data/,
    // and a later slice that trims or merges data files does not turn the gate red.
    assert!(files >= 10 && texts >= 150, "{files} files, {texts} texts");
    Ok(())
}

fn cited_file(body: &str) -> anyhow::Result<(tempfile::TempDir, std::path::PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("example.yaml");
    std::fs::write(&path, body)?;
    Ok((dir, path))
}

const GOOD: &str = r#"
version: "example 0.1.0"
rows:
  - id: example.one
    cite: https://www.ncbi.nlm.nih.gov/books/NBK000000/
templates:
  example.one: {en: "Last measured {when}.", es: "Última medida {when}."}
"#;

#[test]
fn a_cited_file_loads_with_its_version_hash_and_rows() -> R {
    let (_dir, path) = cited_file(GOOD)?;
    let f = cited::load(&path)?;
    assert_eq!(f.version, "example 0.1.0");
    assert_eq!(f.sha256.len(), 64);
    assert_eq!(f.rows.len(), 1);
    assert_eq!(f.doc["templates"]["example.one"]["es"], "Última medida {when}.");
    Ok(())
}

#[test]
fn the_cited_loader_refuses_a_row_or_template_that_breaks_the_rules() -> R {
    let cite = "https://www.ncbi.nlm.nih.gov/books/NBK000000/";
    for (what, body, reason) in [
        (
            "http cite",
            GOOD.replace("https://www", "http://www"),
            "no https `cite`",
        ),
        (
            "no cite",
            GOOD.replace(&format!("    cite: {cite}\n"), ""),
            "no https `cite`",
        ),
        ("bare https", GOOD.replace(cite, "https://"), "no https `cite`"),
        (
            "missing es template",
            GOOD.replace(r#", es: "Última medida {when}.""#, ""),
            "no `es` text",
        ),
        (
            "empty en template",
            GOOD.replace(r#"en: "Last measured {when}.""#, r#"en: """#),
            "no `en` text",
        ),
        (
            "imperative template",
            GOOD.replace("Última medida {when}.", "Evitar codeína."),
            "reads as advice",
        ),
        (
            "no version",
            GOOD.replace(r#"version: "example 0.1.0""#, ""),
            "no `version`",
        ),
        (
            "rows not a list",
            GOOD.replace(&format!("  - id: example.one\n    cite: {cite}"), "  id: example.one"),
            "not a list",
        ),
    ] {
        assert_ne!(body, GOOD, "{what}: the mutation did not apply");
        let (_dir, path) = cited_file(&body)?;
        match cited::load(&path) {
            Ok(_) => panic!("{what} loaded"),
            Err(e) => assert!(e.to_string().contains(reason), "{what}: {e}"),
        }
    }
    Ok(())
}

/// The printed doctor summary's model, every profile, specialty and language (its PDF text layer is
/// linted in `summary_pdf.rs`).
async fn summaries(s: &Seeded) -> anyhow::Result<Vec<Value>> {
    use ownhealth_rules::summary::{LANGS, Options, SPECIALTIES, build};
    let mut tx = s.pool().begin().await?;
    let mut out = Vec::new();
    for slug in SLUGS {
        for specialty in SPECIALTIES {
            for lang in LANGS {
                let o = Options {
                    lang: lang.into(),
                    specialty: specialty.into(),
                    sensitive_open: false,
                    as_of: as_of(),
                };
                out.push(build(&mut tx, s.pid(slug), &all_scopes(), &o).await?);
            }
        }
    }
    Ok(out)
}

/// The ClinVar alert wording is a cited data file too; the genome crate reads it at build time,
/// below ownhealth-rules, so this is where it passes the cited loader's checks.
#[test]
fn the_clinvar_alert_file_passes_the_cited_loader() -> R {
    let f = cited::load(&std::path::PathBuf::from(DATA).join("genome/clinvar_alerts.json"))?;
    assert!(f.version.starts_with("clinvar_alerts "), "{}", f.version);
    assert!(f.doc["cite"].as_str().is_some_and(|c| c.starts_with("https://")));
    Ok(())
}
