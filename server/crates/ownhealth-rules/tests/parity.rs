//! Byte parity with the Python backend 0.1.1: on the rows its demo seed wrote, the Rust state code
//! gives the same bodies and state_hash, the same rendered status, dashboards, check_drug answers and
//! status words (fixtures/expected.json, written by the Python backend).

mod common;

use common::{EXPECTED, as_of, seeded};
use ownhealth_core::access::{all_scopes, scope_set};
use ownhealth_rules::{
    build_state, canonical_json, check_drug, dashboard, render, rules_summary, state_hash, status_word,
};
use serde_json::Value;

type R = anyhow::Result<()>;

const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];

/// Same JSON, same key order.
fn same(what: &str, got: &Value, want: &Value) {
    let (g, w) = (
        serde_json::to_string(got).unwrap_or_default(),
        serde_json::to_string(want).unwrap_or_default(),
    );
    assert!(g == w, "{what} differs from Python\n rust:   {g}\n python: {w}");
}

#[test]
fn the_pack_hash_and_the_rules_route_match_python() -> R {
    let pack = ownhealth_rules::load_pack()?;
    assert_eq!(pack.sha256, EXPECTED["pack"]["sha256"].as_str().unwrap_or_default());
    assert_eq!(pack.version, EXPECTED["pack"]["version"].as_str().unwrap_or_default());
    same("GET /rules", &rules_summary()?, &EXPECTED["rules"]);
    Ok(())
}

#[tokio::test]
async fn bodies_hashes_and_renders_match_python() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    for slug in SLUGS {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        let want = &EXPECTED["bodies"][slug];
        same(&format!("{slug} body"), &body, want);
        assert_eq!(canonical_json(&body)?, canonical_json(want)?);
        assert_eq!(state_hash(&body)?, want["state_hash"].as_str().unwrap_or_default());
        for lang in ["en", "es"] {
            for mode in ["full", "pseudonymous", "minimal"] {
                let key = format!("{slug}/{lang}/{mode}");
                same(
                    &key,
                    &render(&body, lang, "as_reported", mode, None)?,
                    &EXPECTED["renders"][&key],
                );
            }
        }
    }
    let body = build_state(&mut tx, s.pid("alex"), as_of(), None).await?;
    let summary = scope_set(["read:summary"]);
    let mut no_genome = all_scopes();
    no_genome.remove("read:genome");
    same(
        "alex/summary",
        &render(&body, "en", "as_reported", "pseudonymous", Some(&summary))?,
        &EXPECTED["renders"]["alex/summary"],
    );
    same(
        "alex/no_genome",
        &render(&body, "en", "as_reported", "pseudonymous", Some(&no_genome))?,
        &EXPECTED["renders"]["alex/no_genome"],
    );
    same(
        "alex/all",
        &render(&body, "en", "as_reported", "pseudonymous", Some(&all_scopes()))?,
        &EXPECTED["renders"]["alex/all"],
    );
    let past = chrono::NaiveDate::from_ymd_opt(2025, 2, 1).unwrap_or_default();
    same(
        "alex@2025-02-01",
        &build_state(&mut tx, s.pid("alex"), past, None).await?,
        &EXPECTED["bodies"]["alex@2025-02-01"],
    );
    tx.rollback().await?;
    Ok(())
}

/// The `canonical` blocks come from lane A's conversions (normalize::group_units, to_ucum, convert).
#[tokio::test]
async fn renders_in_si_and_conventional_units_match_python() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    for slug in SLUGS {
        let body = build_state(&mut tx, s.pid(slug), as_of(), None).await?;
        for lang in ["en", "es"] {
            for units in ["si", "conventional"] {
                let key = format!("{slug}/{lang}/units:{units}");
                same(
                    &key,
                    &render(&body, lang, units, "pseudonymous", None)?,
                    &EXPECTED["renders"][&key],
                );
            }
        }
    }
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn dashboards_match_python() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let check = |key: String, mut got: Value| {
        let generated = got.as_object_mut().and_then(|m| m.shift_remove("generated_at"));
        assert!(
            generated
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|g| g.ends_with('Z') && g.len() == 20)
        );
        same(&key, &got, &EXPECTED["dashboards"][&key]);
    };
    for slug in SLUGS {
        for lang in ["en", "es"] {
            check(
                format!("{slug}/{lang}"),
                dashboard(&mut tx, s.pid(slug), as_of(), lang, &all_scopes()).await?,
            );
        }
    }
    let summary = scope_set(["read:summary"]);
    check(
        "alex/summary".into(),
        dashboard(&mut tx, s.pid("alex"), as_of(), "en", &summary).await?,
    );
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn check_drug_matches_python_for_every_drug() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let want = EXPECTED["check_drug"].as_object().cloned().unwrap_or_default();
    assert!(want.len() > 200);
    for (key, expected) in &want {
        let parts: Vec<&str> = key.split('/').collect();
        let (slug, drug, last) = (parts[0], parts[1], parts[2]);
        let (lang, scopes) = match last {
            "en" | "es" => (last, all_scopes()),
            scope => ("en", scope_set([scope])),
        };
        let got = check_drug(&mut tx, s.pid(slug), drug, &scopes, lang, Some(as_of())).await?;
        same(key, &got, expected);
    }
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn status_words_match_python() -> R {
    let s = seeded().await?;
    let mut conn = s.pool().acquire().await?;
    for (group, word) in EXPECTED["status_words"].as_object().cloned().unwrap_or_default() {
        assert_eq!(
            status_word(&mut conn, s.pid("alex"), &group, as_of()).await?,
            word.as_str().unwrap_or_default(),
            "{group}"
        );
    }
    Ok(())
}
