//! v0.2 Ops: the health endpoint per source, with the last successful sync for alerting. JSON for the
//! app, the Prometheus text format for a monitor, and each source readable with its own token.

#[path = "auth_support.rs"]
mod support;

use axum::http::{Method, StatusCode};
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::sources::{self, NewSource, RunOutcome, SourcePatch};
use serde_json::Value;
use sqlx::PgConnection;
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

struct Ids {
    alex_email: Uuid,
    carmen_nightscout: Uuid,
    folder: Uuid,
    lucia_hae: Uuid,
    other_household: Uuid,
}

async fn source(
    conn: &mut PgConnection,
    household: Uuid,
    profile: Option<Uuid>,
    kind: &str,
    label: &str,
    poll: Option<i32>,
) -> anyhow::Result<Uuid> {
    Ok(sources::create_source(
        conn,
        &NewSource {
            profile_id: profile,
            poll_seconds: poll,
            ..NewSource::new(household, kind, label)
        },
    )
    .await?)
}

async fn run(conn: &mut PgConnection, id: Uuid, ok: bool) -> R {
    let run = sources::begin_run(conn, id, "poll", None).await?;
    let outcome = RunOutcome::default();
    sources::finish_run(conn, run, if ok { Ok(&outcome) } else { Err("connection refused") }).await?;
    Ok(())
}

async fn setup(env: &Env) -> anyhow::Result<Ids> {
    let mut c = env.pool().acquire().await?;
    let h = env.fam().household_id;
    let ids = Ids {
        alex_email: source(
            &mut c,
            h,
            Some(env.profile("alex")),
            "email",
            "Correo de Alex",
            Some(300),
        )
        .await?,
        carmen_nightscout: source(
            &mut c,
            h,
            Some(env.profile("carmen")),
            "nightscout",
            "Nightscout de Carmen",
            Some(300),
        )
        .await?,
        folder: source(&mut c, h, None, "folder", "Escáner", Some(60)).await?,
        lucia_hae: source(
            &mut c,
            h,
            Some(env.profile("lucia")),
            "health_auto_export",
            "iPad",
            None,
        )
        .await?,
        other_household: {
            let other = create_rivera_family(&mut c, "Otra").await?;
            source(&mut c, other.household_id, None, "folder", "Otra casa", Some(60)).await?
        },
    };
    run(&mut c, ids.alex_email, false).await?;
    run(&mut c, ids.alex_email, false).await?;
    run(&mut c, ids.carmen_nightscout, true).await?;
    Ok(ids)
}

fn ids_of(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
}

fn row(v: &Value, id: Uuid) -> &Value {
    v.as_array()
        .and_then(|a| a.iter().find(|r| r["id"] == id.to_string()))
        .unwrap_or(&Value::Null)
}

#[tokio::test]
async fn each_person_sees_the_sources_of_the_profiles_they_can_open() -> R {
    let env = Env::new(true).await?;
    let ids = setup(&env).await?;

    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.get("/api/inboxes/health").await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let mut got = ids_of(&r.json);
    got.sort();
    let mut want: Vec<String> = [ids.alex_email, ids.carmen_nightscout, ids.folder, ids.lucia_hae]
        .iter()
        .map(Uuid::to_string)
        .collect();
    want.sort();
    assert_eq!(
        got, want,
        "alex: own, carmen (caregiver), lucía (guardian), household-wide"
    );

    let email = row(&r.json, ids.alex_email);
    assert_eq!(email["kind"], "email");
    assert_eq!(email["consecutive_failures"], 2);
    assert_eq!(email["last_error"], "connection refused");
    assert!(email["last_success_at"].is_null());
    assert_eq!(email["stale"], true, "polled and never synced");
    let ns = row(&r.json, ids.carmen_nightscout);
    assert!(ns["last_success_at"].is_string());
    assert_eq!(
        (ns["stale"].as_bool(), ns["consecutive_failures"].as_i64()),
        (Some(false), Some(0))
    );
    assert_eq!(row(&r.json, ids.folder)["stale"], true);
    assert_eq!(
        row(&r.json, ids.lucia_hae)["stale"],
        false,
        "a push source is never stale"
    );
    for r in r.json.as_array().into_iter().flatten() {
        let keys: Vec<&String> = r.as_object().into_iter().flatten().map(|(k, _)| k).collect();
        assert!(
            !keys.iter().any(|k| k.contains("secret") || k.contains("token")),
            "{keys:?}"
        );
    }

    // sam has no grant on carmen
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam.get("/api/inboxes/health").await?;
    let got = ids_of(&r.json);
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(!got.contains(&ids.carmen_nightscout.to_string()));
    assert!(!got.contains(&ids.other_household.to_string()));
    let r = sam
        .get(&format!("/api/inboxes/{}/health", ids.carmen_nightscout))
        .await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = alex
        .get(&format!("/api/inboxes/{}/health", ids.carmen_nightscout))
        .await?;
    assert_eq!(
        (r.status, r.json["kind"].as_str()),
        (StatusCode::OK, Some("nightscout"))
    );
    let r = alex
        .get(&format!("/api/inboxes/{}/health", ids.other_household))
        .await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = alex.get("/api/inboxes/not-a-uuid/health").await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = env.anon().get("/api/inboxes/health").await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn prometheus_format_for_alerting() -> R {
    let env = Env::new(true).await?;
    let ids = setup(&env).await?;
    let mut alex = env.as_user(env.user("alex")).await?;

    let r = alex.get("/api/inboxes/health?format=prometheus").await?;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), "text/plain; version=0.0.4; charset=utf-8");
    let text = String::from_utf8(r.bytes)?;
    let labels = |id: Uuid, kind: &str| format!("{{source=\"{id}\",kind=\"{kind}\"}}");
    let ns = labels(ids.carmen_nightscout, "nightscout");
    let success: f64 = text
        .lines()
        .find_map(|l| l.strip_prefix(&format!("ownhealth_source_last_success_timestamp_seconds{ns} ")))
        .and_then(|v| v.parse().ok())
        .unwrap_or_default();
    let now = chrono::Utc::now().timestamp() as f64;
    assert!((now - success).abs() < 120.0, "last success {success}, now {now}");
    let email = labels(ids.alex_email, "email");
    for want in [
        format!("ownhealth_source_consecutive_failures{email} 2"),
        format!("ownhealth_source_stale{email} 1"),
        format!("ownhealth_source_last_success_timestamp_seconds{email} 0"),
        format!(
            "ownhealth_source_stale{} 0",
            labels(ids.lucia_hae, "health_auto_export")
        ),
    ] {
        assert!(text.lines().any(|l| l == want), "missing {want:?} in\n{text}");
    }
    // labels carry ids and kinds, never the user's words or the profile
    for private in [
        "Carmen",
        "Correo",
        "connection refused",
        &env.profile("alex").to_string(),
    ] {
        assert!(!text.contains(private), "{private} in\n{text}");
    }
    assert!(!text.contains(&ids.other_household.to_string()));

    let r = alex.get("/api/inboxes/health?format=xml").await?;
    assert_eq!((r.status, r.code()), (StatusCode::UNPROCESSABLE_ENTITY, "invalid"));
    Ok(())
}

/// A monitor holds only the source's own token: it reads that source and nothing else.
#[tokio::test]
async fn a_source_token_reads_its_own_health_only() -> R {
    let env = Env::new(true).await?;
    let ids = setup(&env).await?;
    let mut conn = env.pool().acquire().await?;
    let token = sources::issue_token(&mut conn, ids.alex_email).await?;
    let bearer = format!("Bearer {token}");
    let mut monitor = env.anon();
    let get = |path: String| (Method::GET, path);

    let (m, p) = get(format!("/api/inboxes/{}/health?format=prometheus", ids.alex_email));
    let r = monitor.send(m, &p, None, &[("authorization", &bearer)]).await?;
    assert_eq!(r.status, StatusCode::OK);
    let text = String::from_utf8(r.bytes)?;
    assert!(text.contains(&format!(
        "ownhealth_source_stale{{source=\"{}\",kind=\"email\"}} 1",
        ids.alex_email
    )));

    let (m, p) = get(format!("/api/inboxes/{}/health", ids.alex_email));
    let r = monitor.send(m, &p, None, &[("x-ownhealth-secret", &token)]).await?;
    assert_eq!(
        (r.status, r.json["id"].as_str()),
        (StatusCode::OK, Some(ids.alex_email.to_string().as_str()))
    );

    let (m, p) = get("/api/inboxes/health".into());
    let r = monitor.send(m, &p, None, &[("authorization", &bearer)]).await?;
    assert_eq!(ids_of(&r.json), vec![ids.alex_email.to_string()]);

    // another source of the same household: not found, so a token cannot probe ids
    let (m, p) = get(format!("/api/inboxes/{}/health", ids.carmen_nightscout));
    let r = monitor.send(m, &p, None, &[("authorization", &bearer)]).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let mut wrong = token.clone();
    wrong.pop();
    wrong.push(if token.ends_with('A') { 'B' } else { 'A' });
    let (m, p) = get(format!("/api/inboxes/{}/health", ids.alex_email));
    let r = monitor
        .send(m, &p, None, &[("authorization", &format!("Bearer {wrong}"))])
        .await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    sources::update_source(
        &mut conn,
        ids.alex_email,
        &SourcePatch {
            enabled: Some(false),
            ..SourcePatch::default()
        },
    )
    .await?;
    let (m, p) = get(format!("/api/inboxes/{}/health", ids.alex_email));
    let r = monitor.send(m, &p, None, &[("authorization", &bearer)]).await?;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "a disabled source's token stops working"
    );
    Ok(())
}

fn problem(code: &str) -> sources::RunProblem {
    sources::RunProblem {
        code: code.into(),
        message: "attachment 1: synthetic failure".into(),
    }
}

#[tokio::test]
async fn a_run_where_every_item_failed_is_not_a_successful_sync() -> R {
    let env = Env::new(true).await?;
    let mut c = env.pool().acquire().await?;
    let h = env.fam().household_id;
    let id = source(
        &mut c,
        h,
        Some(env.profile("alex")),
        "email",
        "Correo de Alex",
        Some(300),
    )
    .await?;
    let ok = sources::begin_run(&mut c, id, "poll", None).await?;
    let cursor = serde_json::json!({"uid": 7});
    let first = RunOutcome {
        cursor: Some(cursor.clone()),
        ..RunOutcome::default()
    };
    sources::finish_run(&mut c, ok, Ok(&first)).await?;
    // the last success is long ago
    sqlx::query("update source set last_success_at = now() - interval '1 day' where id = $1")
        .bind(id)
        .execute(&mut *c)
        .await?;
    let before: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("select last_success_at from source where id = $1")
            .bind(id)
            .fetch_one(&mut *c)
            .await?;
    let failed = RunOutcome {
        seen: 2,
        problems: vec![problem("message_failed"), problem("message_failed")],
        cursor: Some(serde_json::json!({"uid": 9})),
        ..RunOutcome::default()
    };
    for n in 1..=2 {
        let run = sources::begin_run(&mut c, id, "poll", None).await?;
        sources::finish_run(&mut c, run, Ok(&failed)).await?;
        let status: String = sqlx::query_scalar("select status from source_run where id = $1")
            .bind(run)
            .fetch_one(&mut *c)
            .await?;
        assert_eq!(status, "error");
        let s = sources::get_source(&mut c, id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("gone"))?;
        assert_eq!(s.consecutive_failures, n);
        assert_eq!(s.last_success_at, before);
        assert_eq!(s.cursor, Some(cursor.clone()), "the failed items are retried");
        assert_eq!(
            s.last_error.as_deref(),
            Some("message_failed: attachment 1: synthetic failure")
        );
    }
    let health = sources::source_health(&mut c, h).await?;
    let me = health
        .iter()
        .find(|x| x.id == id)
        .ok_or_else(|| anyhow::anyhow!("no row"))?;
    assert!(me.stale, "{me:?}");
    assert_eq!(
        (me.last_run_status.as_deref(), me.last_run_problems),
        (Some("error"), 2)
    );

    // something arrived: partial, a success, but the last run says so
    let mixed = RunOutcome {
        seen: 3,
        new: 1,
        problems: vec![problem("attachment_skipped")],
        ..RunOutcome::default()
    };
    let run = sources::begin_run(&mut c, id, "poll", None).await?;
    sources::finish_run(&mut c, run, Ok(&mixed)).await?;
    let s = sources::get_source(&mut c, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("gone"))?;
    assert_eq!(s.consecutive_failures, 0);
    assert!(s.last_success_at > before);
    drop(c);

    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.get("/api/inboxes").await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let row = row(&r.json, id);
    assert_eq!(row["last_run_status"], "partial");
    assert_eq!(row["last_run_problems"], 1);
    assert_eq!(
        row["last_run_problem"],
        "attachment_skipped: attachment 1: synthetic failure"
    );
    let r = alex.get("/api/inboxes/health?format=prometheus").await?;
    let text = String::from_utf8(r.bytes)?;
    assert!(
        text.contains(&format!(
            "ownhealth_source_last_run_ok{{source=\"{id}\",kind=\"email\"}} 0"
        )),
        "{text}"
    );
    Ok(())
}
