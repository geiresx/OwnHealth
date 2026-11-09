//! Nightscout pull (lane D): the person's own site, a read token (secret purpose `nightscout_token`),
//! through `core::net::get` with `Egress::home_service(allow_private)`. Readings become CGM samples
//! (`cgm::METRIC`); the cursor is the newest reading's epoch ms. FreeStyle Libre arrives the same way
//! when the site runs nightscout-connect (LibreLinkUp): its readings are ordinary `sgv` entries.
//!
//! Source `config` (kind `nightscout`): `{base_url (or url), allow_private?: bool, api?: "v1"|"v3",
//! backfill_days?: int, overlap_hours?: int}`.
//!
//! Each run reads from the cursor minus `overlap_hours` (default 24): uploaders (Dexcom, xDrip, AAPS)
//! backfill readings missed in a Bluetooth or phone gap hours later, with their original dates, so a
//! cursor-only read would skip them forever. Writes keep the first copy of a reading, so the overlap
//! adds no duplicates. An entry dated more than `FUTURE_SLACK_MINUTES` ahead (an uploader with a wrong
//! clock or time zone) is not imported and does not move the cursor: it is read again, and imported,
//! once its date has come, and the readings after it keep arriving.
//!
//! - v1: `GET /api/v1/entries/sgv.json?find[date][$gt]=<cursor>&count=1000&token=<token>`. The v1 API
//!   answers newest first, so a run pages backwards with `find[date][$lt]` until a page comes back short.
//! - v3: the token is exchanged for a JWT at `/api/v2/authorization/request/<token>`, then
//!   `GET /api/v3/entries?date$gt=<cursor>&sort=date&limit=1000` with `Authorization: Bearer`, forwards.
//!
//! Entries below 39 mg/dL are sensor status codes, not readings, and are dropped (Nightscout's reports
//! drop them too). Error messages name the status, never the URL (it carries the token).

use chrono::{DateTime, Duration, Utc};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::net::{self, Egress, Fetched};
use ownhealth_core::secrets::{SecretBox, SecretString, read_secret};
use ownhealth_core::sources::{RunOutcome, Source};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cgm;
use crate::store::{self, Conflict, Sample};

pub const DEFAULT_POLL_SECONDS: i32 = 300;
pub const PROVIDER: &str = "nightscout";
/// Entries per request.
pub const PAGE: usize = 1000;
/// Requests per run: 300 000 readings, almost three years at one every 5 minutes.
pub const MAX_PAGES: usize = 300;
/// How far back the first run reads.
pub const DEFAULT_BACKFILL_DAYS: i64 = 90;
/// How far behind the cursor each run reads again, for backfilled readings.
pub const DEFAULT_OVERLAP_HOURS: i64 = 24;
/// How far ahead of now an entry may be dated and still count (clock skew between phone and server).
pub const FUTURE_SLACK_MINUTES: i64 = 5;
/// Below this, `sgv` is a sensor status code (Dexcom's 1-38), not a reading.
pub const MIN_SGV: f64 = 39.0;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Config {
    /// `url` is the name the inbox screen writes.
    #[serde(alias = "url")]
    pub base_url: String,
    #[serde(default)]
    pub allow_private: bool,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub backfill_days: Option<i64>,
    #[serde(default)]
    pub overlap_hours: Option<i64>,
}

/// One reading from the site.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    pub date_ms: i64,
    pub sgv: f64,
    pub utc_offset: Option<i16>,
}

fn fail(msg: impl Into<String>) -> JobError {
    JobError::retry(msg)
}

/// The readings in a v1 array or a v3 `{"result": [...]}` answer.
pub fn parse_entries(body: &[u8]) -> Result<Vec<Entry>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "nightscout: the answer is not JSON".to_string())?;
    let items = match &v {
        Value::Array(a) => a,
        Value::Object(o) => match o.get("result") {
            Some(Value::Array(a)) => a,
            _ => return Err("nightscout: the answer has no entries".into()),
        },
        _ => return Err("nightscout: the answer has no entries".into()),
    };
    Ok(items
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str).is_none_or(|t| t == "sgv"))
        .filter_map(|e| {
            let sgv = e.get("sgv").and_then(Value::as_f64)?;
            let date_ms = e.get("date").and_then(Value::as_i64)?;
            let utc_offset = e
                .get("utcOffset")
                .and_then(Value::as_i64)
                .and_then(|o| i16::try_from(o).ok())
                .filter(|o| o.abs() <= 18 * 60);
            (sgv >= MIN_SGV && sgv.is_finite()).then_some(Entry {
                date_ms,
                sgv,
                utc_offset,
            })
        })
        .collect())
}

fn endpoint(base: &str, path: &str, query: &[(&str, String)]) -> Result<String, JobError> {
    let mut u = url::Url::parse(&format!("{}{path}", base.trim_end_matches('/')))
        .map_err(|_| JobError::permanent("nightscout: base_url is not a valid URL"))?;
    {
        let mut q = u.query_pairs_mut();
        for (k, v) in query {
            q.append_pair(k, v);
        }
    }
    if u.query() == Some("") {
        u.set_query(None);
    }
    Ok(u.into())
}

async fn fetch(url: &str, egress: &Egress, headers: &[(&str, &str)]) -> Result<Fetched, JobError> {
    let r = net::get(url, egress, headers)
        .await
        .map_err(|e| fail(format!("nightscout: {e}")))?;
    match r.status {
        200..=299 => Ok(r),
        401 | 403 => Err(fail(format!(
            "nightscout refused the token (HTTP {}): check the read token",
            r.status
        ))),
        s => Err(fail(format!("nightscout answered HTTP {s}"))),
    }
}

async fn fetch_v1(cfg: &Config, token: Option<&SecretString>, since_ms: i64) -> Result<Vec<Entry>, JobError> {
    let egress = Egress::home_service(cfg.allow_private);
    let mut all = Vec::new();
    let mut upper: Option<i64> = None;
    for _ in 0..MAX_PAGES {
        let mut q = vec![("find[date][$gt]", since_ms.to_string()), ("count", PAGE.to_string())];
        if let Some(u) = upper {
            q.push(("find[date][$lt]", u.to_string()));
        }
        if let Some(t) = token {
            q.push(("token", t.expose().to_string()));
        }
        let url = endpoint(&cfg.base_url, "/api/v1/entries/sgv.json", &q)?;
        let page = fetch(&url, &egress, &[("accept", "application/json")]).await?;
        let raw: usize = serde_json::from_slice::<Vec<Value>>(&page.body).map_or(0, |a| a.len());
        let entries = parse_entries(&page.body).map_err(fail)?;
        let oldest = entries.iter().map(|e| e.date_ms).min();
        all.extend(entries);
        match oldest {
            Some(o) if raw >= PAGE => upper = Some(o),
            _ => return Ok(all),
        }
    }
    Err(JobError::permanent(format!(
        "nightscout: more than {} readings since the last sync; lower backfill_days",
        PAGE * MAX_PAGES
    )))
}

async fn fetch_v3(cfg: &Config, token: Option<&SecretString>, since_ms: i64) -> Result<Vec<Entry>, JobError> {
    let egress = Egress::home_service(cfg.allow_private);
    let token = token.ok_or_else(|| JobError::permanent("nightscout: the v3 API needs a read token"))?;
    let url = endpoint(
        &cfg.base_url,
        &format!("/api/v2/authorization/request/{}", token.expose()),
        &[],
    )?;
    let auth = fetch(&url, &egress, &[("accept", "application/json")]).await?;
    let jwt = serde_json::from_slice::<Value>(&auth.body)
        .ok()
        .and_then(|v| v.get("token").and_then(Value::as_str).map(String::from))
        .ok_or_else(|| fail("nightscout: the token exchange gave no JWT"))?;
    let bearer = format!("Bearer {jwt}");
    let mut all: Vec<Entry> = Vec::new();
    let mut cursor = since_ms;
    for _ in 0..MAX_PAGES {
        let q = [
            ("date$gt", cursor.to_string()),
            ("sort", "date".to_string()),
            ("limit", PAGE.to_string()),
            ("fields", "date,sgv,type,utcOffset".to_string()),
        ];
        let url = endpoint(&cfg.base_url, "/api/v3/entries", &q)?;
        let page = fetch(
            &url,
            &egress,
            &[("accept", "application/json"), ("authorization", &bearer)],
        )
        .await?;
        let raw: usize = serde_json::from_slice::<Value>(&page.body)
            .ok()
            .and_then(|v| v.get("result").and_then(Value::as_array).map(Vec::len))
            .unwrap_or(0);
        let entries = parse_entries(&page.body).map_err(fail)?;
        let newest = entries.iter().map(|e| e.date_ms).max();
        all.extend(entries);
        match newest {
            Some(n) if raw >= PAGE && n > cursor => cursor = n,
            _ => return Ok(all),
        }
    }
    Err(JobError::permanent(format!(
        "nightscout: more than {} readings since the last sync; lower backfill_days",
        PAGE * MAX_PAGES
    )))
}

/// Checks a `nightscout` source config before it is saved: an http(s) `base_url` and a known `api`.
pub fn check_config(config: &Value) -> Result<(), String> {
    let cfg: Config =
        serde_json::from_value(config.clone()).map_err(|_| "the source needs config.base_url".to_string())?;
    let ok = url::Url::parse(&cfg.base_url).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some());
    if !ok {
        return Err("config.base_url must be an http(s) URL".into());
    }
    if !matches!(cfg.api.as_deref(), None | Some("v1" | "v3")) {
        return Err("config.api must be v1 or v3".into());
    }
    Ok(())
}

fn config(source: &Source) -> Result<Config, JobError> {
    serde_json::from_value(source.config.clone())
        .map_err(|_| JobError::permanent("nightscout: the source needs config.base_url"))
}

/// The `nightscout` poller.
pub async fn poll_nightscout(ctx: JobContext, source: Source) -> Result<RunOutcome, JobError> {
    let cfg = config(&source)?;
    let profile_id = source
        .profile_id
        .ok_or_else(|| JobError::permanent("nightscout: the source has no profile"))?;
    let token = match source.secret_id {
        Some(id) => {
            let sb = SecretBox::from_settings(&ctx.settings)?;
            let mut conn = ctx.pool.acquire().await?;
            Some(read_secret(&mut conn, &sb, source.household_id, id).await?)
        }
        None => None,
    };
    let backfill = cfg.backfill_days.unwrap_or(DEFAULT_BACKFILL_DAYS).clamp(1, 3650);
    let overlap = cfg.overlap_hours.unwrap_or(DEFAULT_OVERLAP_HOURS).clamp(0, 24 * 14);
    let latest_ok = (Utc::now() + Duration::minutes(FUTURE_SLACK_MINUTES)).timestamp_millis();
    // a cursor a future-dated entry set before this rule existed is pulled back to now
    let cursor = source
        .cursor
        .as_ref()
        .and_then(|c| c.get("date_ms"))
        .and_then(Value::as_i64)
        .map(|c| c.min(latest_ok));
    let since_ms = match cursor {
        Some(c) => c - Duration::hours(overlap).num_milliseconds(),
        None => (Utc::now() - Duration::days(backfill)).timestamp_millis(),
    };
    let mut entries = match cfg.api.as_deref() {
        None | Some("v1") => fetch_v1(&cfg, token.as_ref(), since_ms).await?,
        Some("v3") => fetch_v3(&cfg, token.as_ref(), since_ms).await?,
        Some(_) => return Err(JobError::permanent("nightscout: config.api must be v1 or v3")),
    };
    entries.retain(|e| e.date_ms <= latest_ok);
    let newest = entries
        .iter()
        .map(|e| e.date_ms)
        .chain(cursor)
        .max()
        .unwrap_or(since_ms);
    let mut tx = ctx.pool.begin().await?;
    let wsid = store::device_source(
        &mut tx,
        profile_id,
        PROVIDER,
        None,
        Some(&source.id.to_string()),
        Some(source.id),
    )
    .await?;
    let samples: Vec<Sample> = entries
        .iter()
        .filter_map(|e| {
            DateTime::<Utc>::from_timestamp_millis(e.date_ms).map(|t| Sample {
                metric: cgm::METRIC,
                t,
                t_end: None,
                v: e.sgv,
                tz: e.utc_offset,
                source: wsid,
            })
        })
        .collect();
    let w = store::write(&mut tx, profile_id, None, samples, Conflict::Keep).await?;
    tx.commit().await?;
    let n = |x: i64| i32::try_from(x).unwrap_or(i32::MAX);
    Ok(RunOutcome {
        seen: n(w.seen),
        new: n(w.new),
        duplicate: n(w.duplicate()),
        cursor: Some(json!({ "date_ms": newest })),
        ..RunOutcome::default()
    })
}

pub fn register() {
    ownhealth_core::sources::register_poller("nightscout", |ctx, source| Box::pin(poll_nightscout(ctx, source)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_from_v1_and_v3_answers() -> Result<(), String> {
        let v1 = br#"[{"type":"sgv","sgv":120,"date":1760000000000,"utcOffset":120},
            {"type":"mbg","mbg":118,"date":1760000001000},
            {"type":"sgv","sgv":5,"date":1760000002000},
            {"sgv":140,"date":1760000300000}]"#;
        let e = parse_entries(v1)?;
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].utc_offset, Some(120));
        let v3 = br#"{"status":200,"result":[{"type":"sgv","sgv":99,"date":1760000000000}]}"#;
        assert_eq!(parse_entries(v3)?.len(), 1);
        assert!(parse_entries(b"<html>").is_err());
        assert!(parse_entries(br#"{"status":401}"#).is_err());
        Ok(())
    }

    #[test]
    fn query_keys_are_encoded_and_a_path_prefix_is_kept() -> Result<(), JobError> {
        let u = endpoint(
            "https://ns.example/site/",
            "/api/v1/entries/sgv.json",
            &[("find[date][$gt]", "5".into())],
        )?;
        assert_eq!(
            u,
            "https://ns.example/site/api/v1/entries/sgv.json?find%5Bdate%5D%5B%24gt%5D=5"
        );
        assert_eq!(endpoint("http://h:1337", "/x", &[])?, "http://h:1337/x");
        Ok(())
    }

    #[test]
    fn config_takes_base_url_or_url() -> serde_json::Result<()> {
        let a: Config = serde_json::from_value(json!({"base_url": "http://ns", "allow_private": true}))?;
        let b: Config = serde_json::from_value(json!({"url": "http://ns", "allow_private": true}))?;
        assert_eq!(a, b);
        assert!(serde_json::from_value::<Config>(json!({"allow_private": true})).is_err());
        Ok(())
    }
}
