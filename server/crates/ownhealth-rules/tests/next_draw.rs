//! The next-draw planner (`ownhealth_rules::next_draw`, `data/prevention/draw_prep.yaml`): the goldens
//! of Alex and Carmen at the docs' date, one line per analyte with every reason it is listed for, the
//! preparation from the cited rows, and words in es and en for every line.
//!
//! Re-record the golden with `OWNHEALTH_RECORD_NEXT_DRAW=1 cargo test -p ownhealth-rules --test next_draw`.

mod common;

use std::path::Path;

use common::{as_of, seeded};
use ownhealth_rules::next_draw::{self, DrawPrep};
use ownhealth_rules::{cited, load_pack};
use serde_json::{Value, json};

type R = anyhow::Result<()>;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/prevention/draw_prep.yaml");
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/next_draw.json");

fn items(plan: &Value) -> Vec<&Value> {
    plan["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|s| s["items"].as_array().into_iter().flatten())
        .collect()
}

fn groups(plan: &Value) -> Vec<&str> {
    items(plan).iter().filter_map(|i| i["group"].as_str()).collect()
}

fn kinds(item: &Value) -> Vec<&str> {
    item["reasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["kind"].as_str())
        .collect()
}

fn prep() -> anyhow::Result<&'static DrawPrep> {
    Ok(next_draw::load()?)
}

#[test]
fn the_data_file_loads_through_the_cited_loader_and_names_pack_analytes() -> R {
    let f = cited::load(Path::new(DATA))?;
    assert_eq!(f.version, prep()?.version);
    let pack = load_pack()?;
    let ids: Vec<&str> = prep()?.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["fasting", "morning_urine", "biotin"]);
    for row in &prep()?.rows {
        assert!(row.cite.starts_with("https://"), "{}", row.id);
        for g in &row.groups {
            assert!(pack.analytes.contains_key(g), "{}: {g} is not a pack analyte", row.id);
        }
    }
    // a row naming a group the pack does not know is refused
    let bad = std::fs::read_to_string(DATA)?.replace("groups: [glucose]", "groups: [glucoze]");
    let err = next_draw::parse(bad.as_bytes(), &pack).err().map(|e| e.to_string());
    assert!(err.as_deref().is_some_and(|e| e.contains("glucoze")), "{err:?}");
    Ok(())
}

#[tokio::test]
async fn goldens_for_alex_and_carmen() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let mut got = serde_json::Map::new();
    for slug in ["alex", "carmen"] {
        got.insert(slug.into(), next_draw::build(&mut tx, s.pid(slug), as_of()).await?);
    }
    let got = Value::Object(got);
    if std::env::var_os("OWNHEALTH_RECORD_NEXT_DRAW").is_some() {
        std::fs::write(GOLDEN, serde_json::to_string_pretty(&got)? + "\n")?;
    }
    let want: Value = serde_json::from_str(&std::fs::read_to_string(GOLDEN)?)?;
    for slug in ["alex", "carmen"] {
        assert_eq!(got[slug], want[slug], "{slug}");
    }
    // the analytes, grouped by preparation: early-morning urine, then biotin, then the rest
    let alex = &got["alex"];
    assert_eq!(
        groups(alex),
        ["urine_acr", "ft4", "tsh", "hcv_ab", "lpa", "vitamin_d_25oh"]
    );
    let preps: Vec<&Value> = alex["sections"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| &s["prep"])
        .collect();
    assert_eq!(preps, [&json!(["morning_urine"]), &json!(["biotin"]), &json!([])]);
    let tsh = items(alex)
        .into_iter()
        .find(|i| i["group"] == "tsh")
        .cloned()
        .unwrap_or_default();
    assert_eq!(tsh["loinc"], "3016-3");
    assert_eq!(tsh["reasons"][0]["rule"], "recency.thyroid");
    assert_eq!(tsh["reasons"][0]["src"], json!(["src_a05"]));
    assert_eq!(tsh["reasons"][0]["last"], "2025-01-20");
    let lpa = items(alex)
        .into_iter()
        .find(|i| i["group"] == "lpa")
        .cloned()
        .unwrap_or_default();
    assert_eq!(kinds(&lpa), ["lab_range"]);
    assert_eq!(lpa["reasons"][0]["src"], json!(["src_a07"]));
    let carmen = &got["carmen"];
    assert_eq!(groups(carmen), ["hcv_ab", "lpa", "hba1c", "egfr"]);
    let hcv = items(carmen)
        .into_iter()
        .find(|i| i["group"] == "hcv_ab")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        hcv["reasons"][0]["cite"],
        "https://www.cdc.gov/mmwr/volumes/69/rr/rr6902a1.htm"
    );
    assert_eq!(hcv["loinc"], "16128-1");
    // the plan holds codes and ids only: the state hash and the prevention hash it was made from
    for plan in [alex, carmen] {
        assert!(plan["state_hash"].as_str().is_some_and(|h| h.starts_with("sha256:")));
        assert!(
            plan["prevention_hash"]
                .as_str()
                .is_some_and(|h| h.starts_with("sha256:"))
        );
        assert_eq!(plan["schema"], next_draw::SCHEMA);
    }
    tx.rollback().await?;
    Ok(())
}

/// A body with TSH both stale and last out of range, and a prevention document listing the stale
/// series: TSH prints once, with both reasons, each with its rule, cite and src.
fn stale_and_flagged() -> (Value, Value) {
    let body = json!({
        "schema": "ownhealth.state/v1", "profile": "test", "as_of": "2026-11-07",
        "state_hash": "sha256:00",
        "flags": [
            {"reason": "LAB_HIGH", "rule": "range.lab", "value": "6.1", "unit": "mIU/L", "ref": "0.4 - 4.0",
             "flag": "H", "date": "2025-01-20", "src": "src_tsh1", "group": "tsh"},
            {"reason": "RCV_CHANGE", "rule": "rcv.crp_hs", "value": "2.4", "prev": "1.1", "unit": "mg/L",
             "date": "2026-09-14", "src": ["src_c1", "src_c2"], "group": "crp_hs"},
        ],
        "stale": [{"group": "tsh", "last": "2025-01-20", "rule": "recency.thyroid", "src": "src_tsh1"}],
        "never_measured": [],
    });
    let prevention = json!({
        "schema": "ownhealth.prevention/v1", "profile": "test", "as_of": "2026-11-07",
        "rules": "es.yaml prevention-es 0.2.0", "prevention_hash": "sha256:11",
        "items": [
            {"key": "stale:recency.thyroid:tsh", "rule": "recency.thyroid", "area": "lab", "kind": "recency",
             "status": "overdue", "group": "tsh", "due": "2026-01-20",
             "last": {"date": "2025-01-20", "src": "src_tsh1"}, "cite": "https://example.org/thyroid",
             "src": ["src_tsh1"], "labs": true},
            {"key": "es.cancer.colorectal", "rule": "es.cancer.colorectal", "area": "screening",
             "kind": "every_n_months", "status": "due", "cite": "https://example.org/crc", "src": []},
        ],
    });
    (body, prevention)
}

#[test]
fn an_analyte_both_stale_and_flagged_is_listed_once_with_both_reasons() -> R {
    let pack = load_pack()?;
    let (body, prevention) = stale_and_flagged();
    let plan = next_draw::plan(&body, &prevention, &pack, prep()?);
    // the RCV change and the screening row are not draw items
    assert_eq!(groups(&plan), ["tsh"], "{plan:#}");
    let tsh = items(&plan)[0];
    assert_eq!(kinds(tsh), ["recency", "lab_range"]);
    assert_eq!(tsh["reasons"][0]["cite"], "https://example.org/thyroid");
    assert_eq!(tsh["reasons"][1]["rule"], "range.lab");
    assert_eq!(tsh["reasons"][1]["src"], json!(["src_tsh1"]));
    assert_eq!(tsh["prep"], json!(["biotin"]));
    let en = next_draw::render(&plan, &pack, "en")?;
    let texts: Vec<&str> = items(&en)[0]["reasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["text"].as_str())
        .collect();
    assert_eq!(
        texts,
        [
            "Last measured on 2025-01-20, longer ago than the rule's interval of 12 months.",
            "Last value 6.1 mIU/L on 2025-01-20, outside the lab range 0.4 - 4.0.",
        ]
    );
    Ok(())
}

/// A lab gap the person snoozed or dismissed sits in the document's `hidden` list: it stays off the
/// request, and so does every other kind of item.
#[test]
fn a_hidden_lab_gap_stays_off_the_request() -> R {
    let pack = load_pack()?;
    let (mut body, mut prevention) = stale_and_flagged();
    body["flags"] = json!([]);
    let gap = prevention["items"][0].clone();
    prevention["items"] = json!([]);
    prevention["hidden"] = json!([gap]);
    let plan = next_draw::plan(&body, &prevention, &pack, prep()?);
    assert!(groups(&plan).is_empty(), "{plan:#}");
    assert_eq!(plan["count"], 0);
    Ok(())
}

#[tokio::test]
async fn every_line_has_words_in_both_languages_and_names_its_rule() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pack = load_pack()?;
    for slug in ["alex", "carmen"] {
        let plan = next_draw::build(&mut tx, s.pid(slug), as_of()).await?;
        for lang in ["en", "es"] {
            let out = next_draw::render(&plan, &pack, lang)?;
            for sec in out["sections"].as_array().into_iter().flatten() {
                assert!(sec["title"].as_str().is_some_and(|t| !t.is_empty()), "{slug} {lang}");
                for note in sec["notes"].as_array().into_iter().flatten() {
                    assert!(note["text"].as_str().is_some_and(|t| !t.is_empty()));
                    assert!(note["cite"].as_str().is_some_and(|c| c.starts_with("https://")));
                }
            }
            for it in items(&out) {
                assert!(it["name"].as_str().is_some_and(|t| !t.is_empty()), "{slug} {lang} {it}");
                for r in it["reasons"].as_array().into_iter().flatten() {
                    assert!(r["rule"].as_str().is_some_and(|t| !t.is_empty()), "{r}");
                    assert!(r["cite"].as_str().is_some_and(|c| c.starts_with("https://")), "{r}");
                    let text = r["text"].as_str().unwrap_or_default();
                    assert!(!text.is_empty() && !text.contains('{'), "{slug} {lang}: {text}");
                }
            }
        }
        let es = next_draw::render(&plan, &pack, "es")?;
        assert_eq!(es["labels"]["title"], "Pruebas para la próxima analítica");
    }
    let es = next_draw::render(&next_draw::build(&mut tx, s.pid("alex"), as_of()).await?, &pack, "es")?;
    let urine = items(&es)[0];
    assert_eq!(urine["name"], "Cociente alb/creat");
    assert_eq!(
        urine["reasons"][0]["text"],
        "Última medición el 2024-05-22, hace más que el intervalo de 12 meses de la regla."
    );
    tx.rollback().await?;
    Ok(())
}
