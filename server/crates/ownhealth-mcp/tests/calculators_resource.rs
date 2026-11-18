//! The calculators resource (`ownhealth://profile/{id}/calculators`): listed and read with
//! read:labs, the same document the REST route serves, refused like any other resource without it.

mod common;

use common::*;
use serde_json::Value;

const URI: &str = "ownhealth://profile/alex/calculators";

#[tokio::test]
async fn an_agent_with_read_labs_reads_the_calculators() -> R {
    let w = world().await?;
    let c = w
        .connect(&w.tok_scopes(scopes(&["read:summary", "read:labs"])).await?)
        .await?;
    assert!(c.list_resources().await?.iter().any(|u| u == URI));
    let doc: Value = serde_json::from_str(&c.read_text(URI).await?)?;
    assert_eq!(doc["schema"], "ownhealth.calculators/v1");
    assert_eq!(doc["profile"], "alex");
    assert_eq!(doc["lang"], "en");
    assert!(
        doc["calculators_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("sha256:"))
    );
    let ids: Vec<&str> = doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["id"].as_str())
        .collect();
    for id in ["ckd_epi_2021_cr", "ckd_epi_2021_cr_cys", "fib4", "homa_ir"] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    for it in doc["items"].as_array().into_iter().flatten() {
        assert!(it["cite"].as_str().is_some_and(|u| u.starts_with("https://")), "{it}");
        assert!(it["name"].as_str().is_some_and(|n| !n.is_empty()));
    }
    Ok(())
}

#[tokio::test]
async fn a_token_without_read_labs_neither_sees_nor_reads_it() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok_scopes(without(&["read:labs"])).await?).await?;
    assert!(!c.list_resources().await?.iter().any(|u| u.ends_with("/calculators")));
    let refused = c.read_resource(URI).await.expect_err("needs read:labs");
    let missing = c
        .read_resource("ownhealth://source/src_doesnotexist")
        .await
        .expect_err("missing");
    // one answer for every refusal, as for the other resources
    assert_eq!(refused.code, missing.code);
    assert!(refused.message.contains("scope_missing"), "{}", refused.message);
    Ok(())
}

/// A token in minimal identity mode gets the draw dates relative to as_of, as every tool answer
/// does; the hash stays the document's.
#[tokio::test]
async fn a_minimal_token_reads_dates_relative_to_as_of() -> R {
    let w = world().await?;
    let read = |mode: &'static str| {
        let w = &w;
        async move {
            let c = w
                .connect(
                    &w.token(Tok {
                        identity_mode: mode,
                        profiles: vec!["alex", "lucia"],
                        ..Tok::default()
                    })
                    .await?,
                )
                .await?;
            let mut out = Vec::new();
            for uri in [URI, "ownhealth://profile/lucia/calculators"] {
                out.push(serde_json::from_str::<Value>(&c.read_text(uri).await?)?);
            }
            anyhow::Ok(out)
        }
    };
    let full = read("full").await?;
    let minimal = read("minimal").await?;
    let iso = |s: &str| s.len() >= 10 && s.as_bytes()[4] == b'-' && s[..4].chars().all(|c| c.is_ascii_digit());
    let mut dates = 0;
    for (f, m) in full.iter().zip(&minimal) {
        assert_eq!(f["calculators_hash"], m["calculators_hash"]);
        for it in m["items"].as_array().into_iter().flatten() {
            let mut seen: Vec<&str> = vec![it["date"].as_str().unwrap_or_default()];
            seen.extend(
                it["inputs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i["date"].as_str()),
            );
            seen.extend(it["reason"]["on"].as_str());
            for s in seen.into_iter().filter(|s| !s.is_empty()) {
                dates += 1;
                assert!(!iso(s) && s.ends_with("as_of"), "{s} in {it}");
            }
            // the refusal text is filled after the dates were made relative
            let text = it["text"].as_str().unwrap_or_default();
            assert!(!text.split_whitespace().any(iso), "{text}");
        }
    }
    assert!(dates >= 3, "the control saw dates: {dates}");
    Ok(())
}

/// A token with read:labs only reads the lab calculators, and each risk result as not assessed for
/// the scopes it lacks: no condition, medicine or smoking line and no src of them. With read:records
/// and read:vitals the statin and its src show, so the check can fail.
#[tokio::test]
async fn a_read_labs_token_sees_no_condition_medicine_or_vital_in_a_risk_result() -> R {
    let w = world().await?;
    let read = |s: &'static [&'static str]| {
        let w = &w;
        async move {
            let c = w.connect(&w.tok_scopes(scopes(s)).await?).await?;
            anyhow::Ok(serde_json::from_str::<Value>(&c.read_text(URI).await?)?)
        }
    };
    let all = read(&["read:summary", "read:labs", "read:records", "read:vitals"]).await?;
    // the control: with read:records and read:vitals the statin line shows, with its src
    let statin_src = all["items"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|i| i["inputs"].as_array().into_iter().flatten())
        .find(|i| i["group"] == "statin" && i["value"] == "recorded")
        .and_then(|i| i["src"].as_str())
        .map(str::to_string)
        .expect("the control sees the statin");
    let doc = read(&["read:labs"]).await?;
    let body = doc.to_string();
    for hidden in [
        statin_src.as_str(),
        "\"statin\"",
        "antihypertensive",
        "\"diabetes\"",
        "smoking_status",
        "bp_systolic",
    ] {
        assert!(!body.contains(hidden), "{hidden} in {body}");
    }
    let mut risk = 0;
    for it in doc["items"].as_array().into_iter().flatten() {
        let id = it["id"].as_str().unwrap_or_default();
        if id.starts_with("score2") || id.starts_with("prevent") {
            risk += 1;
            assert_eq!(it["reason_code"], "scope_missing", "{it}");
            assert_eq!(
                it["reason"]["scopes"],
                serde_json::json!(["read:records", "read:vitals"])
            );
            assert_eq!(it["inputs"], serde_json::json!([]));
        }
    }
    assert!(risk >= 3, "{risk}");
    Ok(())
}
