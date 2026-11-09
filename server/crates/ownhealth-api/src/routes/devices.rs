//! Lane D (v0.2): device data over REST (`ownhealth_devices`).
//!
//! | Route | Auth | What |
//! |---|---|---|
//! | `POST /ingest/health-auto-export` | `Authorization: Bearer oh_ing_...` (or `X-OwnHealth-Secret`) of a `health_auto_export` source | Health Auto Export JSON v2 -> `{seen, new, duplicate}` |
//! | `GET /profiles/{p}/cgm/summary?days=14&end=` | session, `read:wearables` | the CGM summary (`ownhealth_devices::cgm::CgmSummary`) with its LOINC codes |
//!
//! The Gadgetbridge export goes through the normal upload (`POST /ingest/upload`, sniffed as
//! `gadgetbridge_db`); a Nightscout site is a `nightscout` source (`/inboxes`).
//!
//! Every push is a `source_run` (trigger `push`), so the inbox screen and the health endpoint show the
//! last successful sync. A refused push records the reason, never the body.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::access::PrincipalKind;
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::sources::{begin_run, finish_run};
use ownhealth_core::{Error, Runtime};
use ownhealth_devices::{cgm, hae};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::{Caller, SourceCaller};

async fn health_auto_export(
    SourceCaller(source): SourceCaller,
    State(rt): State<Runtime>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if source.kind != "health_auto_export" {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_inbox",
            "This token belongs to another kind of inbox.",
        ));
    }
    let mut conn = rt.pool.acquire().await?;
    let run = begin_run(&mut conn, source.id, "push", None).await?;
    let result = async {
        let mut tx = rt.pool.begin().await?;
        let o = hae::import_push(&mut tx, &rt.blobs, &source, &body).await?;
        tx.commit().await?;
        Ok::<_, Error>(o)
    }
    .await;
    match &result {
        Ok(o) => finish_run(&mut conn, run, Ok(o)).await?,
        Err(Error::Invalid(m)) => finish_run(&mut conn, run, Err(m)).await?,
        Err(_) => finish_run(&mut conn, run, Err("internal error")).await?,
    }
    let o = result?;
    Ok(Json(json!({"seen": o.seen, "new": o.new, "duplicate": o.duplicate})))
}

fn bad(msg: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", msg)
}

/// `end` as an instant (RFC 3339) or a date (its end, UTC); now when absent.
fn end_param(q: &[(String, String)]) -> Result<DateTime<Utc>, ApiError> {
    let Some((_, v)) = q.iter().find(|(k, _)| k == "end") else {
        return Ok(Utc::now());
    };
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Ok(t.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(v, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.succ_opt())
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
        .ok_or_else(|| bad("end must be a date (YYYY-MM-DD) or an RFC 3339 time."))
}

async fn cgm_summary(
    caller: Caller,
    State(rt): State<Runtime>,
    Path(p): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> Result<Json<Value>, ApiError> {
    let days = match q.iter().find(|(k, _)| k == "days") {
        None => cgm::DEFAULT_DAYS,
        Some((_, v)) => v
            .parse::<i64>()
            .ok()
            .filter(|d| (1..=90).contains(d))
            .ok_or_else(|| bad("days must be a whole number from 1 to 90."))?,
    };
    let end = end_param(&q)?;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, Some("read:wearables"), None).await?;
    let summary = cgm::cgm_summary(&mut tx, perm.profile.id, end, days).await?;
    let principal = &caller.0;
    let via = if principal.kind == PrincipalKind::Token {
        "api"
    } else {
        "ui"
    };
    let mut ev = AuditEvent::new(principal.actor_kind(), via, "read");
    ev.actor_id = principal.user_id;
    ev.token_id = principal.token_id;
    ev.tool = Some("cgm.summary".into());
    ev.profile_id = Some(perm.profile.id);
    ev.resource_type = Some("wearable_sample".into());
    ev.resource_ids = Some(vec![cgm::METRIC.into()]);
    write_audit(&mut tx, &ev).await?;
    tx.commit().await?;
    let mut out = serde_json::to_value(&summary).map_err(Error::from)?;
    out["metric"] = json!(cgm::METRIC);
    out["unit"] = json!("mg/dL");
    out["loinc"] = json!({
        "reading": cgm::LOINC_READING,
        "mean_glucose": cgm::LOINC_MEAN_GLUCOSE,
        "times_in_ranges": cgm::LOINC_TIR_PANEL,
    });
    Ok(Json(out))
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route(
            "/ingest/health-auto-export",
            post(health_auto_export).layer(DefaultBodyLimit::max(hae::MAX_BODY_BYTES)),
        )
        .route("/profiles/{p}/cgm/summary", get(cgm_summary))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use chrono::Duration;
    use ownhealth_core::demo::create_rivera_family;
    use ownhealth_core::sources::{self, NewSource};
    use ownhealth_core::testing::TestDb;
    use ownhealth_devices::store::{self, Conflict, Sample};
    use ownhealth_devices::synthetic::cgm_readings;
    use tower::ServiceExt;

    use super::*;

    type R = anyhow::Result<()>;

    async fn send(
        app: &Router,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = app.clone().oneshot(req.body(Body::from(body))?).await?;
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }

    #[tokio::test]
    async fn pushes_record_runs_and_the_cgm_summary_answers() -> R {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        // dev auth: the caller is the household's first adult (Alex)
        let settings = db.settings(dir.path())?;
        let rt = Runtime::with_pool(settings, db.pool.clone())?;
        let app = Router::new().nest("/api", crate::router()).with_state(rt);
        let mut c = db.pool.acquire().await?;
        let fam = create_rivera_family(&mut c, "Rivera").await?;
        let hae_id = sources::create_source(
            &mut c,
            &NewSource {
                profile_id: Some(fam.profile("alex")),
                ..NewSource::new(fam.household_id, "health_auto_export", "iPhone de Alex")
            },
        )
        .await?;
        let token = sources::issue_token(&mut c, hae_id).await?;
        let bearer = format!("Bearer {token}");
        let body = serde_json::to_vec(&json!({"data": {"metrics": [{"name": "step_count", "units": "count",
            "data": [{"date": "2026-11-02 10:00:00 +0100", "qty": 321, "source": "Apple Watch de Alex"}]}]}}))?;
        let push = "/api/ingest/health-auto-export";
        let (s, v) = send(&app, Method::POST, push, &[("authorization", &bearer)], body.clone()).await?;
        assert_eq!((s, v), (StatusCode::OK, json!({"seen": 1, "new": 1, "duplicate": 0})));
        let (s, v) = send(&app, Method::POST, push, &[("x-ownhealth-secret", &token)], body).await?;
        assert_eq!((s, v), (StatusCode::OK, json!({"seen": 1, "new": 0, "duplicate": 1})));
        let (s, v) = send(
            &app,
            Method::POST,
            push,
            &[("authorization", &bearer)],
            b"<xml/>".to_vec(),
        )
        .await?;
        assert_eq!(
            (s, v["detail"]["code"].clone()),
            (StatusCode::UNPROCESSABLE_ENTITY, json!("invalid"))
        );
        let runs = sources::list_runs(&mut c, hae_id, 10).await?;
        let got: Vec<(&str, &str, i32)> = runs
            .iter()
            .map(|r| (r.trigger.as_str(), r.status.as_str(), r.items_new))
            .collect();
        assert_eq!(got, vec![("push", "error", 0), ("push", "ok", 0), ("push", "ok", 1)]);
        assert!(
            runs[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not Health Auto Export JSON")
        );
        let health = sources::source_health(&mut c, fam.household_id).await?;
        assert!(health.iter().any(|h| h.id == hae_id && h.last_success_at.is_some()));

        // another kind of inbox's token is refused here
        let ns_id = sources::create_source(
            &mut c,
            &NewSource {
                profile_id: Some(fam.profile("lucia")),
                config: json!({"base_url": "http://ns.home.arpa"}),
                ..NewSource::new(fam.household_id, "nightscout", "Nightscout")
            },
        )
        .await?;
        let other = sources::issue_token(&mut c, ns_id).await?;
        let (s, _) = send(
            &app,
            Method::POST,
            push,
            &[("authorization", &format!("Bearer {other}"))],
            b"{}".to_vec(),
        )
        .await?;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // 14 days of CGM for Lucía, then her summary
        let end = Utc::now();
        let wsid = store::device_source(
            &mut c,
            fam.profile("lucia"),
            "nightscout",
            None,
            Some("ns"),
            Some(ns_id),
        )
        .await?;
        let samples = cgm_readings(end - Duration::days(14), 14, 5, 9)
            .into_iter()
            .map(|(t, v)| Sample {
                metric: cgm::METRIC,
                t,
                t_end: None,
                v,
                tz: None,
                source: wsid,
            })
            .collect();
        store::write(&mut c, fam.profile("lucia"), None, samples, Conflict::Keep).await?;
        let uri = format!(
            "/api/profiles/lucia/cgm/summary?end={}",
            end.to_rfc3339().replace('+', "%2B")
        );
        let (s, v) = send(&app, Method::GET, &uri, &[], vec![]).await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(
            (v["valid"].clone(), v["loinc"]["reading"].clone()),
            (json!(true), json!("99504-3"))
        );
        assert!(v["in_range_pct"].as_f64().is_some_and(|x| x > 50.0), "{v}");
        let (s, v) = send(&app, Method::GET, "/api/profiles/lucia/cgm/summary?days=7", &[], vec![]).await?;
        assert_eq!(
            (s, v["valid"].clone(), v["invalid_reason"].clone()),
            (StatusCode::OK, json!(false), json!("short_window"))
        );
        for bad in ["days=0", "days=x", "end=yesterday"] {
            let (s, _) = send(
                &app,
                Method::GET,
                &format!("/api/profiles/lucia/cgm/summary?{bad}"),
                &[],
                vec![],
            )
            .await?;
            assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        }
        let audited: i64 = sqlx::query_scalar("select count(*) from audit_event where tool = 'cgm.summary'")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(audited, 2);
        Ok(())
    }
}
