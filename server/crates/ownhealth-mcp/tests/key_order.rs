//! The agent reads the text block, so its keys keep 0.1.1's order: Python's `dict.pop` and
//! `{**data, "points": ...}` leave every other key where it was. serde_json's `Map::remove` under
//! `preserve_order` is a swap_remove, which moved the last key into the removed slot.

mod common;

use common::*;
use serde_json::{Value, json};

/// The keys of `data` that are in `expected`, in the order the answer has them.
fn order_of(data: &Value, expected: &[&str]) -> Vec<String> {
    data.as_object()
        .map(|m| m.keys().filter(|k| expected.contains(&k.as_str())).cloned().collect())
        .unwrap_or_default()
}

/// The keys of `expected` that `data` holds, in `expected`'s order.
fn wanted(data: &Value, expected: &[&str]) -> Vec<String> {
    expected
        .iter()
        .filter(|k| data.get(**k).is_some())
        .map(|k| k.to_string())
        .collect()
}

/// The positions of `keys` as top-level keys of the text block (`key:` at a line start) rise.
fn text_in_order(text: &str, keys: &[&str]) -> bool {
    let at: Vec<Option<usize>> = keys.iter().map(|k| text.find(&format!("\n{k}:"))).collect();
    at.iter().all(Option::is_some) && at.windows(2).all(|w| w[0] < w[1])
}

#[tokio::test]
async fn get_trend_keeps_points_where_python_has_them() -> R {
    let w = world().await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let r = c
        .call(
            "get_trend",
            json!({"profile": "alex", "code": "13457-7", "window": "all"}),
        )
        .await?;
    let data = r.ok()["data"].clone();
    let py = [
        "name",
        "codes",
        "canonical_unit",
        "resolution",
        "coverage_pct",
        "points",
        "stats",
        "annotations",
    ];
    assert_eq!(order_of(&data, &py), wanted(&data, &py), "{data}");
    assert!(
        text_in_order(&r.text, &["points", "stats", "annotations"]),
        "{}",
        r.text
    );
    Ok(())
}

#[tokio::test]
async fn get_health_status_drops_a_section_without_moving_the_rest() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok_scopes(without(&["read:records"])).await?).await?;
    let r = c.call("get_health_status", json!({"profile": "alex"})).await?;
    let data = r.ok()["data"].clone();
    // the fake renderer's order, which the tool must keep
    let fake = [
        "flags",
        "medications",
        "never_measured",
        "stale",
        "notes",
        "data_freshness",
        "state_hash",
        "rules_version",
    ];
    assert!(data.get("medications").is_none());
    assert_eq!(order_of(&data, &fake), wanted(&data, &fake), "{data}");
    assert!(
        text_in_order(&r.text, &["notes", "state_hash", "rules_version"]),
        "{}",
        r.text
    );
    Ok(())
}

#[tokio::test]
async fn check_drug_drops_a_part_without_moving_the_rest() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok_scopes(without(&["read:genome"])).await?).await?;
    let r = c
        .call("check_drug", json!({"profile": "alex", "drug": "atorvastatin"}))
        .await?;
    let data = r.ok()["data"].clone();
    let fake = [
        "drug",
        "pgx",
        "phenoconversion",
        "current_medications_relevant",
        "relevant_labs",
        "guidance",
    ];
    assert!(data.get("pgx").is_none() && data.get("guidance").is_none());
    assert_eq!(order_of(&data, &fake), wanted(&data, &fake), "{data}");
    assert!(
        text_in_order(&r.text, &["drug", "current_medications_relevant", "relevant_labs"]),
        "{}",
        r.text
    );
    Ok(())
}
