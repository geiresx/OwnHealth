//! Rendering happens after hashing (0.1.1 tests/state/test_render.py, test_render_units.py):
//! language, units, identity mode and scopes change the text, never the state_hash.

mod common;

use std::collections::BTreeSet;

use common::{as_of, seeded};
use ownhealth_core::access::{all_scopes, scope_set};
use ownhealth_core::text::is_untrusted;
use ownhealth_rules::pack::{build_pack, shipped_files};
use ownhealth_rules::rendering::{convert, render_with};
use ownhealth_rules::{build_state, render, state_hash};
use rust_decimal::Decimal;
use serde_json::{Value, json};

type R = anyhow::Result<()>;

#[tokio::test]
async fn same_hash_for_every_language_unit_and_identity_setting() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let body = build_state(&mut tx, s.pid("alex"), as_of(), None).await?;
    let mut hashes = BTreeSet::new();
    let mut versions = BTreeSet::new();
    for lang in ["en", "es"] {
        for units in ["as_reported", "si", "conventional"] {
            for mode in ["full", "pseudonymous", "minimal"] {
                let out = render(&body, lang, units, mode, None)?;
                hashes.insert(out["state_hash"].to_string());
                versions.insert(out["rules_version"].to_string());
            }
        }
    }
    assert_eq!(hashes, BTreeSet::from([body["state_hash"].to_string()]));
    assert_eq!(versions, BTreeSet::from([json!("ownhealth-rules 0.1.0").to_string()]));
    let text = |lang| -> anyhow::Result<Value> {
        Ok(render(&body, lang, "as_reported", "pseudonymous", None)?["data"]["flags"][0]["text"].clone())
    };
    assert_eq!(text("en")?, "Lp(a) 142 nmol/L (H, lab range < 75) on 2025-09-08.");
    assert_eq!(
        text("es")?,
        "Lp(a) 142 nmol/L (H, rango del laboratorio < 75) el 2025-09-08."
    );
    // rendering did not touch the body
    assert_eq!(body["state_hash"].as_str(), Some(state_hash(&body)?.as_str()));
    Ok(())
}

#[tokio::test]
async fn rendered_flags_follow_mcp_5_1() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = render(
        &build_state(&mut tx, s.pid("alex"), as_of(), None).await?,
        "en",
        "as_reported",
        "pseudonymous",
        None,
    )?;
    let f = &out["data"]["flags"];
    assert_eq!(
        (
            &f[0]["rank"],
            &f[0]["kind"],
            &f[0]["code"],
            &f[0]["flag"],
            &f[0]["range"],
            &f[0]["src"]
        ),
        (
            &json!(1),
            &json!("out_of_range"),
            &json!("LOINC:43583-4"),
            &json!("H"),
            &json!("< 75"),
            &json!("src_a07")
        )
    );
    assert_eq!(
        (&f[1]["code"], &f[1]["flag"], &f[1]["src"]),
        (&json!("LOINC:62292-8"), &json!("L"), &json!("src_a12"))
    );
    assert_eq!(
        (&f[2]["kind"], &f[2]["drug"], &f[2]["gene"], &f[2]["src"]),
        (
            &json!("gene_drug"),
            &json!("Atorvastatin"),
            &json!("SLCO1B1"),
            &json!(["src_g31", "src_m02"])
        )
    );
    let t3 = f[2]["text"].as_str().unwrap_or_default();
    assert!(!t3.contains("Discuss with your prescriber") && t3.contains("To discuss with your prescriber"));
    assert_eq!(out["data"]["stale"][0]["window_months"], 12);
    assert_eq!(
        out["data"]["never_measured"][0],
        json!({"name": "Hepatitis C antibody", "rule": "infect.hcv.once_adult",
               "text": "Hepatitis C antibody: no result on record (rule infect.hcv.once_adult)."})
    );
    assert_eq!(out["notice"], "Information from your records, not a diagnosis.");
    assert_eq!(out["limitations"][0], "Flags use each lab's own printed range.");
    Ok(())
}

#[tokio::test]
async fn free_text_goes_out_wrapped() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = render(
        &build_state(&mut tx, s.pid("alex"), as_of(), None).await?,
        "en",
        "as_reported",
        "pseudonymous",
        None,
    )?;
    let note = &out["data"]["notes"][0]["text"];
    assert!(is_untrusted(note) && note["origin"] == "person_note");
    assert_eq!(note["text"], "Recheck vitamin D in winter.");
    Ok(())
}

#[tokio::test]
async fn minimal_identity_mode_drops_free_text_and_makes_dates_relative() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = render(
        &build_state(&mut tx, s.pid("alex"), as_of(), None).await?,
        "en",
        "as_reported",
        "minimal",
        None,
    )?;
    assert_eq!(out["data"]["notes"], json!([]));
    assert_eq!(out["data"]["flags"][0]["date"], "425 days ago");
    assert_eq!(
        out["data"]["flags"][0]["text"],
        "Lp(a) 142 nmol/L (H, lab range < 75) 425 days ago."
    );
    assert_eq!(out["identity_mode"], "minimal");
    Ok(())
}

#[tokio::test]
async fn scopes_leave_sections_out_and_name_them() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let body = build_state(&mut tx, s.pid("alex"), as_of(), None).await?;
    let summary_only = render(
        &body,
        "en",
        "as_reported",
        "pseudonymous",
        Some(&scope_set(["read:summary"])),
    )?;
    assert_eq!(
        (&summary_only["data"]["flags"], &summary_only["data"]["medications"]),
        (&json!([]), &json!([]))
    );
    assert_eq!(
        summary_only["withheld"]["by_scope"],
        json!(["genome", "labs", "records", "wearables"])
    );
    let mut scopes = all_scopes();
    scopes.remove("read:genome");
    let no_genome = render(&body, "en", "as_reported", "pseudonymous", Some(&scopes))?;
    let flags = no_genome["data"]["flags"].as_array().cloned().unwrap_or_default();
    assert!(!flags.iter().any(|f| f["kind"] == "gene_drug"));
    assert_eq!(no_genome["withheld"]["by_scope"], json!(["genome"]));
    assert!(
        flags.len() == 4
            && no_genome["data"]["medications"]
                .as_array()
                .is_some_and(|m| !m.is_empty())
    );
    let full = render(&body, "en", "as_reported", "pseudonymous", Some(&all_scopes()))?;
    assert_eq!(full["withheld"]["by_scope"], json!([]));
    assert_eq!(full["data"]["flags"].as_array().map(Vec::len), Some(5));
    assert!(summary_only["state_hash"] == no_genome["state_hash"] && no_genome["state_hash"] == full["state_hash"]);
    Ok(())
}

#[tokio::test]
async fn a_pack_defined_lab_reason_renders() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let mut body = build_state(&mut tx, s.pid("alex"), as_of(), None).await?;
    let mut files = shipped_files()?;
    if let Some(t) = files
        .get_mut("templates.yaml")
        .and_then(|d| d.get_mut("templates"))
        .and_then(Value::as_object_mut)
    {
        t.insert(
            "LAB_CRITICAL_HIGH".into(),
            json!({"en": "{name} {value} {unit} (critical, lab range {ref}) {when}.",
                   "es": "{name} {value} {unit} (crítico, rango del laboratorio {ref}) {when}."}),
        );
    }
    let pack = build_pack(&files, true)?;
    let first = body["flags"]
        .as_array()
        .and_then(|f| f.iter().position(|f| f["rule"] == "range.lab"))
        .unwrap_or(0);
    body["flags"][first]["reason"] = json!("LAB_CRITICAL_HIGH");
    let out = render_with(&pack, &body, "en", "as_reported", "pseudonymous", None)?;
    let flag = &out["data"]["flags"][0];
    assert!(
        flag["kind"] == "out_of_range"
            && flag["text"]
                .as_str()
                .is_some_and(|t| t.contains("(critical, lab range < 75)"))
    );
    Ok(())
}

/// get_health_status honours the token's unit preference: si and conventional, both ways.
#[test]
fn si_and_conventional_targets() {
    let close = |v: &Value, want: &str| {
        let got = v["value"]
            .as_str()
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or_default();
        (got - want.parse::<Decimal>().unwrap_or_default()).abs() < Decimal::new(1, 2)
    };
    let si = convert(Some("glucose"), Some("92"), Some("mg/dL"), "si").unwrap_or_default();
    assert!(si["unit"] == "mmol/L" && close(&si, "5.106"), "{si}");
    let conv = convert(Some("glucose"), Some("5.3"), Some("mmol/L"), "conventional").unwrap_or_default();
    assert!(conv["unit"] == "mg/dL" && close(&conv, "95.48"), "{conv}");
    assert_eq!(convert(Some("glucose"), Some("5.3"), Some("mmol/L"), "si"), None); // already in the preferred unit
    assert_eq!(
        convert(Some("glucose"), Some("5.3"), Some("mmol/L"), "as_reported"),
        None
    );
}
