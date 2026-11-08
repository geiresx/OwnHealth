//! `tests/mcp/test_mcp_identity.py`: per-token identity mode, language and units are applied after
//! state_hash (mcp.md 4, 9.6).

mod common;

use std::collections::BTreeSet;

use chrono::NaiveDate;
use common::*;
use ownhealth_core::provenance::{SrcRef, ensure_src};
use ownhealth_mcp::redact;
use serde_json::json;

#[tokio::test]
async fn same_state_hash_for_every_language_unit_and_identity_mode() -> R {
    let w = world().await?;
    let (mut hashes, mut texts) = (BTreeSet::new(), BTreeSet::new());
    let today = today().format("%Y-%m-%d").to_string();
    for (lang, units, mode) in [
        ("en", "si", "full"),
        ("es", "as_reported", "pseudonymous"),
        ("es", "conventional", "minimal"),
    ] {
        let tok = w
            .token(Tok {
                lang,
                units,
                identity_mode: mode,
                ..Tok::default()
            })
            .await?;
        let env = w
            .connect(&tok)
            .await?
            .call_tool("get_health_status", json!({"profile": "alex", "as_of": today}))
            .await?
            .ok();
        assert_eq!(env["lang"], lang);
        assert_eq!(env["identity_mode"], mode);
        hashes.insert(env["data"]["state_hash"].to_string());
        texts.insert(env["data"]["flags"][0]["text"].to_string());
    }
    assert_eq!(hashes.len(), 1);
    assert_eq!(texts.len(), 2);
    Ok(())
}

#[tokio::test]
async fn pseudonymous_and_minimal_redaction() -> R {
    let w = world().await?;
    let body = "Alex Rivera asked: call me at +34 600 123 456 or alex.rivera@example.com, DNI 12345678Z.";
    let mut c = w.conn().await?;
    let sid = ensure_src(&mut c, &SrcRef::New(manual(w.pid("alex"))), Some(w.pid("alex"))).await?;
    sqlx::query(
        "insert into note (profile_id, kind, author_kind, body, src_id) values ($1, 'journal', 'person', $2, $3)",
    )
    .bind(w.pid("alex"))
    .bind(body)
    .bind(&sid)
    .execute(&mut *c)
    .await?;
    drop(c);
    let cl = w
        .connect(
            &w.token(Tok {
                identity_mode: "pseudonymous",
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let env = cl
        .call_tool("get_health_status", json!({"profile": "alex"}))
        .await?
        .ok();
    let notes = env["data"]["notes"].as_array().cloned().unwrap_or_default();
    let note = notes.iter().find(|n| n["src"] == sid.as_str()).expect("note")["text"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    for bad in ["Alex", "Rivera", "600", "@", "12345678Z"] {
        assert!(!note.contains(bad), "{note}");
    }
    for good in ["[name]", "[phone]", "[email]"] {
        assert!(note.contains(good), "{note}");
    }
    let rows = cl
        .call_tool("search_observations", json!({"profile": "alex", "query": "Vitamina D"}))
        .await?
        .ok();
    assert_eq!(rows["data"]["results"][0]["lab"], "lab");
    assert_eq!(rows["data"]["results"][0]["date"], "2026-09-14");
    let as_of = today();
    let cl = w
        .connect(
            &w.token(Tok {
                identity_mode: "minimal",
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let rows = cl
        .call_tool(
            "search_observations",
            json!({"profile": "alex", "query": "Vitamina D", "as_of": as_of.format("%Y-%m-%d").to_string()}),
        )
        .await?
        .ok();
    let r = &rows["data"]["results"][0];
    let days = (as_of - NaiveDate::from_ymd_opt(2026, 9, 14).expect("date")).num_days();
    assert_eq!(r["date"], format!("{days} days before as_of"));
    assert!(r["name"].is_null());
    assert_eq!(r["lab"], "lab");
    assert_eq!(r["value"], json!(24));
    assert_eq!(r["unit"], "ng/mL");
    let cl = w.connect(&w.tok().await?).await?;
    let env = cl
        .call_tool("get_health_status", json!({"profile": "alex"}))
        .await?
        .ok();
    assert!(
        env["data"]["notes"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|n| n["text"]["text"] == body)
    );
    Ok(())
}

#[test]
fn detectors_leave_dates_and_values_alone() {
    assert_eq!(
        redact::scrub("LDL 97 mg/dL on 2026-09-14", &[]),
        "LDL 97 mg/dL on 2026-09-14"
    );
    assert_eq!(redact::scrub("IBAN ES91 2100 0418 4502 0005 1332", &[]), "IBAN [iban]");
    assert_eq!(redact::stricter("full", Some("minimal")), "minimal");
    assert_eq!(redact::stricter("minimal", Some("full")), "minimal");
    assert_eq!(redact::age_band(Some(8)).as_deref(), Some("0-17"));
    assert_eq!(redact::age_band(Some(47)).as_deref(), Some("40-49"));
}
