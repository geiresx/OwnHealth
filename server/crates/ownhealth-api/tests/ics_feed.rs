//! The calendar feed of one profile's due items (migration 1131): a secret URL a calendar app polls
//! with no cookie. Each test runs on its own throwaway database as the synthetic Rivera family.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::{Duration as Wall, Instant};

use axum::body::Body;
use axum::extract::ConnectInfo;
use chrono::{Duration, Months, NaiveDate};
use http::{Request, StatusCode, header};
use icalendar::{Calendar, CalendarComponent, Component, DatePerhapsTime};
use ownhealth_auth::sessions::{COOKIE, create_session, create_session_with};
use ownhealth_auth::testing::unzip;
use ownhealth_core::Runtime;
use ownhealth_core::access::{ProfileIn, create_profile, set_grant, today_utc};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::testing::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

type R = anyhow::Result<()>;

struct App {
    db: TestDb,
    _dir: tempfile::TempDir,
    rt: Runtime,
    family: Family,
}

async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    Ok(App {
        db,
        _dir: dir,
        rt,
        family,
    })
}

/// The address and user agent of the calendar app in these tests (TEST-NET-1, RFC 5737).
const APP_IP: &str = "192.0.2.7";
const APP_UA: &str = "SyntheticCalendar/1.0";

async fn raw(
    a: &App,
    who: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, http::HeaderMap, Vec<u8>)> {
    let session = match who {
        Some(who) => {
            let mut c = a.rt.pool.acquire().await?;
            Some(create_session(&mut c, a.family.user(who), None, None, "passkey").await?)
        }
        None => None,
    };
    request(a, session, method, uri, body).await
}

/// One request with this session cookie, or as a calendar app when there is none.
async fn request(
    a: &App,
    session: Option<String>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, http::HeaderMap, Vec<u8>)> {
    let mut req = Request::builder().method(method).uri(uri);
    req = match session {
        Some(secret) => req
            .header(header::COOKIE, format!("{COOKIE}={secret}"))
            .header(header::CONTENT_TYPE, "application/json"),
        None => req
            .header(header::USER_AGENT, APP_UA)
            .extension(ConnectInfo(SocketAddr::new(APP_IP.parse()?, 443))),
    };
    let req = req.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))?;
    let res = ownhealth_api::router().with_state(a.rt.clone()).oneshot(req).await?;
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 26).await?;
    Ok((status, headers, bytes.to_vec()))
}

async fn send(a: &App, who: &str, method: &str, uri: &str, body: Option<Value>) -> anyhow::Result<(StatusCode, Value)> {
    let (s, _, b) = raw(a, Some(who), method, uri, body).await?;
    Ok((s, serde_json::from_slice(&b).unwrap_or(Value::Null)))
}

/// A new feed: (id, the path the calendar app polls, without `/api`).
async fn new_feed(a: &App, who: &str, p: &str, detail: &str) -> anyhow::Result<(Uuid, String)> {
    let (s, v) = send(
        a,
        who,
        "POST",
        &format!("/profiles/{p}/calendar-feeds"),
        Some(json!({"detail": detail})),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    // the screen puts its own origin in front: the address the person opened the app at
    let path = v["path"]
        .as_str()
        .and_then(|p| p.strip_prefix("/api"))
        .unwrap_or_default()
        .to_string();
    assert!(path.starts_with("/ics/oh_ics_") && path.ends_with(".ics"), "{v}");
    let id = v["feed"]["id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_default();
    Ok((id, path))
}

/// The feed with no cookie, as a calendar app fetches it.
async fn fetch(a: &App, path: &str) -> anyhow::Result<(StatusCode, http::HeaderMap, String)> {
    let (s, h, b) = raw(a, None, "GET", path, None).await?;
    Ok((s, h, String::from_utf8(b)?))
}

async fn ics(a: &App, path: &str) -> anyhow::Result<String> {
    let (s, h, text) = fetch(a, path).await?;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(h[header::CONTENT_TYPE], "text/calendar; charset=utf-8");
    Ok(text)
}

/// The events of a feed, by UID: (start day, summary, description).
fn events(text: &str) -> anyhow::Result<BTreeMap<String, (NaiveDate, String, String)>> {
    // RFC 5545 3.1: CRLF line ends, lines folded at 75 octets
    assert!(
        text.ends_with("\r\n") && !text.replace("\r\n", "").contains('\n'),
        "line ends"
    );
    for line in text.split("\r\n") {
        assert!(line.len() <= 75, "unfolded line: {line}");
    }
    let cal: Calendar = text.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let mut out = BTreeMap::new();
    for c in &cal.components {
        let CalendarComponent::Event(e) = c else { continue };
        let Some(DatePerhapsTime::Date(day)) = e.get_start() else {
            anyhow::bail!("not an all-day event: {:?}", e.get_start());
        };
        let uid = e.get_uid().unwrap_or_default().to_string();
        assert!(e.property_value("DTSTAMP").is_some(), "DTSTAMP");
        let summary = e.get_summary().unwrap_or_default().to_string();
        let description = e.get_description().unwrap_or_default().to_string();
        assert!(out.insert(uid, (day, summary, description)).is_none(), "UID twice");
    }
    Ok(out)
}

fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

const OPEN: [&str; 3] = ["overdue", "due", "upcoming"];

/// The Due screen of one profile in one language: the open items (key, title, due), and the
/// snoozed ones.
async fn open_items(a: &App, who: &str, p: &str, lang: &str) -> anyhow::Result<Vec<Value>> {
    let (s, v) = send(a, who, "GET", &format!("/profiles/{p}/due?lang={lang}"), None).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut out: Vec<Value> = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| OPEN.contains(&i["status"].as_str().unwrap_or_default()))
        .cloned()
        .collect();
    out.extend(
        v["hidden"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["decision"]["kind"] == "snooze" && OPEN.contains(&i["status"].as_str().unwrap_or_default()))
            .cloned(),
    );
    Ok(out)
}

const KEY: &str = "es.cancer.breast";

/// Carmen (71, Spain) has breast cancer screening due; Alex is her caregiver.
#[tokio::test]
async fn the_feed_is_rfc_5545_with_one_all_day_event_per_open_item_and_stable_uids() -> R {
    let a = app().await?;
    let (_, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    // two fetches and the Due list on one UTC day: sampled again when midnight passes in between
    let (today, first, again, open) = loop {
        let today = today_utc();
        let first = events(&ics(&a, &path).await?)?;
        let again = events(&ics(&a, &path).await?)?;
        let open = open_items(&a, "alex", "carmen", "en").await?;
        if today_utc() == today {
            break (today, first, again, open);
        }
    };
    assert!(open.iter().any(|i| i["key"] == KEY), "{open:?}");
    assert_eq!(first.len(), open.len(), "{first:?}");
    for (day, summary, _) in first.values() {
        assert_eq!(summary, "OwnHealth: 1 elemento pendiente");
        // nothing lands where a calendar no longer looks
        assert!(*day >= today, "{day}");
    }
    // the same UIDs on the next fetch, and one per item
    assert_eq!(first.keys().collect::<Vec<_>>(), again.keys().collect::<Vec<_>>());
    let uids: BTreeSet<&String> = first.keys().collect();
    assert_eq!(uids.len(), open.len());
    // each item's event, found by its UID, sits on its due date, or today when that has passed or
    // there is none (a breast screening never recorded is "due" from the day Carmen turned 50)
    let carmen = a.family.profile("carmen");
    for i in &open {
        let want = i["due"]
            .as_str()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .map_or(today, |d| d.max(today));
        let uid = ownhealth_api::routes::ics::uid(carmen, i["key"].as_str().unwrap_or_default());
        assert_eq!(first.get(&uid).map(|e| e.0), Some(want), "{i}");
    }
    assert!(first.values().any(|e| e.0 == today), "{first:?}");
    Ok(())
}

/// Every word an item's title or rule id would put in a calendar, in both languages.
async fn telling_words(a: &App, p: &str) -> anyhow::Result<Vec<String>> {
    let mut words = vec!["vaccine".into(), "vacuna".into(), "screening".into(), "cribado".into()];
    for lang in ["en", "es"] {
        for i in open_items(a, "alex", p, lang).await? {
            words.extend(i["title"].as_str().map(str::to_string));
            words.extend(i["key"].as_str().map(str::to_string));
            words.extend(i["rule"].as_str().map(str::to_string));
        }
    }
    words.sort();
    words.dedup();
    Ok(words)
}

fn leaks(text: &str, words: &[String]) -> Vec<String> {
    let low = text.replace("\r\n ", "").to_lowercase();
    words
        .iter()
        .filter(|w| low.contains(&w.to_lowercase()))
        .cloned()
        .collect()
}

#[tokio::test]
async fn a_generic_feed_names_no_rule_or_vaccine_and_a_named_one_does() -> R {
    let a = app().await?;
    let words = telling_words(&a, "carmen").await?;
    let (id, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    for locale in ["en", "es"] {
        sqlx::query("update app_user set locale = $2 where id = $1")
            .bind(a.family.user("alex"))
            .bind(locale)
            .execute(&a.db.pool)
            .await?;
        let text = ics(&a, &path).await?;
        assert_eq!(leaks(&text, &words), Vec::<String>::new(), "{locale}: {text}");
        let want = if locale == "es" {
            "OwnHealth: 1 elemento pendiente"
        } else {
            "OwnHealth: 1 item due"
        };
        assert!(events(&text)?.values().all(|e| e.1 == want && e.2.is_empty()), "{text}");
    }
    // the positive control: the same check on the named feed finds the names
    let (s, v) = send(
        &a,
        "alex",
        "PATCH",
        &format!("/profiles/carmen/calendar-feeds/{id}"),
        Some(json!({"detail": "named"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["detail"], "named");
    let named = ics(&a, &path).await?;
    assert!(!leaks(&named, &words).is_empty(), "{named}");
    let ev = events(&named)?;
    // Alex reads Spanish (the loop above ends on es)
    let breast = open_items(&a, "alex", "carmen", "es").await?;
    let title = breast
        .iter()
        .find(|i| i["key"] == KEY)
        .and_then(|i| i["title"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        ev.values()
            .any(|e| e.1 == format!("OwnHealth: {title}") && e.2.contains("https://")),
        "{named}"
    );
    Ok(())
}

#[tokio::test]
async fn a_snooze_moves_the_event_and_a_dismissed_item_leaves_the_feed() -> R {
    let a = app().await?;
    let (_, path) = new_feed(&a, "alex", "carmen", "named").await?;
    let before = events(&ics(&a, &path).await?)?;
    let title = |ev: &BTreeMap<String, (NaiveDate, String, String)>, key_title: &str| {
        ev.iter()
            .find(|(_, e)| e.1.ends_with(key_title))
            .map(|(uid, e)| (uid.clone(), e.0))
    };
    let open = open_items(&a, "alex", "carmen", "es").await?;
    let name = |key: &str| {
        open.iter()
            .find(|i| i["key"] == key)
            .and_then(|i| i["title"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let (uid, _) = title(&before, &name(KEY)).expect("breast screening in the feed");

    let until = today_utc() + Duration::days(30);
    let (s, v) = send(
        &a,
        "alex",
        "POST",
        &format!("/profiles/carmen/due/{KEY}/decision"),
        Some(json!({"kind": "snooze", "until": iso(until)})),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let snoozed = events(&ics(&a, &path).await?)?;
    // the same event, on the day the item is back on the Due screen
    assert_eq!(snoozed.get(&uid).map(|e| e.0), Some(until + Duration::days(1)));
    assert_eq!(snoozed.len(), before.len());

    let other = open
        .iter()
        .filter_map(|i| i["key"].as_str())
        .find(|k| *k != KEY)
        .expect("a second open item")
        .to_string();
    let (other_uid, _) = title(&before, &name(&other)).expect("listed");
    let (s, v) = send(
        &a,
        "alex",
        "POST",
        &format!("/profiles/carmen/due/{other}/decision"),
        Some(json!({"kind": "dismiss", "reason": "synthetic reason"})),
    )
    .await?;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let dismissed = events(&ics(&a, &path).await?)?;
    assert!(!dismissed.contains_key(&other_uid), "{dismissed:?}");
    assert_eq!(dismissed.len(), before.len() - 1);
    assert!(!ics(&a, &path).await?.contains("synthetic reason"));
    Ok(())
}

fn median(mut v: Vec<Wall>) -> Wall {
    v.sort();
    v[v.len() / 2]
}

#[tokio::test]
async fn an_unknown_wrong_or_revoked_token_gets_the_same_404_in_the_same_time() -> R {
    let a = app().await?;
    let (id, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    let (_, revoked) = new_feed(&a, "alex", "carmen", "generic").await?;
    let revoked_id = {
        let (_, v) = send(&a, "alex", "GET", "/profiles/carmen/calendar-feeds", None).await?;
        let items = v["items"].as_array().cloned().unwrap_or_default();
        assert_eq!(items.len(), 2, "{v}");
        items
            .iter()
            .filter_map(|f| f["id"].as_str())
            .find(|f| *f != id.to_string())
            .unwrap_or_default()
            .to_string()
    };
    let (s, _) = send(
        &a,
        "alex",
        "DELETE",
        &format!("/profiles/carmen/calendar-feeds/{revoked_id}"),
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = send(&a, "alex", "GET", "/profiles/carmen/calendar-feeds", None).await?;
    assert_eq!(v["items"].as_array().map(Vec::len), Some(1), "{v}");

    // the same prefix with another secret, and a prefix nobody has
    let (prefix, _) = path.trim_end_matches(".ics").split_at("/ics/oh_ics_".len() + 8);
    let wrong = format!("{prefix}_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.ics");
    let unknown = "/ics/oh_ics_zzzzzzzz_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.ics".to_string();
    let cases = [unknown, wrong, revoked, "/ics/nonsense.ics".to_string()];
    let mut bodies = BTreeSet::new();
    for c in &cases {
        let (s, _, body) = fetch(&a, c).await?;
        assert_eq!(s, StatusCode::NOT_FOUND, "{c}");
        bodies.insert(body);
    }
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    // the same work on each refused path: a valid fetch builds the whole document, a refused one
    // only looks up the prefix and compares the hash
    let mut times: Vec<Vec<Wall>> = vec![Vec::new(); 3];
    for _ in 0..15 {
        for (i, c) in cases[..3].iter().enumerate() {
            let t = Instant::now();
            fetch(&a, c).await?;
            times[i].push(t.elapsed());
        }
    }
    let m: Vec<Wall> = times.into_iter().map(median).collect();
    let (lo, hi) = (
        m.iter().min().copied().unwrap_or_default(),
        m.iter().max().copied().unwrap_or_default(),
    );
    assert!(hi <= lo * 3 + Wall::from_millis(3), "refused paths differ: {m:?}");
    Ok(())
}

#[tokio::test]
async fn each_fetch_is_audited_once_and_a_busy_feed_is_slowed_down() -> R {
    let a = app().await?;
    let (id, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    let count = || async {
        sqlx::query_scalar::<_, i64>(
            "select count(*) from audit_event where tool = 'ics' and resource_ids = array[$1::text]",
        )
        .bind(id.to_string())
        .fetch_one(&a.db.pool)
        .await
    };
    assert_eq!(count().await?, 0);
    let text = ics(&a, &path).await?;
    assert_eq!(count().await?, 1);
    ics(&a, &path).await?;
    assert_eq!(count().await?, 2);
    type Row = (
        String,
        Uuid,
        String,
        String,
        Uuid,
        i32,
        i32,
        Option<String>,
        Option<String>,
    );
    let (actor, actor_id, via, action, profile, rows, bytes, ip, ua): Row = sqlx::query_as(
        "select actor_kind, actor_id, via, action, profile_id, rows_out, bytes_out, host(ip), user_agent \
         from audit_event where tool = 'ics' order by id limit 1",
    )
    .fetch_one(&a.db.pool)
    .await?;
    // a calendar app's fetch reads as one, not as the creator reading the profile, and names where
    // it came from, so the owner of an address that leaked can tell who polls it
    assert_eq!(
        (actor.as_str(), actor_id, via.as_str(), action.as_str(), profile),
        (
            "share",
            a.family.user("alex"),
            "export",
            "calendar_feed.fetch",
            a.family.profile("carmen")
        )
    );
    assert_eq!((ip.as_deref(), ua.as_deref()), (Some(APP_IP), Some(APP_UA)));
    assert_eq!(rows as usize, events(&text)?.len());
    assert_eq!(bytes as usize, text.len());
    let (_, v) = send(&a, "alex", "GET", "/profiles/carmen/calendar-feeds", None).await?;
    assert!(v["items"][0]["last_used_at"].is_string(), "{v}");

    // an hour's worth of fetches: the next one waits
    let mut c = a.db.pool.acquire().await?;
    for _ in 0..58 {
        let mut e = AuditEvent::new("share", "export", "calendar_feed.fetch");
        e.tool = Some("ics".into());
        e.profile_id = Some(a.family.profile("carmen"));
        e.resource_type = Some("calendar_feed".into());
        e.resource_ids = Some(vec![id.to_string()]);
        write_audit(&mut c, &e).await?;
    }
    let (s, h, _) = fetch(&a, &path).await?;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert!(h.contains_key(header::RETRY_AFTER));
    assert_eq!(count().await?, 60, "a refused fetch is not a read");
    Ok(())
}

/// A HEAD answers 405 before any lookup: it delivers no calendar, so it takes no fetch from the hour
/// and writes no audit row.
#[tokio::test]
async fn a_head_request_is_refused_and_not_counted_as_a_fetch() -> R {
    let a = app().await?;
    let (_, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    let (s, h, _) = raw(&a, None, "HEAD", &path, None).await?;
    assert_eq!(
        (s, h.get(header::ALLOW).cloned()),
        (StatusCode::METHOD_NOT_ALLOWED, Some("GET".parse()?))
    );
    let n: i64 = sqlx::query_scalar("select count(*) from audit_event where tool = 'ics'")
        .fetch_one(&a.db.pool)
        .await?;
    assert_eq!(n, 0);
    Ok(())
}

/// A wrong secret for a real prefix answers at once while a fetch of that feed holds its row: only a
/// matching secret waits for the lock.
#[tokio::test]
async fn a_wrong_secret_never_waits_on_a_fetch_in_progress() -> R {
    let a = app().await?;
    let (id, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    // the same feed id and secret length, one character of the secret changed
    let at = "/ics/oh_ics_".len() + 9;
    let flip = if &path[at..=at] == "x" { "y" } else { "x" };
    let wrong = format!("{}{flip}{}", &path[..at], &path[at + 1..]);
    assert_eq!((wrong.len(), &wrong[..at]), (path.len(), &path[..at]));
    assert_ne!(wrong, path);
    let mut held = a.db.pool.begin().await?;
    sqlx::query("select 1 from calendar_feed where id = $1 for update")
        .bind(id)
        .execute(&mut *held)
        .await?;
    let got = tokio::time::timeout(Wall::from_secs(3), fetch(&a, &wrong)).await;
    held.rollback().await?;
    assert_eq!(
        got.map(|r| r.map(|f| f.0).ok()).ok().flatten(),
        Some(StatusCode::NOT_FOUND)
    );
    Ok(())
}

/// A fetch that read a named feed and then waited for its row, while its creator made it generic,
/// serves the generic feed: the detail is read again under the lock.
#[tokio::test]
async fn a_feed_made_generic_while_a_fetch_waits_is_served_generic() -> R {
    let a = app().await?;
    let words = telling_words(&a, "carmen").await?;
    let (id, path) = new_feed(&a, "alex", "carmen", "named").await?;
    let pool = &a.db.pool;
    let mut held = pool.begin().await?;
    sqlx::query("select 1 from calendar_feed where id = $1 for update")
        .bind(id)
        .execute(&mut *held)
        .await?;
    let downgrade = async move {
        // the fetch has read the feed and now waits for its row
        let start = Instant::now();
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity where datname = current_database() \
                 and wait_event_type = 'Lock' and query like '%for update of f%'",
            )
            .fetch_one(pool)
            .await?;
            if waiting > 0 {
                break;
            }
            anyhow::ensure!(start.elapsed() < Wall::from_secs(10), "the fetch never waited");
            tokio::time::sleep(Wall::from_millis(20)).await;
        }
        sqlx::query("update calendar_feed set detail = 'generic' where id = $1")
            .bind(id)
            .execute(&mut *held)
            .await?;
        held.commit().await?;
        anyhow::Ok(())
    };
    let (got, changed) = tokio::join!(ics(&a, &path), downgrade);
    changed?;
    let text = got?;
    assert_eq!(leaks(&text, &words), Vec::<String>::new(), "{text}");
    assert!(!text.contains("Carmen"), "{text}");
    Ok(())
}

/// Two calendar apps polling the same address at once, one fetch short of the hour's limit: one gets
/// the feed, the other waits.
#[tokio::test]
async fn two_fetches_at_once_never_pass_the_hourly_limit() -> R {
    let a = app().await?;
    let (id, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    let mut c = a.db.pool.acquire().await?;
    for _ in 0..59 {
        let mut e = AuditEvent::new("share", "export", "calendar_feed.fetch");
        e.tool = Some("ics".into());
        e.profile_id = Some(a.family.profile("carmen"));
        e.resource_type = Some("calendar_feed".into());
        e.resource_ids = Some(vec![id.to_string()]);
        write_audit(&mut c, &e).await?;
    }
    // two open connections in the pool, so neither fetch waits to connect and both start together
    let warm = a.db.pool.acquire().await?;
    drop((c, warm));
    let (one, two) = tokio::join!(fetch(&a, &path), fetch(&a, &path));
    let mut got = vec![one?.0, two?.0];
    got.sort();
    assert_eq!(got, vec![StatusCode::OK, StatusCode::TOO_MANY_REQUESTS]);
    Ok(())
}

#[tokio::test]
async fn the_feed_stops_when_its_creator_loses_access() -> R {
    let a = app().await?;
    let (_, path) = new_feed(&a, "alex", "carmen", "generic").await?;
    ics(&a, &path).await?;
    // a lock on records binds Alex, her caregiver, and lifting it opens the feed again
    let lock = |locks: Vec<&'static str>| {
        sqlx::query("update profile set locks = $2 where id = $1")
            .bind(a.family.profile("carmen"))
            .bind(locks)
            .execute(&a.db.pool)
    };
    lock(vec!["read:records"]).await?;
    assert_eq!(fetch(&a, &path).await?.0, StatusCode::NOT_FOUND);
    lock(vec![]).await?;
    ics(&a, &path).await?;
    sqlx::query("delete from profile_grant where user_id = $1 and profile_id = $2")
        .bind(a.family.user("alex"))
        .bind(a.family.profile("carmen"))
        .execute(&a.db.pool)
        .await?;
    assert_eq!(fetch(&a, &path).await?.0, StatusCode::NOT_FOUND);

    // a disabled login too
    let (_, own) = new_feed(&a, "alex", "alex", "generic").await?;
    ics(&a, &own).await?;
    sqlx::query("update app_user set disabled_at = now() where id = $1")
        .bind(a.family.user("alex"))
        .execute(&a.db.pool)
        .await?;
    assert_eq!(fetch(&a, &own).await?.0, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn making_a_feed_needs_read_records_and_only_its_creator_sees_or_revokes_it() -> R {
    let a = app().await?;
    // Sam views Alex's summary only
    let (s, v) = send(&a, "sam", "POST", "/profiles/alex/calendar-feeds", Some(json!({}))).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = send(
        &a,
        "alex",
        "POST",
        "/profiles/alex/calendar-feeds",
        Some(json!({"detail": "loud"})),
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (id, _) = new_feed(&a, "alex", "lucia", "generic").await?;
    // Sam is Lucía's guardian too, and sees none of Alex's feeds
    let (s, v) = send(&a, "sam", "GET", "/profiles/lucia/calendar-feeds", None).await?;
    assert_eq!((s, &v["items"]), (StatusCode::OK, &json!([])), "{v}");
    let (s, _) = send(
        &a,
        "sam",
        "DELETE",
        &format!("/profiles/lucia/calendar-feeds/{id}"),
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // the plaintext is never listed
    let (_, v) = send(&a, "alex", "GET", "/profiles/lucia/calendar-feeds", None).await?;
    assert!(
        v["items"][0]["prefix"]
            .as_str()
            .is_some_and(|p| p.starts_with("oh_ics_")),
        "{v}"
    );
    assert!(
        v["items"][0].get("url").is_none() && !v.to_string().contains(".ics"),
        "{v}"
    );
    Ok(())
}

/// A session past its step-up window: older than STEP_UP_SECONDS, or opened with a recovery code.
async fn stale(a: &App, who: &str) -> anyhow::Result<String> {
    let mut c = a.rt.pool.acquire().await?;
    Ok(create_session_with(&mut c, a.family.user(who), None, None, "passkey", Some(false)).await?)
}

#[tokio::test]
async fn making_a_feed_and_naming_its_items_need_step_up_and_going_back_or_revoking_does_not() -> R {
    let a = app().await?;
    let (id, _) = new_feed(&a, "alex", "carmen", "generic").await?;
    let feeds = "/profiles/carmen/calendar-feeds";
    let one = format!("{feeds}/{id}");
    let code = |b: &[u8]| serde_json::from_slice::<Value>(b).unwrap_or(Value::Null)["detail"]["code"].clone();
    let (s, _, b) = request(&a, Some(stale(&a, "alex").await?), "POST", feeds, Some(json!({}))).await?;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, json!("step_up_required")));
    let named = Some(json!({"detail": "named"}));
    let (s, _, b) = request(&a, Some(stale(&a, "alex").await?), "PATCH", &one, named.clone()).await?;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, json!("step_up_required")));
    let detail = || async {
        sqlx::query_scalar::<_, String>("select detail from calendar_feed where id = $1")
            .bind(id)
            .fetch_one(&a.db.pool)
            .await
    };
    // the refused call named nothing
    assert_eq!(detail().await?, "generic");
    // named with a fresh step-up, then a stale session can still go back to generic
    let (s, v) = send(&a, "alex", "PATCH", &one, named).await?;
    assert_eq!((s, detail().await?.as_str()), (StatusCode::OK, "named"), "{v}");
    let generic = Some(json!({"detail": "generic"}));
    let (s, _, b) = request(&a, Some(stale(&a, "alex").await?), "PATCH", &one, generic).await?;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let (s, _, b) = request(&a, Some(stale(&a, "alex").await?), "DELETE", &one, None).await?;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    // nothing was made or named on the refused calls
    let rows: Vec<(String, bool)> =
        sqlx::query_as("select detail, revoked_at is not null from calendar_feed where profile_id = $1")
            .bind(a.family.profile("carmen"))
            .fetch_all(&a.db.pool)
            .await?;
    assert_eq!(rows, vec![("generic".to_string(), true)]);
    Ok(())
}

#[tokio::test]
async fn a_person_export_holds_no_feed_secret_or_prefix() -> R {
    let a = app().await?;
    let (id, path) = new_feed(&a, "alex", "alex", "named").await?;
    ics(&a, &path).await?;
    let (prefix, hash): (String, Vec<u8>) =
        sqlx::query_as("select prefix, secret_sha256 from calendar_feed where id = $1")
            .bind(id)
            .fetch_one(&a.db.pool)
            .await?;
    let (s, _, zip) = raw(
        &a,
        Some("alex"),
        "POST",
        "/profiles/alex/export",
        Some(json!({"fmt": "zip"})),
    )
    .await?;
    assert_eq!(s, StatusCode::OK);
    let files = unzip(&zip)?;
    assert!(
        files.keys().any(|n| n == "ownhealth/audit_event.ndjson"),
        "{:?}",
        files.keys()
    );
    for (name, bytes) in &files {
        let text = String::from_utf8_lossy(bytes);
        for needle in ["oh_ics_", "secret_sha256", &prefix, &hex(&hash)] {
            assert!(
                !name.contains(needle) && !text.contains(needle),
                "{name} holds {needle}"
            );
        }
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::test]
async fn deleting_a_profile_removes_its_feeds() -> R {
    let a = app().await?;
    let mut c = a.db.pool.acquire().await?;
    let born = today_utc().checked_sub_months(Months::new(60 * 12)).unwrap_or_default();
    let pid = create_profile(
        &mut c,
        &ProfileIn {
            household_id: a.family.household_id,
            slug: "dana".into(),
            display_name: "Dana Rivera".into(),
            relationship: Some("other".into()),
            birth_date: Some(born),
            sex_at_birth: Some("female".into()),
            country: Some("ES".into()),
            ..ProfileIn::default()
        },
    )
    .await?;
    set_grant(&mut c, a.family.user("alex"), pid, "guardian", None, None, None).await?;
    drop(c);
    let (_, path) = new_feed(&a, "alex", "dana", "generic").await?;
    ics(&a, &path).await?;
    let mut tx = a.db.pool.begin().await?;
    ownhealth_auth::household::delete_profile(&mut tx, pid).await?;
    tx.commit().await?;
    let n: i64 = sqlx::query_scalar("select count(*) from calendar_feed where profile_id = $1")
        .bind(pid)
        .fetch_one(&a.db.pool)
        .await?;
    assert_eq!(n, 0);
    assert_eq!(fetch(&a, &path).await?.0, StatusCode::NOT_FOUND);
    Ok(())
}
