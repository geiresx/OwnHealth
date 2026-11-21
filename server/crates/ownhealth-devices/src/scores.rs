//! Vendor scores (migration 1220): readiness, recovery, sleep and activity scores, stress, strain and
//! Body Battery are the vendor's numbers, computed with its own method. They are stored apart from
//! measurements, in `vendor_score`, tagged `computed_by = 'provider'` with the provider's name and the
//! connection that brought them, and never written into `wearable_sample` (a CHECK there refuses a key
//! that `is_sample_key` refuses).
//!
//! The rule pack, the calculators and MCP never read them; only the Wearables page shows them.

use chrono::{DateTime, Utc};
use ownhealth_core::Result;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

/// The `vendor_score.category` CHECK, in the order the Wearables page lists them.
pub const CATEGORIES: [&str; 7] = [
    "readiness",
    "recovery",
    "sleep",
    "activity",
    "stress",
    "strain",
    "body_battery",
];

/// Words that mark a series key as a vendor score, wherever they stand in the key and however it is
/// spelled ('BodyBattery', 'body-battery', 'stresslevel'). The same list as the CHECK
/// `wearable_sample_not_a_vendor_score`. 'charge' covers Polar's Nightly Recharge and ANS charge,
/// 'status' Garmin's HRV status and training status, 'performance' WHOOP's sleep performance.
pub const SCORE_WORDS: [&str; 9] = [
    "readiness",
    "battery",
    "stress",
    "strain",
    "score",
    "charge",
    "resilience",
    "status",
    "performance",
];

/// The longest qualifier the table keeps (the `vendor_score.qualifier` CHECK).
pub const QUALIFIER_MAX: usize = 80;

/// True when `metric` names a vendor score, so it belongs in `vendor_score`, never `wearable_sample`:
/// the key, lowercased with everything but letters and digits taken out, contains a word of
/// `SCORE_WORDS` or starts with `recovery` (`hr_recovery` is a measurement).
pub fn is_score_key(metric: &str) -> bool {
    let k: String = metric
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    SCORE_WORDS.iter().any(|w| k.contains(w)) || k.starts_with("recovery")
}

/// True when `metric` may be stored in `wearable_sample`: a plain key (a lowercase letter, then
/// lowercase letters, digits and '_') that is not a vendor score. The same rule as the CHECK
/// `wearable_sample_not_a_vendor_score`.
pub fn is_sample_key(metric: &str) -> bool {
    let mut chars = metric.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !is_score_key(metric)
}

/// One score as a connector maps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    /// The vendor, as `wearable_source.provider` names it (`oura`, `garmin`, `whoop`).
    pub provider: String,
    /// One of `CATEGORIES`.
    pub category: String,
    pub t: DateTime<Utc>,
    pub value: f64,
    /// The vendor's own word for the score ("Optimal").
    pub qualifier: Option<String>,
    /// The vendor's contributors to the score, as a JSON object.
    pub components: Option<Value>,
}

/// A stored score, as the read route sends it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Stored {
    pub provider: String,
    pub category: String,
    pub t: DateTime<Utc>,
    pub value: f64,
    pub qualifier: Option<String>,
    pub components: Option<Value>,
    pub computed_by: String,
    /// The connection that brought the score, `{id, label}`; null once that connection is removed.
    pub source: Option<Value>,
}

/// What a write did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Written {
    /// Rows inserted or changed.
    pub written: u64,
    /// Scores left out because their value is not a finite number in the table's range.
    pub dropped: u64,
}

/// Store `scores` for one profile, brought by the connection `source_id`. A score already stored for
/// the same connection, provider, category and time takes the new value, qualifier and components.
///
/// One vendor quirk never fails the batch: a value that is not finite (or past ±1e9) is dropped and
/// counted, `components` that are not a JSON object are left out, and a qualifier is cut to
/// `QUALIFIER_MAX` characters. An unknown category or a malformed provider is a mapping bug and fails.
pub async fn write(conn: &mut PgConnection, profile_id: Uuid, source_id: Uuid, scores: &[Score]) -> Result<Written> {
    let mut out = Written::default();
    for s in scores {
        if !s.value.is_finite() || s.value.abs() > 1e9 {
            out.dropped += 1;
            continue;
        }
        let components = s.components.as_ref().filter(|c| c.is_object());
        let qualifier = s
            .qualifier
            .as_deref()
            .map(|q| q.chars().take(QUALIFIER_MAX).collect::<String>());
        out.written += sqlx::query(
            "insert into vendor_score (profile_id, source_id, provider, category, t, value, qualifier, components) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (profile_id, source_id, provider, category, t) do update \
             set value = excluded.value, qualifier = excluded.qualifier, components = excluded.components \
             where (vendor_score.value, vendor_score.qualifier, vendor_score.components) \
             is distinct from (excluded.value, excluded.qualifier, excluded.components)",
        )
        .bind(profile_id)
        .bind(source_id)
        .bind(&s.provider)
        .bind(&s.category)
        .bind(s.t)
        .bind(s.value)
        .bind(qualifier)
        .bind(components)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(out)
}

/// The profile's scores from `since` on, by provider, category, connection and time, each with the
/// connection that brought it.
pub async fn list(conn: &mut PgConnection, profile_id: Uuid, since: DateTime<Utc>) -> Result<Vec<Stored>> {
    Ok(sqlx::query_as(
        "select v.provider, v.category, v.t, v.value, v.qualifier, v.components, v.computed_by, \
         case when s.id is null then null else jsonb_build_object('id', s.id, 'label', s.label) end as source \
         from vendor_score v left join source s on s.id = v.source_id \
         where v.profile_id = $1 and v.t >= $2 order by v.provider, v.category, s.label, v.source_id, v.t",
    )
    .bind(profile_id)
    .bind(since)
    .fetch_all(conn)
    .await?)
}
