//! Typed values: a typed LOINC drives the derived layer like a received code (`tests/imports/test_manual.py`).

mod common;

use chrono::{FixedOffset, TimeZone};
use common::{Env, R, family};
use ownhealth_ingest_structured::manual::{ManualObservation, add_manual_observation};
use rust_decimal::Decimal;

#[tokio::test]
async fn a_typed_loinc_sets_the_alias_group() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let at = FixedOffset::east_opt(0)
        .and_then(|z| z.with_ymd_and_hms(2026, 6, 2, 9, 0, 0).single())
        .ok_or_else(|| anyhow::anyhow!("date"))?;
    let committed = add_manual_observation(
        &mut tx,
        &ManualObservation {
            profile_id: fam.profile("alex"),
            user_id: fam.user("alex"),
            name: "VNDR-FERR home kit",
            value: "85",
            unit: Some("ng/mL"),
            effective_at: at,
            note: None,
            category: None,
            loinc: Some("2276-4"),
            value_code: None,
            corrects: None,
            reason: None,
        },
    )
    .await?;
    let (loinc, group, canonical, unit): (Option<String>, String, Option<Decimal>, Option<String>) =
        sqlx::query_as("select loinc, alias_group, value_canonical, unit_canonical from observation where id = $1")
            .bind(committed.id)
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!((loinc.as_deref(), group.as_str()), (Some("2276-4"), "ferritin"));
    assert!(canonical.is_some() && unit.is_some());
    Ok(())
}
