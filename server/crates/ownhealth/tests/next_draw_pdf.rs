//! The printed next-draw request (`ownhealth_rules::next_draw` + `ownhealth_ops::next_draw_pdf`), read
//! back the way a reader would (PDFium): Alex and Carmen in es and en, with and without their name.
//! Every test line names its rule and src ids, the text layer is information and not advice, two
//! renders give the same bytes, and the footer carries the rules versions.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use common::{as_of, seeded};
use ownhealth_core::access::get_profile;
use ownhealth_ingest_pdf::textcheck::assert_text_layer;
use ownhealth_ops::next_draw_pdf::render;
use ownhealth_rules::load_pack;
use ownhealth_rules::next_draw;
use ownhealth_rules::wording::imperative_phrases;
use serde_json::{Value, json};

type R = anyhow::Result<()>;

/// Every table row starts with the test's name and names the rule of its first reason, and that
/// reason's src ids, on the same line. Every line is information.
fn src_check(model: &Value) -> impl FnMut(&str) -> Result<(), String> + use<> {
    let mut rows: Vec<(String, Vec<String>)> = Vec::new();
    for sec in model["sections"].as_array().into_iter().flatten() {
        for it in sec["items"].as_array().into_iter().flatten() {
            let first = &it["reasons"][0];
            let mut ids = vec![first["rule"].as_str().unwrap_or("rule_missing").to_string()];
            ids.extend(
                first["src"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string),
            );
            rows.push((format!("{} ", it["name"].as_str().unwrap_or_default()), ids));
        }
    }
    move |line: &str| {
        for (start, ids) in &rows {
            if line.starts_with(start.as_str()) && ids.iter().any(|id| !line.contains(id.as_str())) {
                return Err(format!("a test line without its rule or src ids ({ids:?})"));
            }
        }
        if let Some(hit) = imperative_phrases(line).first() {
            return Err(format!("reads as advice: {hit}"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn alex_and_carmen_print_in_both_languages_with_and_without_their_name() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pack = load_pack()?;
    for slug in ["alex", "carmen"] {
        let profile = get_profile(&mut tx, s.pid(slug))
            .await?
            .ok_or_else(|| anyhow::anyhow!("no {slug}"))?;
        let plan = next_draw::build(&mut tx, s.pid(slug), as_of()).await?;
        for lang in ["en", "es"] {
            for hide in [false, true] {
                let what = format!("{slug} {lang} hide={hide}");
                let mut model = next_draw::render(&plan, &pack, lang)?;
                model["person"] = next_draw::person(&profile, as_of(), lang, hide)?;
                let sheet = render(&model);
                assert_eq!(render(&model).pdf, sheet.pdf, "{what}: two renders differ");
                let lines = assert_text_layer(&sheet.pdf, src_check(&model));
                let text = lines.join("\n");
                assert_eq!(
                    text.contains(&profile.display_name),
                    !hide,
                    "{what}: the name shows or hides wrongly:\n{text}"
                );
                if hide {
                    let born = profile.birth_date.map(|b| b.to_string()).unwrap_or_default();
                    assert!(!text.contains(&born), "{what}: the birth date prints");
                }
                for rule in plan["rules"].as_array().into_iter().flatten() {
                    let rule = rule.as_str().unwrap_or_default();
                    assert!(text.contains(rule), "{what}: the footer has no {rule}:\n{text}");
                }
                assert!(
                    lines.iter().any(|l| l.ends_with(" 1/1")),
                    "{what}: no page 1/1:\n{text}"
                );
                // every analyte prints, and every cite it rests on is in the sources list
                for sec in model["sections"].as_array().into_iter().flatten() {
                    for note in sec["notes"].as_array().into_iter().flatten() {
                        let cite = note["cite"].as_str().unwrap_or_default();
                        assert!(
                            lines.iter().any(|l| l.contains(cite)),
                            "{what}: {cite} is not on one line:\n{text}"
                        );
                    }
                    for it in sec["items"].as_array().into_iter().flatten() {
                        let loinc = it["loinc"].as_str().unwrap_or_default();
                        assert!(text.contains(loinc), "{what}: no LOINC {loinc}");
                        for r in it["reasons"].as_array().into_iter().flatten() {
                            let cite = r["cite"].as_str().unwrap_or_default();
                            assert!(
                                lines.iter().any(|l| l.contains(cite)),
                                "{what}: {cite} is not on one line"
                            );
                        }
                    }
                }
                assert_eq!(sheet.printed.is_empty(), plan["count"] == 0);
            }
        }
        // the src ids the sheet prints are the ones the plan cites
        let model = next_draw::render(&plan, &pack, "en")?;
        let sheet = render(&model);
        let mut want: Vec<String> = Vec::new();
        for sec in plan["sections"].as_array().into_iter().flatten() {
            for it in sec["items"].as_array().into_iter().flatten() {
                for r in it["reasons"].as_array().into_iter().flatten() {
                    for id in r["src"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                        if !want.iter().any(|w| w == id) {
                            want.push(id.to_string());
                        }
                    }
                }
            }
        }
        assert_eq!(sheet.printed, want, "{slug}");
    }
    tx.rollback().await?;
    Ok(())
}

#[test]
fn a_person_with_nothing_to_list_gets_a_sheet_that_says_so() -> R {
    let pack = load_pack()?;
    let body = json!({"profile": "empty", "as_of": "2026-11-07", "state_hash": "sha256:00", "flags": []});
    let prevention = json!({"items": [], "rules": "es.yaml prevention-es 0.2.0", "prevention_hash": "sha256:11"});
    let plan = next_draw::plan(&body, &prevention, &pack, next_draw::load()?);
    for lang in ["en", "es"] {
        let model = next_draw::render(&plan, &pack, lang)?;
        let lines = assert_text_layer(&render(&model).pdf, src_check(&model));
        let empty = model["labels"]["empty"].as_str().unwrap_or("no label");
        assert!(lines.join(" ").contains(empty), "{lang}: {lines:#?}");
    }
    Ok(())
}
