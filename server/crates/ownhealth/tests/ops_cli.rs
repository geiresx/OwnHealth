//! Lane H: the `backup | restore | verify-restore` commands of the binary, and the restic recipe in
//! deploy/restic/ run end to end against a local restic repository (`tests/ops/test_backup_restore.py`
//! CLI case, `tests/ops/test_restic_scripts.py`).
//!
//! Needs pg_dump/pg_restore 17 on PATH or in OWNHEALTH_PG_BINDIR (see ownhealth-ops/tests/backup.rs).
//! The restic cases need restic on PATH: skipped without it, except in CI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::config::Settings;
use ownhealth_core::demo::seed_demo;
use ownhealth_core::testing::TestDb;
use serde_json::Value;

type R = anyhow::Result<()>;

const BIN: &str = env!("CARGO_BIN_EXE_ownhealth");

fn deploy() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../deploy/restic")
}

async fn seeded(dir: &Path) -> anyhow::Result<(TestDb, Settings)> {
    ownhealth_ops::register();
    let db = TestDb::new().await?;
    let settings = db.settings(&dir.join("live"))?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    Ok((db, settings))
}

fn ownhealth(s: &Settings, args: &[&str]) -> anyhow::Result<Output> {
    Ok(Command::new(BIN)
        .args(args)
        .env("OWNHEALTH_DATABASE_URL", &s.database_url)
        .env("OWNHEALTH_DATA_DIR", &s.data_dir)
        .output()?)
}

#[tokio::test]
async fn cli_verify_restore_exits_non_zero_on_a_mismatch() -> R {
    let dir = tempfile::tempdir()?;
    let (_db, s) = seeded(dir.path()).await?;
    let archive = dir.path().join("backup");
    let out = ownhealth(&s, &["backup", &archive.display().to_string()])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout)?.trim(), archive.display().to_string());
    let ok = ownhealth(&s, &["verify-restore", &archive.display().to_string()])?;
    assert_eq!(ok.status.code(), Some(0), "{}", String::from_utf8_lossy(&ok.stdout));
    let path = archive.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    m["tables"]["profile"] = Value::from(m["tables"]["profile"].as_i64().unwrap_or(0) + 1);
    std::fs::write(&path, serde_json::to_vec(&m)?)?;
    let bad = ownhealth(&s, &["verify-restore", &archive.display().to_string()])?;
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8(bad.stdout)?.contains("\"ok\": false"));
    // an OpsError is one line on stderr and exit 1, as in Python
    let err = ownhealth(
        &s,
        &["verify-restore", &dir.path().join("nowhere").display().to_string()],
    )?;
    assert_eq!(err.status.code(), Some(1));
    assert!(String::from_utf8(err.stderr)?.contains("error: "));
    Ok(())
}

fn restic() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("restic"))
        .find(|p| p.is_file())
}

/// None when restic is missing outside CI (skip); in CI a missing restic is a broken runner.
fn restic_or_skip() -> Option<PathBuf> {
    let r = restic();
    assert!(
        r.is_some() || std::env::var_os("CI").is_none(),
        "restic is not on PATH: CI must install the version the image ships"
    );
    if r.is_none() {
        eprintln!("skipped: restic not on PATH");
    }
    r
}

struct Env {
    vars: Vec<(String, String)>,
}

fn env_for(s: &Settings, tmp: &Path) -> Env {
    let bin_dir = Path::new(BIN).parent().map(Path::to_path_buf).unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    let vars = [
        ("OWNHEALTH_RUN", String::new()),
        ("OWNHEALTH_DATABASE_URL", s.database_url.clone()),
        ("OWNHEALTH_DATA_DIR", s.data_dir.display().to_string()),
        ("OWNHEALTH_BACKUP_STAGING", tmp.join("staging").display().to_string()),
        ("OWNHEALTH_RESTORE_DIR", tmp.join("restore").display().to_string()),
        ("RESTIC_REPOSITORY", tmp.join("repo").display().to_string()),
        ("RESTIC_PASSWORD", "test-only-password".into()),
        ("RESTIC_CACHE_DIR", tmp.join("cache").display().to_string()),
        ("PATH", format!("{}:{path}", bin_dir.display())),
    ];
    Env {
        vars: vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    }
}

fn run(cmd: impl AsRef<std::ffi::OsStr>, env: &Env, args: &[&str]) -> anyhow::Result<Output> {
    Ok(Command::new(cmd)
        .args(args)
        .envs(env.vars.iter().map(|(k, v)| (k, v)))
        .output()?)
}

fn script(name: &str, env: &Env) -> anyhow::Result<Output> {
    run(deploy().join(name), env, &[])
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[tokio::test]
async fn backup_then_restore_test_then_restore_into_a_fresh_stack() -> R {
    let Some(restic) = restic_or_skip() else { return Ok(()) };
    let tmp = tempfile::tempdir()?;
    let (_db, s) = seeded(tmp.path()).await?;
    let env = env_for(&s, tmp.path());
    assert!(run(&restic, &env, &["init", "--quiet"])?.status.success());

    let done = script("backup.sh", &env)?;
    assert!(done.status.success(), "{}", text(&done));
    let snaps: Value = serde_json::from_slice(&run(&restic, &env, &["snapshots", "--json"])?.stdout)?;
    assert_eq!(snaps.as_array().map(Vec::len), Some(1));
    assert_eq!(snaps[0]["tags"], serde_json::json!(["ownhealth"]));
    assert!(!tmp.path().join("staging").exists());

    let check = script("restore-test.sh", &env)?;
    assert!(check.status.success(), "{}", text(&check));
    let report: Value = serde_json::from_slice(&check.stdout)?;
    assert!(report["ok"] == true && report["profiles"].as_object().map(|p| p.len()) == Some(4));
    assert!(!tmp.path().join("restore").exists());

    let fresh_db = TestDb::empty().await?;
    let fresh = fresh_db.settings(&tmp.path().join("fresh"))?;
    let restored = script("restore.sh", &env_for(&fresh, tmp.path()))?;
    assert!(restored.status.success(), "{}", text(&restored));
    let fresh_report: Value = serde_json::from_slice(&restored.stdout)?;
    assert_eq!(fresh_report["ok"], true);
    let actual: Vec<(String, Value)> = fresh_report["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(p, r)| (p.clone(), r["actual"].clone()))
        .collect();
    let expected: Vec<(String, Value)> = report["profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(p, r)| (p.clone(), r["expected"].clone()))
        .collect();
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn restore_test_fails_when_the_latest_snapshot_does_not_match_its_manifest() -> R {
    let Some(restic) = restic_or_skip() else { return Ok(()) };
    let tmp = tempfile::tempdir()?;
    let (_db, s) = seeded(tmp.path()).await?;
    let env = env_for(&s, tmp.path());
    assert!(run(&restic, &env, &["init", "--quiet"])?.status.success());
    let staging = tmp.path().join("staging");
    ownhealth_ops::backup(&s, &staging).await?;
    let path = staging.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let pid = m["state_hash"]
        .as_object()
        .and_then(|o| o.keys().next().cloned())
        .unwrap_or_default();
    m["state_hash"][&pid] = Value::String(format!("sha256:{}", "0".repeat(64)));
    std::fs::write(&path, serde_json::to_vec(&m)?)?;
    let staging_arg = staging.display().to_string();
    let tag = [
        "backup",
        "--host",
        "ownhealth",
        "--tag",
        "ownhealth",
        "--quiet",
        staging_arg.as_str(),
    ];
    assert!(run(&restic, &env, &tag)?.status.success());

    let check = script("restore-test.sh", &env)?;
    assert_eq!(check.status.code(), Some(1), "{}", text(&check));
    let report: Value = serde_json::from_slice(&check.stdout)?;
    assert_eq!(report["ok"], false);
    assert!(String::from_utf8_lossy(&check.stderr).contains("FAILED"));
    Ok(())
}

#[tokio::test]
async fn restore_refuses_a_stack_that_already_has_data() -> R {
    let Some(restic) = restic_or_skip() else { return Ok(()) };
    let tmp = tempfile::tempdir()?;
    let (_db, s) = seeded(tmp.path()).await?;
    let env = env_for(&s, tmp.path());
    assert!(run(&restic, &env, &["init", "--quiet"])?.status.success());
    assert!(script("backup.sh", &env)?.status.success());
    let done = script("restore.sh", &env)?;
    assert!(
        !done.status.success() && String::from_utf8_lossy(&done.stderr).contains("not empty"),
        "{}",
        text(&done)
    );
    Ok(())
}
