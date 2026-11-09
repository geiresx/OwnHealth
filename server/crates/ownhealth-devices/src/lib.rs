//! Lane D (v0.2): wearables and devices without a vendor cloud (connectors.md 2.2-2.3, 4).
//!
//! | Module | What |
//! |---|---|
//! | `hae` | Health Auto Export (iOS) JSON v2 push to `POST /api/ingest/health-auto-export` with a push token of a `health_auto_export` source; scheduled every 15 min by the app; 0 duplicate samples on re-push |
//! | `gadgetbridge` | the Gadgetbridge SQLite export (`import.gadgetbridge`, sniffed as `gadgetbridge_db` on upload): device tables by name suffix |
//! | `nightscout` | pull `/api/v1/entries/sgv.json` (and the v3 API) from the person's own Nightscout with a read token; FreeStyle Libre arrives the same way through nightscout-connect |
//! | `cgm` | CGM readings as LOINC 99504-3; the HL7 CGM IG summary (mean glucose 97507-8, GMI, times in ranges 106793-3); valid only with >= 70% sensor wear over 14 days |
//! | `store` | the one write path into `wearable_sample`, idempotent, then the rollups |
//! | `synthetic`, `demo` | synthetic CGM for the demo seed (order 1040) and the tests |
//!
//! Samples go to `wearable_sample` through lane C's (0.2.0) wearables store, with a `wearable_source`
//! row linked to the connector `source` (`wearable_source.source_id`). Raw pushes and files are stored
//! as blobs first (connectors.md 6.3 rule 1).

pub mod cgm;
pub mod demo;
pub mod gadgetbridge;
pub mod hae;
pub mod nightscout;
pub mod store;
pub mod synthetic;

use chrono::{DateTime, NaiveDate, Utc};

/// Register `import.gadgetbridge`, the `nightscout` poller and the demo hook. Called by
/// `ownhealth::register_lanes`.
pub fn register() {
    gadgetbridge::register();
    nightscout::register();
    ownhealth_core::demo::register_demo_hook("devices_cgm", demo::ORDER, demo::seed_boxed);
}

fn day_start(d: NaiveDate) -> DateTime<Utc> {
    d.and_hms_opt(0, 0, 0).map_or(DateTime::<Utc>::MIN_UTC, |n| n.and_utc())
}
