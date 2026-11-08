//! Ported from tests/test_access.py, test_audit.py, test_migrations.py, test_observations.py,
//! test_records.py and test_review.py. Each test runs in a throwaway database.

mod common;

use std::collections::BTreeSet;

use chrono::NaiveDate;
use common::*;
use ownhealth_core::acceptance::{Check, DEFAULT_THRESHOLD};
use ownhealth_core::access::*;
use ownhealth_core::audit::{AuditEvent, list_audit, write_audit};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::migrate::{self, Migration};
use ownhealth_core::observations::*;
use ownhealth_core::provenance::{SrcIn, SrcRef, resolve_src};
use ownhealth_core::records::{
    RecordIn, RecordOptions, active_medications, add_record, get_record, list_records, record_history, retract_record,
    supersede_record,
};
use ownhealth_core::review::{self, CandidateIn, ReviewItemIn, RouteFact};
use ownhealth_core::testing::TestDb;
use ownhealth_core::{Error, demo::Family};
use serde_json::{Value, json};
use sqlx::{Acquire, PgConnection};
use uuid::Uuid;

fn session(f: &Family, who: &str) -> Principal {
    Principal::session(f.user(who), f.household_id)
}

fn code(e: Error) -> String {
    match e {
        Error::AccessDenied { code, .. } => code,
        other => format!("{other:?}"),
    }
}

fn set<const N: usize>(items: [&str; N]) -> Scopes {
    scope_set(items)
}

/// Run `sql` in a savepoint and return its error text (the statement must fail).
async fn fails(conn: &mut PgConnection, sql: &str, id: Uuid) -> String {
    let Ok(mut sp) = conn.begin().await else {
        return "no savepoint".into();
    };
    let r = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id).execute(&mut *sp).await;
    let _ = sp.rollback().await;
    r.err().map(|e| e.to_string()).unwrap_or_default()
}

// ------------------------------------------------------------------ access ----

#[tokio::test]
async fn family_grants() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let perms: std::collections::HashMap<String, Permission> = accessible_profiles(&mut c, &session(&f, "alex"))
        .await?
        .into_iter()
        .map(|p| (p.profile.slug.clone(), p))
        .collect();
    assert_eq!(
        perms.keys().cloned().collect::<BTreeSet<_>>(),
        ["alex", "carmen", "lucia", "sam"].map(String::from).into()
    );
    assert!(perms["alex"].role == "owner" && perms["alex"].scopes == all_scopes());
    assert_eq!(perms["sam"].scopes, set(["read:summary"]));
    assert_eq!(
        perms["carmen"].scopes,
        set([
            "read:summary",
            "read:labs",
            "read:vitals",
            "read:records",
            "write:propose"
        ])
    );
    assert!(
        perms["lucia"]
            .profile
            .is_minor(NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default(), 18)
    );
    let sam = session(&f, "sam");
    let slugs: BTreeSet<String> = accessible_profiles(&mut c, &sam)
        .await?
        .into_iter()
        .map(|p| p.profile.slug)
        .collect();
    assert_eq!(slugs, ["alex", "lucia", "sam"].map(String::from).into());
    let e = effective_permission(&mut c, &sam, f.profile("carmen")).await.err();
    assert_eq!(e.map(code).as_deref(), Some("profile_not_granted"));
    Ok(())
}

#[tokio::test]
async fn locks_bind_everyone_but_the_owner_and_tokens_limit() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    set_grant(
        &mut c,
        f.user("sam"),
        f.profile("alex"),
        "viewer",
        Some(&set(["read:summary", "read:sensitive"])),
        None,
        None,
    )
    .await?;
    set_locks(&mut c, f.profile("alex"), &set(["read:sensitive"])).await?;
    assert_eq!(
        effective_permission(&mut c, &session(&f, "sam"), f.profile("alex"))
            .await?
            .scopes,
        set(["read:summary"])
    );
    assert!(
        effective_permission(&mut c, &session(&f, "alex"), f.profile("alex"))
            .await?
            .has("read:sensitive")
    );

    let token = Principal {
        kind: PrincipalKind::Token,
        scopes: set(["read:summary", "read:labs"]),
        profile_ids: Some([f.profile("alex")].into()),
        default_profile: Some(f.profile("alex")),
        ..Principal::session(f.user("alex"), f.household_id)
    };
    assert_eq!(
        resolve_profile_ref(&mut c, &token, ProfileRef::Default).await?,
        f.profile("alex")
    );
    let perm = require(&mut c, &token, "alex".into(), Some("read:labs"), None).await?;
    assert_eq!(perm.scopes, set(["read:summary", "read:labs"]));
    let e = require(&mut c, &token, "alex".into(), Some("read:genome"), None)
        .await
        .err();
    assert_eq!(e.map(code).as_deref(), Some("insufficient_scope"));
    match require(&mut c, &token, "carmen".into(), None, None).await {
        Err(Error::AccessDenied { code, allowed, .. }) => {
            assert_eq!(code, "profile_not_granted");
            assert_eq!(allowed, vec!["alex".to_string()]);
        }
        other => panic!("want profile_not_granted, got {other:?}"),
    }
    let alex = session(&f, "alex");
    assert_eq!(
        resolve_profile_ref(&mut c, &alex, "me".into()).await?,
        f.profile("alex")
    );
    assert!(
        require(&mut c, &alex, "sam".into(), None, Some("import"))
            .await
            .is_err()
    );
    assert_eq!(
        require(&mut c, &alex, "carmen".into(), None, Some("import"))
            .await?
            .role,
        "caregiver"
    );
    Ok(())
}

#[tokio::test]
async fn an_ungranted_uuid_reads_like_an_unknown_one() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let sam = session(&f, "sam");
    let token = Principal {
        kind: PrincipalKind::Token,
        scopes: all_scopes(),
        profile_ids: Some([f.profile("alex")].into()),
        ..Principal::session(f.user("alex"), f.household_id)
    };
    let mut messages = BTreeSet::new();
    for (who, pid) in [
        (&sam, f.profile("carmen")),
        (&token, f.profile("sam")),
        (&sam, Uuid::new_v4()),
    ] {
        match effective_permission(&mut c, who, pid).await {
            Err(e @ Error::AccessDenied { .. }) => {
                messages.insert(e.to_string());
                assert_eq!(code(e), "profile_not_granted");
            }
            other => panic!("want AccessDenied, got {other:?}"),
        }
    }
    assert_eq!(messages.len(), 1);
    assert!(messages.iter().all(|m| !m.contains("carmen")));
    Ok(())
}

// ------------------------------------------------------------------- audit ----

#[tokio::test]
async fn audit_is_append_only_and_bad_ips_are_null() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let mut e = AuditEvent::new("user", "ui", "read");
    e.actor_id = Some(f.user("alex"));
    e.profile_id = Some(f.profile("alex"));
    e.src_ids = Some(vec!["src_a12".into()]);
    e.rows_out = Some(1);
    let id = write_audit(&mut c, &e).await?;
    let rows = list_audit(&mut c, Some(&[f.profile("alex")]), None, 100, None).await?;
    assert_eq!(rows[0]["id"], json!(id));
    assert_eq!(rows[0]["src_ids"], json!(["src_a12"]));
    for stmt in [
        "update audit_event set action = 'x' where id = $1",
        "delete from audit_event where id = $1",
    ] {
        let mut sp = c.begin().await?;
        let r = sqlx::query(stmt).bind(id).execute(&mut *sp).await;
        sp.rollback().await?;
        assert!(r.err().is_some_and(|e| e.to_string().contains("append-only")), "{stmt}");
    }
    let mut sp = c.begin().await?;
    let r = sqlx::query("truncate audit_event").execute(&mut *sp).await;
    sp.rollback().await?;
    assert!(r.err().is_some_and(|e| e.to_string().contains("append-only")));
    for (raw, stored) in [
        ("testclient", None),
        ("1.2.3.4, 5.6.7.8", None),
        (" 10.0.0.1 ", Some("10.0.0.1")),
        ("::1", Some("::1")),
    ] {
        let mut e = AuditEvent::new("user", "api", "read");
        e.ip = Some(raw.into());
        let id = write_audit(&mut c, &e).await?;
        let got: Option<String> = sqlx::query_scalar("select host(ip) from audit_event where id = $1")
            .bind(id)
            .fetch_one(&mut *c)
            .await?;
        assert_eq!(got.as_deref(), stored, "{raw}");
    }
    Ok(())
}

// -------------------------------------------------------------- migrations ----

#[tokio::test]
async fn migrations_are_idempotent_and_refuse_newer_or_edited_schemas() -> R {
    let db = TestDb::new().await?;
    let shipped = migrate::embedded()?;
    let mut c = db.pool.acquire().await?;
    assert!(migrate::pending(&mut c, &shipped).await?.is_empty());
    let versions: Vec<i32> = sqlx::query_scalar("select version from schema_migrations order by version")
        .fetch_all(&mut *c)
        .await?;
    assert_eq!(versions[0], 1);
    drop(c);
    assert!(migrate::migrate(&db.pool, &shipped).await?.is_empty());

    let future = |sql: &str| Migration {
        version: 9999,
        name: "future".into(),
        sql: sql.into(),
        checksum: hex::encode(<sha2::Sha256 as sha2::Digest>::digest(sql.as_bytes())),
    };
    let mut newer = shipped.clone();
    newer.push(future("create table future_thing (id int);"));
    assert!(migrate::migrate(&db.pool, &newer).await?.contains(&9999));
    let e = migrate::migrate(&db.pool, &shipped)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("newer than the code"), "{e}");
    let mut edited = shipped.clone();
    edited.push(future("create table future_thing (id bigint);"));
    let e = migrate::migrate(&db.pool, &edited)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("forward-only"), "{e}");
    Ok(())
}

// ------------------------------------------------------------ observations ----

#[tokio::test]
async fn a_document_value_keeps_its_page_and_bbox() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (doc, boxes) = lab_document(&mut c, &blobs, &f, "alex").await?;
    let partial = SrcIn {
        document_id: Some(doc),
        page_no: Some(1),
        ..SrcIn::new("document_region")
    };
    assert!(
        partial
            .validate()
            .err()
            .is_some_and(|e| { e.to_string().contains("needs document_id, page_no and bbox") })
    );
    assert!(
        SrcIn::new("fhir_import")
            .validate()
            .err()
            .is_some_and(|e| e.to_string().contains("needs import_ref"))
    );
    let unknown = commit_observation(
        &mut c,
        &obs(f.profile("alex"), "97"),
        &"src_unknown".into(),
        CommitOptions::new("auto"),
    )
    .await;
    assert!(matches!(unknown, Err(Error::Provenance(_))));
    let src = SrcIn {
        document_id: Some(doc),
        page_no: Some(1),
        bbox: Some(boxes[0]),
        ..SrcIn::new("document_region")
    };
    let o = ObservationIn {
        document_id: Some(doc),
        ..obs(f.profile("alex"), "97")
    };
    let committed = commit_observation(&mut c, &o, &src.into(), CommitOptions::new("auto")).await?;
    let row = get_observation(&mut c, committed.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert!(row.src_id == committed.src_id && row.status == "current");
    assert_eq!(row.fhir["resourceType"], "Observation");
    assert_eq!(row.fhir["code"]["coding"][0]["code"], "13457-7");
    let resolved = resolve_src(&mut c, &committed.src_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no src"))?;
    assert_eq!(resolved.profile_id, Some(f.profile("alex")));
    assert_eq!(
        resolved.document.as_ref().map(|d| d["id"].clone()),
        Some(json!(doc.to_string()))
    );
    let bbox: Vec<f64> = serde_json::from_value(resolved.src["bbox"].clone())?;
    assert_eq!(bbox, boxes[0].to_vec());
    Ok(())
}

#[tokio::test]
async fn rows_are_immutable_and_corrections_supersede() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let p = f.profile("alex");
    let glucose = |v: &str| obs_with(p, v, "glucose", "2345-7", "2026-09-14");
    let first = commit_observation(
        &mut c,
        &glucose("410"),
        &manual_src().into(),
        CommitOptions::new("person"),
    )
    .await?;
    let e = fails(
        &mut c,
        "update observation set printed_value = '140' where id = $1",
        first.id,
    )
    .await;
    assert!(e.contains("immutable"), "{e}");
    let fixed = supersede_observation(
        &mut c,
        first.id,
        &glucose("140"),
        &manual_src().into(),
        "person",
        "OCR misread, confirmed 140",
        None,
    )
    .await?;
    let old = get_observation(&mut c, first.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert!(old.status == "superseded" && old.status_reason.as_deref() == Some("OCR misread, confirmed 140"));
    assert_eq!(
        get_observation(&mut c, fixed.id).await?.and_then(|o| o.supersedes),
        Some(first.id)
    );
    let hist: Vec<Value> = observation_history(&mut c, fixed.id)
        .await?
        .into_iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(hist, vec![json!(first.id.to_string()), json!(fixed.id.to_string())]);
    let q = ObservationQuery {
        alias_group: Some("glucose".into()),
        ..Default::default()
    };
    let current: Vec<String> = list_observations(&mut c, p, &q)
        .await?
        .into_iter()
        .map(|o| o.printed_value)
        .collect();
    assert_eq!(current, vec!["140"]);
    let again = supersede_observation(
        &mut c,
        first.id,
        &glucose("150"),
        &manual_src().into(),
        "person",
        "again",
        None,
    )
    .await;
    assert!(matches!(again, Err(Error::Lifecycle(_))));
    let e = fails(
        &mut c,
        "update observation set status = 'current' where id = $1",
        first.id,
    )
    .await;
    assert!(e.contains("not allowed"), "{e}");
    Ok(())
}

#[tokio::test]
async fn as_of_sees_what_the_store_knew_and_provisional_confirms() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let p = f.profile("alex");
    let a = commit_observation(
        &mut c,
        &obs(p, "101"),
        &manual_src().into(),
        CommitOptions::new("person"),
    )
    .await?;
    let between: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("select clock_timestamp()")
        .fetch_one(&mut *c)
        .await?;
    retract_observation(&mut c, a.id, "entered in error").await?;
    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("select clock_timestamp()")
        .fetch_one(&mut *c)
        .await?;
    let ldl = |as_of| ObservationQuery {
        alias_group: Some("ldl_c".into()),
        as_of,
        ..Default::default()
    };
    assert!(list_observations(&mut c, p, &ldl(None)).await?.is_empty());
    assert_eq!(
        list_observations(&mut c, p, &ldl(Some(between)))
            .await?
            .iter()
            .map(|o| o.id)
            .collect::<Vec<_>>(),
        vec![a.id]
    );
    assert!(list_observations(&mut c, p, &ldl(Some(after))).await?.is_empty());
    assert!(matches!(
        retract_observation(&mut c, a.id, "twice").await,
        Err(Error::Lifecycle(_))
    ));

    let prov = commit_observation(
        &mut c,
        &obs(p, "97"),
        &manual_src().into(),
        CommitOptions {
            status: "provisional",
            ..CommitOptions::new("person")
        },
    )
    .await?;
    assert!(
        list_observations(&mut c, p, &ObservationQuery::default())
            .await?
            .is_empty()
    );
    assert!(matches!(
        retract_observation(&mut c, prov.id, "  ").await,
        Err(Error::Lifecycle(_))
    ));
    confirm_observation(&mut c, prov.id).await?;
    assert_eq!(
        latest_per_group(&mut c, p, None, None)
            .await?
            .iter()
            .map(|o| o.id)
            .collect::<Vec<_>>(),
        vec![prov.id]
    );
    assert!(matches!(
        confirm_observation(&mut c, prov.id).await,
        Err(Error::Lifecycle(_))
    ));
    Ok(())
}

#[tokio::test]
async fn effective_at_must_be_aware_and_srcs_stay_on_their_profile() -> R {
    let mut naive = json!({"profile_id": Uuid::nil(), "printed_name": "x", "printed_value": "1", "alias_group": "g", "derivation_version": "t"});
    naive["effective_at"] = json!("2026-01-01T00:00:00");
    assert!(ObservationIn::from_json(naive).is_err());

    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (alex, sam) = (f.profile("alex"), f.profile("sam"));
    let first = commit_observation(
        &mut c,
        &obs(alex, "97"),
        &manual_src().into(),
        CommitOptions::new("person"),
    )
    .await?;
    let mut sp = c.begin().await?;
    let cross = commit_observation(
        &mut sp,
        &obs(sam, "98"),
        &SrcRef::Id(first.src_id.clone()),
        CommitOptions::new("person"),
    )
    .await;
    sp.rollback().await?;
    assert!(
        matches!(&cross, Err(Error::Provenance(m)) if m.contains("another profile")),
        "{cross:?}"
    );
    let again = commit_observation(
        &mut c,
        &obs_with(alex, "99", "ldl_c", "13457-7", "2026-10-01"),
        &SrcRef::Id(first.src_id.clone()),
        CommitOptions::new("person"),
    )
    .await?;
    assert_eq!(again.src_id, first.src_id);
    let foreign = SrcIn {
        profile_id: Some(alex),
        ..manual_src()
    };
    let mut sp = c.begin().await?;
    let minted = commit_observation(&mut sp, &obs(sam, "98"), &foreign.into(), CommitOptions::new("person")).await;
    sp.rollback().await?;
    assert!(
        matches!(&minted, Err(Error::Provenance(m)) if m.contains("belongs to the profile")),
        "{minted:?}"
    );
    Ok(())
}

// ----------------------------------------------------------------- records ----

fn record(kind: &str, v: Value) -> RecordIn {
    RecordIn::from_json(kind, v).unwrap_or_else(|e| panic!("bad test record: {e}"))
}

#[tokio::test]
async fn medications_lifecycle_and_active_on_date() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let p = f.profile("alex");
    let src: SrcRef = manual_src().into();
    let old = add_record(&mut c, &record("medication", json!({"profile_id": p, "name": "Atorvastatin", "strength": "20 mg", "started": "2022-06-20", "stopped": "2025-12-01", "med_status": "stopped"})), &src, RecordOptions::new("person")).await?;
    let new = add_record(
        &mut c,
        &record(
            "medication",
            json!({"profile_id": p, "name": "Atorvastatin", "strength": "20 mg", "started": "2026-03-10"}),
        ),
        &src,
        RecordOptions::new("person"),
    )
    .await?;
    add_record(
        &mut c,
        &record(
            "medication",
            json!({"profile_id": p, "name": "Omeprazole", "strength": "20 mg", "started": "2026-04-01"}),
        ),
        &src,
        RecordOptions::new("person"),
    )
    .await?;
    let on = |y, m, d| NaiveDate::from_ymd_opt(y, m, d);
    let names = |rows: Vec<serde_json::Map<String, Value>>| -> Vec<(String, String)> {
        rows.iter()
            .map(|r| {
                (
                    r["name"].as_str().unwrap_or_default().into(),
                    r["started"].as_str().unwrap_or_default().into(),
                )
            })
            .collect()
    };
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        names(active_medications(&mut c, p, on(2026, 11, 7), None, None, true).await?),
        vec![pair("Atorvastatin", "2026-03-10"), pair("Omeprazole", "2026-04-01")]
    );
    assert_eq!(
        names(active_medications(&mut c, p, on(2024, 1, 1), None, None, true).await?),
        vec![pair("Atorvastatin", "2022-06-20")]
    );
    assert!(
        active_medications(&mut c, p, on(2026, 1, 15), None, None, true)
            .await?
            .is_empty()
    );
    let row = get_record(&mut c, "medication", new.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert_eq!(row["fhir"]["resourceType"], "MedicationStatement");
    assert_eq!(row["src_id"], json!(new.src_id));
    assert_ne!(old.id, new.id);
    Ok(())
}

#[tokio::test]
async fn condition_correction_and_retraction() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let p = f.profile("alex");
    let src: SrcRef = manual_src().into();
    let c1 = add_record(
        &mut c,
        &record(
            "condition",
            json!({"profile_id": p, "text": "Hypercholesterolemia", "verification_status": "provisional"}),
        ),
        &src,
        RecordOptions::new("person"),
    )
    .await?;
    let fixed = record(
        "condition",
        json!({"profile_id": p, "text": "Hypercholesterolaemia", "code_system": "icd10", "code": "E78.0", "verification_status": "confirmed"}),
    );
    let c2 = supersede_record(&mut c, c1.id, &fixed, &src, "person", "confirmed by lipid clinic").await?;
    let ids = |rows: Vec<serde_json::Map<String, Value>>| rows.into_iter().map(|r| r["id"].clone()).collect::<Vec<_>>();
    assert_eq!(
        ids(record_history(&mut c, "condition", c2.id).await?),
        vec![json!(c1.id.to_string()), json!(c2.id.to_string())]
    );
    assert_eq!(
        ids(list_records(&mut c, "condition", p, None, true).await?),
        vec![json!(c2.id.to_string())]
    );
    retract_record(&mut c, "condition", c2.id, "entered on the wrong profile").await?;
    assert!(list_records(&mut c, "condition", p, None, true).await?.is_empty());
    assert!(matches!(
        retract_record(&mut c, "condition", c2.id, "again").await,
        Err(Error::Lifecycle(_))
    ));
    let moved = record("condition", json!({"profile_id": f.profile("sam"), "text": "x"}));
    assert!(matches!(
        supersede_record(&mut c, c1.id, &moved, &src, "person", "move").await,
        Err(Error::Lifecycle(_))
    ));
    Ok(())
}

// ------------------------------------------------------------------ review ----

async fn run(c: &mut PgConnection, doc: Uuid) -> anyhow::Result<Uuid> {
    Ok(review::create_extraction_run(c, doc, "rules:test", "1", b"t", None, None).await?)
}

fn cand(f: &Family, bbox: [f64; 4], conf: f64, checks: Vec<Check>, value: &str) -> CandidateIn {
    CandidateIn {
        page_no: 1,
        bbox,
        cells: json!({"value": {"text": value}}),
        row_conf: conf,
        checks,
        observation: obs(f.profile("alex"), value),
    }
}

async fn submit(
    c: &mut PgConnection,
    run: Uuid,
    cand: &CandidateIn,
    allowance: &str,
    identity: &str,
) -> anyhow::Result<review::RouteResult> {
    Ok(review::submit_candidate(c, run, cand, allowance, identity, DEFAULT_THRESHOLD, false).await?)
}

#[tokio::test]
async fn review_routes_accepts_rejects_and_reopens() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (doc, boxes) = lab_document(&mut c, &blobs, &f, "alex").await?;
    let r = run(&mut c, doc).await?;

    let res = submit(
        &mut c,
        r,
        &cand(&f, boxes[0], 0.99, vec![], "97"),
        "template_text",
        "matched",
    )
    .await?;
    assert!(res.state == "auto_accepted" && res.review_item_id.is_none());
    let row = get_observation(&mut c, res.observation_id.unwrap_or_default())
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert!(row.accepted_by == "auto" && row.document_id == Some(doc) && row.candidate_row_id.is_some());
    let (page, bbox, kind): (i32, Vec<f32>, String) =
        sqlx::query_as("select page_no, bbox, kind from src where id = $1")
            .bind(&res.src_id)
            .fetch_one(&mut *c)
            .await?;
    assert!(page == 1 && kind == "document_region");
    assert_eq!(
        bbox.iter().map(|v| *v as f64).collect::<Vec<_>>(),
        boxes[0].iter().map(|v| *v as f32 as f64).collect::<Vec<_>>()
    );

    let bad = vec![Check::fail("units", "unit_dimension_mismatch")];
    let held = submit(
        &mut c,
        r,
        &cand(&f, boxes[0], 0.99, bad, "97"),
        "template_text",
        "matched",
    )
    .await?;
    assert!(held.state == "in_review" && held.reasons == vec!["unit_dimension_mismatch"]);
    let item_id = held.review_item_id.unwrap_or_default();
    let item = review::get_item(&mut c, item_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no item"))?;
    assert!(item.status == "open" && item.payload["checks"][0]["reason"] == "unit_dimension_mismatch");
    let edits = json!({"value_canonical": "2.51", "unit_canonical": "mmol/L"});
    let created = review::accept(&mut c, item_id, f.user("alex"), Some(&edits)).await?;
    let row = get_observation(&mut c, created[0])
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing"))?;
    assert!(row.accepted_by == "person" && row.reviewed_by == Some(f.user("alex")));
    assert_eq!(row.value_canonical.map(|d| d.to_string()).as_deref(), Some("2.51"));
    assert_eq!(row.printed_value, "97");
    assert!(matches!(
        review::accept(&mut c, item_id, f.user("alex"), None).await,
        Err(Error::Review(_))
    ));

    let low = submit(
        &mut c,
        r,
        &cand(&f, boxes[0], 0.5, vec![], "97"),
        "rules_text",
        "matched",
    )
    .await?;
    let low_id = low.review_item_id.unwrap_or_default();
    assert!(matches!(
        review::reject(&mut c, low_id, f.user("alex"), " ").await,
        Err(Error::Review(_))
    ));
    review::reject(&mut c, low_id, f.user("alex"), "not my result").await?;
    assert_eq!(
        review::get_item(&mut c, low_id)
            .await?
            .and_then(|i| i.decision_reason)
            .as_deref(),
        Some("not my result")
    );
    review::reopen(&mut c, low_id, f.user("alex")).await?;
    assert_eq!(
        review::get_item(&mut c, low_id).await?.map(|i| i.status).as_deref(),
        Some("open")
    );
    Ok(())
}

#[tokio::test]
async fn bulk_accept_only_clean_rows_above_threshold() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (doc, boxes) = lab_document(&mut c, &blobs, &f, "alex").await?;
    let r = run(&mut c, doc).await?;
    let low = submit(
        &mut c,
        r,
        &cand(&f, boxes[0], 0.91, vec![], "97"),
        "rules_text",
        "matched",
    )
    .await?;
    let lower = submit(
        &mut c,
        r,
        &cand(&f, boxes[1], 0.80, vec![], "24"),
        "rules_text",
        "matched",
    )
    .await?;
    let failed = submit(
        &mut c,
        r,
        &cand(&f, boxes[1], 0.99, vec![Check::fail("g", "not_grounded")], "25"),
        "rules_text",
        "matched",
    )
    .await?;
    let accepted = review::bulk_accept(&mut c, doc, f.user("alex"), 0.90).await?;
    assert_eq!(accepted, vec![low.review_item_id.unwrap_or_default()]);
    for held in [lower, failed] {
        assert_eq!(
            review::get_item(&mut c, held.review_item_id.unwrap_or_default())
                .await?
                .map(|i| i.status)
                .as_deref(),
            Some("open")
        );
    }
    assert_eq!(review::count_open(&mut c, f.household_id, None).await?, 2);
    Ok(())
}

#[tokio::test]
async fn identity_hints_are_not_enough_and_kinds_without_a_handler_are_dismissed() -> R {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path())?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let (doc, boxes) = lab_document(&mut c, &blobs, &f, "alex").await?;
    let r = run(&mut c, doc).await?;
    assert_eq!(
        submit(
            &mut c,
            r,
            &cand(&f, boxes[0], 0.99, vec![], "97"),
            "template_text",
            "hinted"
        )
        .await?
        .state,
        "in_review"
    );
    let item = review::create_item(
        &mut c,
        &ReviewItemIn {
            kind: "restore_check_failed".into(),
            household_id: Some(f.household_id),
            payload: json!({}),
            ..Default::default()
        },
    )
    .await?;
    let e = review::accept(&mut c, item, f.user("alex"), None)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("dismiss"), "{e}");
    review::dismiss(&mut c, item, f.user("alex"), Some("seen")).await?;
    assert_eq!(
        review::get_item(&mut c, item).await?.map(|i| i.status).as_deref(),
        Some("dismissed")
    );
    Ok(())
}

#[tokio::test]
async fn an_edit_keeps_the_received_resource_in_step_with_the_columns() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let received = json!({
        "resourceType": "Observation", "id": "upstream-1", "status": "final",
        "code": {"coding": [{"system": "http://loinc.org", "code": "13457-7"}], "text": "LDL"},
        "valueQuantity": {"value": 97, "unit": "mg/dL"}, "meta": {"source": "synthetic"},
    });
    let o = ObservationIn {
        fhir: Some(received.clone()),
        ..obs(f.profile("alex"), "97")
    };
    let src = manual_src();
    let checks = [Check::fail("units", "unit_dimension_mismatch")];
    let res = review::route_fact(
        &mut c,
        RouteFact::new(&o, &src, &checks, 0.99, "structured_valid", "matched"),
    )
    .await?;
    let edits =
        json!({"value_num": "79", "printed_value": "79", "fhir": {"resourceType": "Observation", "valueString": "x"}});
    let created = review::accept(
        &mut c,
        res.review_item_id.unwrap_or_default(),
        f.user("alex"),
        Some(&edits),
    )
    .await?;
    let resource: Value = sqlx::query_scalar("select fhir from observation where id = $1")
        .bind(created[0])
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(resource["valueQuantity"]["value"], json!(79));
    assert!(resource.get("valueString").is_none());
    assert_eq!(resource["id"], "upstream-1");
    assert_eq!(resource["meta"], json!({"source": "synthetic"}));
    assert_eq!(resource["code"], received["code"]);
    Ok(())
}

#[tokio::test]
async fn edits_cannot_move_a_fact() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    let o = obs(f.profile("alex"), "97");
    let src = manual_src();
    let checks = [Check::fail("g", "not_grounded")];
    let res = review::route_fact(&mut c, RouteFact::new(&o, &src, &checks, 0.99, "rules_text", "matched")).await?;
    let edits = json!({"profile_id": f.profile("sam")});
    let e = review::accept(
        &mut c,
        res.review_item_id.unwrap_or_default(),
        f.user("alex"),
        Some(&edits),
    )
    .await;
    assert!(matches!(&e, Err(Error::Review(m)) if m.contains("profile_id")), "{e:?}");
    Ok(())
}
