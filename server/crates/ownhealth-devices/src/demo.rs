//! Lane D demo data (order 1040): Lucía's Nightscout site as a `nightscout` source with 14 days of
//! synthetic CGM readings ending at the demo's date, and one finished run, so the inbox screen shows a
//! healthy source and the CGM summary has a valid window. The source has no poll interval: the demo
//! never reaches out to a site that does not exist.

use chrono::Duration;
use futures::future::BoxFuture;
use ownhealth_core::Result;
use ownhealth_core::demo::DemoContext;
use ownhealth_core::sources::{self, NewSource, RunOutcome};
use serde_json::json;
use sqlx::PgConnection;

use crate::cgm;
use crate::nightscout::PROVIDER;
use crate::store::{self, Conflict, Sample};
use crate::synthetic::cgm_readings;

pub const ORDER: i32 = 1040;
pub const DAYS: i64 = 14;

pub async fn seed(conn: &mut PgConnection, ctx: &mut DemoContext) -> Result<()> {
    let lucia = ctx.family.profile("lucia");
    let source_id = sources::create_source(
        &mut *conn,
        &NewSource {
            profile_id: Some(lucia),
            config: json!({"base_url": "https://nightscout.rivera.home.arpa", "allow_private": true, "api": "v1"}),
            ..NewSource::new(ctx.family.household_id, "nightscout", "Nightscout de Lucía")
        },
    )
    .await?;
    let end = crate::day_start(ctx.as_of) + Duration::hours(8);
    let start = end - Duration::days(DAYS);
    let wsid = store::device_source(
        &mut *conn,
        lucia,
        PROVIDER,
        None,
        Some(&source_id.to_string()),
        Some(source_id),
    )
    .await?;
    let readings = cgm_readings(start, DAYS, 5, 0x0c6a_11c1a);
    let newest = readings.iter().map(|r| r.0).max().unwrap_or(end);
    let samples = readings
        .into_iter()
        .map(|(t, v)| Sample {
            metric: cgm::METRIC,
            t,
            t_end: None,
            v,
            tz: Some(60),
            source: wsid,
        })
        .collect();
    let w = store::write(&mut *conn, lucia, None, samples, Conflict::Keep).await?;
    let run = sources::begin_run(&mut *conn, source_id, "manual", None).await?;
    let n = |x: i64| i32::try_from(x).unwrap_or(i32::MAX);
    sources::finish_run(
        &mut *conn,
        run,
        Ok(&RunOutcome {
            seen: n(w.seen),
            new: n(w.new),
            duplicate: n(w.duplicate()),
            cursor: Some(json!({"date_ms": newest.timestamp_millis()})),
            ..RunOutcome::default()
        }),
    )
    .await?;
    Ok(())
}

pub fn seed_boxed<'c>(conn: &'c mut PgConnection, ctx: &'c mut DemoContext) -> BoxFuture<'c, Result<()>> {
    Box::pin(seed(conn, ctx))
}
