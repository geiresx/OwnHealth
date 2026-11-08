//! The export route and hard deletion of a profile and an account
//! (`tests/security/test_export_delete.py`; the zip's contents are tested with `export_profile`).

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeSet;

use axum::http::StatusCode;
use ownhealth_auth::testing::{sha256_hex, unzip};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation, retract_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use serde_json::{Value, json};
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

fn obs(profile_id: Uuid, value: &str, day: &str) -> anyhow::Result<ObservationIn> {
    Ok(ObservationIn::from_json(json!({
        "profile_id": profile_id, "printed_name": "Colesterol LDL", "printed_value": value,
        "printed_unit": "mg/dL", "printed_ref_range": "< 130", "value_num": value, "ref_high": "130",
        "loinc": "13457-7", "alias_group": "ldl_c", "unit_ucum": "mg/dL", "value_canonical": value,
        "unit_canonical": "mg/dL", "derivation_version": "test", "effective_at": format!("{day}T08:00:00+00:00"),
    }))?)
}

/// A LabNorte PDF on `slug` (unique bytes unless `value` is given), a manual value retracted later,
/// and a read by Sam. Returns the blob hash.
async fn seed(env: &Env, slug: &str, value: Option<&str>) -> anyhow::Result<String> {
    let random = ownhealth_core::ids::token_hex(3);
    let (pdf, _) = lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id: "LN-T",
        pages: vec![vec![
            LabRow::new("Colesterol LDL", "97", "mg/dL", "< 130"),
            LabRow::new("Vitamina D", value.unwrap_or(&random), "ng/mL", "30 - 100").flagged("L"),
        ]],
    });
    let sha = env.rt.blobs.put_bytes(&pdf)?;
    let mut c = env.pool().acquire().await?;
    let pid = env.profile(slug);
    create_document(
        &mut c,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            ..NewDocument::upload(
                env.fam().household_id,
                Some(pid),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?;
    let manual = SrcRef::New(SrcIn::manual(Some(env.user("alex"))));
    let m = commit_observation(
        &mut c,
        &obs(pid, "101", "2026-10-01")?,
        &manual,
        CommitOptions::new("person"),
    )
    .await?;
    retract_observation(&mut c, m.id, "typed the wrong value").await?;
    commit_observation(
        &mut c,
        &obs(pid, "97", "2026-10-02")?,
        &manual,
        CommitOptions::new("person"),
    )
    .await?;
    write_audit(
        &mut c,
        &AuditEvent {
            actor_id: Some(env.user("sam")),
            profile_id: Some(pid),
            ..AuditEvent::new("user", "ui", "read")
        },
    )
    .await?;
    Ok(sha)
}

fn tmp_exports(env: &Env) -> usize {
    std::fs::read_dir(env.settings.data_dir.join("tmp"))
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("export-"))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn export_zip() -> R {
    let env = Env::new(true).await?;
    let sha = seed(&env, "alex", None).await?;
    let mut c = env.pool().acquire().await?;
    let sam_src = SrcRef::New(SrcIn::manual(Some(env.user("sam"))));
    commit_observation(
        &mut c,
        &obs(env.profile("sam"), "55", "2026-09-14")?,
        &sam_src,
        CommitOptions::new("person"),
    )
    .await?;
    drop(c);
    let body = json!({"fmt": "zip"});
    let r = env
        .stale(env.user("alex"))
        .await?
        .post("/api/profiles/alex/export", &body)
        .await?;
    assert_eq!(r.code(), "step_up_required");
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.post("/api/profiles/alex/export", &json!({"fmt": "tar"})).await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    let r = alex.post("/api/profiles/alex/export", &body).await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.header("content-type"), "application/zip");
    assert_eq!(r.header("cache-control"), "no-store");
    let today = chrono::Local::now().date_naive().format("%Y-%m-%d");
    assert_eq!(
        r.header("content-disposition"),
        format!("attachment; filename=\"ownhealth-alex-{today}.zip\"")
    );
    assert_eq!(r.header("content-length"), r.bytes.len().to_string());
    let z = unzip(&r.bytes)?;
    let manifest: Value = serde_json::from_slice(z.get("export-manifest.json").map_or(&[][..], Vec::as_slice))?;
    assert_eq!(
        (&manifest["format"], &manifest["profile"]["slug"]),
        (&json!("ownhealth.export/v1"), &json!("alex"))
    );
    for f in manifest["files"].as_array().into_iter().flatten() {
        let path = f["path"].as_str().unwrap_or_default();
        assert_eq!(
            Some(sha256_hex(z.get(path).map_or(&[][..], Vec::as_slice))),
            f["sha256"].as_str().map(str::to_string),
            "{path}"
        );
    }
    let names: BTreeSet<&str> = z.keys().map(String::as_str).collect();
    for n in [
        "fhir/Patient.ndjson",
        "fhir/Observation.ndjson",
        "fhir/DocumentReference.ndjson",
        "ownhealth/audit_event.ndjson",
    ] {
        assert!(names.contains(n), "{n}");
    }
    // the original upload, byte for byte
    let original = names
        .iter()
        .find(|n| n.starts_with(&format!("files/{sha}/")))
        .copied()
        .unwrap_or_default();
    assert_eq!(sha256_hex(z.get(original).map_or(&[][..], Vec::as_slice)), sha);
    // only Alex's data, and nothing about access control or tokens
    assert!(
        !names
            .iter()
            .any(|n| n.contains("api_token") || n.contains("profile_grant"))
    );
    let observations =
        String::from_utf8_lossy(z.get("fhir/Observation.ndjson").map_or(&[][..], Vec::as_slice)).to_string();
    assert!(!observations.contains(&env.profile("sam").to_string()));
    assert_eq!(observations.lines().count(), 2);
    // audited, and the temporary zip is gone
    let (via, bytes_out, args): (String, i32, String) = sqlx::query_as(
        "select via, bytes_out, args_summary from audit_event where action = 'export' and profile_id = $1",
    )
    .bind(env.profile("alex"))
    .fetch_one(env.pool())
    .await?;
    assert_eq!((via.as_str(), bytes_out as usize), ("export", r.bytes.len()));
    assert!(args.starts_with("fmt=zip;files="), "{args}");
    assert_eq!(tmp_exports(&env), 0);
    Ok(())
}

#[tokio::test]
async fn a_failed_export_audit_leaves_no_zip_behind() -> R {
    let env = Env::new(true).await?;
    sqlx::raw_sql(
        "create function oh_test_refuse() returns trigger language plpgsql as \
         $$ begin if new.action = 'export' then raise exception 'audit store down'; end if; return new; end $$; \
         create trigger oh_test_refuse before insert on audit_event for each row execute function oh_test_refuse();",
    )
    .execute(env.pool())
    .await?;
    let r = env
        .as_user(env.user("alex"))
        .await?
        .post("/api/profiles/alex/export", &json!({"fmt": "zip"}))
        .await?;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(tmp_exports(&env), 0);
    Ok(())
}

#[tokio::test]
async fn delete_profile() -> R {
    let env = Env::new(true).await?;
    let sha = seed(&env, "alex", None).await?;
    assert!(env.rt.blobs.exists(&sha));
    let mut c = env.pool().acquire().await?;
    let sam_src = SrcRef::New(SrcIn::manual(Some(env.user("sam"))));
    let sams = commit_observation(
        &mut c,
        &obs(env.profile("sam"), "55", "2026-09-14")?,
        &sam_src,
        CommitOptions::new("person"),
    )
    .await?;
    drop(c);
    let mut alex = env.as_user(env.user("alex")).await?;
    assert_eq!(
        alex.delete("/api/profiles/alex?confirm=Alex").await?.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        alex.delete("/api/profiles/alex").await?.json["detail"][0]["loc"],
        json!(["query", "confirm"])
    );
    let r = env
        .as_user(env.user("sam"))
        .await?
        .delete("/api/profiles/alex?confirm=alex")
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = env
        .stale(env.user("alex"))
        .await?
        .delete("/api/profiles/alex?confirm=alex")
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = alex.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        (&r.json["deleted"], &r.json["rows"]["observation"], &r.json["blobs"]),
        (&json!("alex"), &json!(2), &json!(1))
    );
    let pid = env.profile("alex");
    for (table, col) in [
        ("observation", "profile_id"),
        ("document", "profile_id"),
        ("src", "profile_id"),
        ("profile", "id"),
    ] {
        let n = env
            .scalar_i64(&format!("select count(*) from {table} where {col} = '{pid}'"))
            .await?;
        assert_eq!(n, 0, "{table}");
    }
    // the audit log keeps its events (ids only), and gains the purge
    let actions: Vec<String> = sqlx::query_scalar("select action from audit_event where profile_id = $1")
        .bind(pid)
        .fetch_all(env.pool())
        .await?;
    assert!(actions.iter().any(|a| a == "read") && actions.iter().any(|a| a == "purge"));
    assert!(!env.rt.blobs.exists(&sha));
    // Sam's data is untouched
    let owner: Uuid = sqlx::query_scalar("select profile_id from observation where id = $1")
        .bind(sams.id)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(owner, env.profile("sam"));
    Ok(())
}

#[tokio::test]
async fn shared_blob_survives() -> R {
    let env = Env::new(true).await?;
    let sha = seed(&env, "lucia", Some("24")).await?;
    let again = seed(&env, "sam", Some("24")).await?; // same bytes, other profile
    assert_eq!(sha, again);
    let r = env
        .as_user(env.user("alex"))
        .await?
        .delete("/api/profiles/lucia?confirm=lucia")
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(env.rt.blobs.exists(&sha));
    Ok(())
}

#[tokio::test]
async fn delete_account() -> R {
    let env = Env::new(true).await?;
    let mut sam = env.as_user(env.user("sam")).await?;
    assert_eq!(
        sam.delete("/api/account?confirm=Sam").await?.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Sam is the only one on sam: closing the login would strand it
    let r = sam.delete("/api/account?confirm=Sam%20Rivera").await?;
    assert_eq!(
        (r.status, &r.json["detail"]["profiles"]),
        (StatusCode::CONFLICT, &json!(["sam"]))
    );
    let r = sam
        .delete("/api/account?confirm=Sam%20Rivera&delete_profile=true")
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["profile_deleted"]["deleted"], "sam");
    assert!(r.set_cookies()[0].starts_with("oh_session=\"\""));
    assert_eq!(
        sam.get("/api/household").await?.status,
        StatusCode::UNAUTHORIZED,
        "the session is gone"
    );
    let (name, disabled): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("select display_name, disabled_at from app_user where id = $1")
            .bind(env.user("sam"))
            .fetch_one(env.pool())
            .await?;
    assert!(name == "Deleted user" && disabled.is_some());
    let sam_id = env.user("sam");
    assert_eq!(
        env.scalar_i64(&format!(
            "select count(*) from profile_grant where user_id = '{sam_id}'"
        ))
        .await?,
        0
    );
    assert_eq!(
        env.scalar_i64(&format!(
            "select count(*) from profile where id = '{}'",
            env.profile("sam")
        ))
        .await?,
        0
    );
    // Lucía still has Alex as guardian
    let profiles = env.as_user(env.user("alex")).await?.get("/api/household").await?.json["profiles"].clone();
    assert!(profiles.as_array().into_iter().flatten().any(|p| p["slug"] == "lucia"));
    Ok(())
}

#[tokio::test]
async fn delete_account_checks_before_deleting() -> R {
    let env = Env::new(true).await?;
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam
        .post(
            "/api/household/profiles",
            &json!({"display_name": "Mateo Rivera", "role": "guardian"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = sam
        .delete("/api/account?confirm=Sam%20Rivera&delete_profile=true")
        .await?;
    assert_eq!(
        (r.status, &r.json["detail"]["profiles"]),
        (StatusCode::CONFLICT, &json!(["mateo"]))
    );
    assert_eq!(
        env.scalar_i64(&format!(
            "select count(*) from profile where id = '{}'",
            env.profile("sam")
        ))
        .await?,
        1
    );
    Ok(())
}
