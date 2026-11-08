//! The v0.1 lab-import exit criteria on the whole synthetic eval set (ROADMAP v0.1, ingestion.md 13).
//!
//! Two runs: with the catalog stand-in normalizer (lane B's pipeline alone; every gate strict), and
//! with lane A's real normalizer (the system as it ships; no false auto-accept allowed). The
//! normalizer hook is process-wide, so the two runs take turns.
mod support;

use support::env::{env, fake_normalizer};
use support::evaluate::{SlipResult, Totals, evaluate, report, totals};
use support::generate;

static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The results, the totals and the derivation versions of the committed observations (which
/// normalizer actually ran).
async fn run() -> anyhow::Result<(Vec<SlipResult>, Totals, Vec<String>)> {
    let e = env().await?;
    let mut c = e.conn().await?;
    let results = evaluate(&mut c, &e.blobs, &generate::all_slips()).await?;
    let tot = totals(&results);
    println!("{}{}", report(&results), serde_json::to_string_pretty(&tot)?);
    let versions: Vec<String> = sqlx::query_scalar("select distinct derivation_version from observation order by 1")
        .fetch_all(&mut *c)
        .await?;
    Ok((results, tot, versions))
}

fn gates(results: &[SlipResult], tot: &Totals) {
    assert!(tot.slips == 41 && tot.row_f1 >= 0.97, "{tot:?}");
    assert_eq!(tot.false_auto_accepts, 0, "{}", report(results));
    assert_eq!(tot.dropped, 0, "{}", report(results));
    assert_eq!(tot.auto_with_page_bbox, tot.auto_accepted);
    let ln = results
        .iter()
        .find(|r| r.slip == "es-labnorte-3p")
        .expect("the LabNorte report");
    assert!(ln.auto as f64 / ln.truth as f64 >= 0.98, "{ln:?}");
    assert_eq!(ln.auto_with_bbox, ln.auto);
}

#[tokio::test]
async fn exit_criteria_with_the_stand_in_normalizer() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    fake_normalizer();
    let (results, tot, versions) = run().await?;
    assert_eq!(versions, [support::fake_normalize::VERSION]);
    gates(&results, &tot);
    Ok(())
}

#[tokio::test]
async fn exit_criteria_with_lane_a() -> anyhow::Result<()> {
    let _turn = TURN.lock().await;
    ownhealth_ingest_pdf::use_normalizer(None);
    let (results, tot, versions) = run().await?;
    assert!(
        !versions.is_empty() && !versions.iter().any(|v| v == support::fake_normalize::VERSION),
        "{versions:?}"
    );
    gates(&results, &tot);
    Ok(())
}
