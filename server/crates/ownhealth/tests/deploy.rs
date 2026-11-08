//! The deploy files (deploy/, compose.yaml), checked from outside the app.
//!
//! - The app's database role cannot rewrite the audit log (security-privacy.md section 9).
//!   deploy/db/app-role.sql runs as the table owner after the migrations. Connected as the role it
//!   creates, INSERT and SELECT on audit_event work, and UPDATE, DELETE, TRUNCATE and ALTER TABLE ...
//!   DISABLE TRIGGER are refused by privileges, not only by the trigger. The owner control shows the
//!   same ALTER succeeds for the owner, so the test can fail. Needs psql 15 or newer on PATH, or the
//!   test container (scripts/test-db.sh).
//! - What the two compose files hand the app container, rendered by `docker compose config`: the app
//!   binds 0.0.0.0, connects as the non-owner role and does not migrate; the admin service migrates as
//!   the owner; the homelab app gets no backup credentials. Skipped without docker compose.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use ownhealth_core::config::Settings;
use ownhealth_core::testing::{TestDb, admin_url};
use serde_json::Value;
use sqlx::{Connection, PgConnection};

type R = anyhow::Result<()>;

const CONTAINER: &str = "ownhealth-testpg";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn ok(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// psql 15 or newer on PATH.
fn host_psql() -> bool {
    Command::new("psql")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.split_whitespace()
                .find_map(|w| w.split('.').next()?.parse::<u32>().ok())
        })
        .is_some_and(|major| major >= 15)
}

/// `postgres://user:password@host:port/<db>` from the admin URL, with another login when given.
fn url(db: &str, login: Option<(&str, &str)>) -> String {
    let admin = admin_url();
    let base = admin.rsplit_once('/').map_or(admin.as_str(), |(b, _)| b);
    let base = match (login, base.split_once("://"), base.rsplit_once('@')) {
        (Some((user, password)), Some((scheme, _)), Some((_, host))) => format!("{scheme}://{user}:{password}@{host}"),
        _ => base.to_string(),
    };
    format!("{base}/{db}")
}

fn apply_role_sql(db: &str, role: &str, password: &str) -> anyhow::Result<Output> {
    let sql = std::fs::read(root().join("deploy/db/app-role.sql"))?;
    let args = [
        "-X",
        "-q",
        "-v",
        "ON_ERROR_STOP=1",
        "-v",
        &format!("app_role={role}"),
        "-f",
        "-",
    ];
    let mut cmd = if host_psql() {
        let mut c = Command::new("psql");
        c.arg(url(db, None)).args(args);
        c
    } else {
        let mut c = Command::new("docker");
        c.args([
            "exec",
            "-i",
            "-e",
            "OWNHEALTH_APP_ROLE_PASSWORD",
            CONTAINER,
            "psql",
            "-U",
            "postgres",
            "-d",
            db,
        ])
        .args(args);
        c
    };
    let mut child = cmd
        .env("OWNHEALTH_APP_ROLE_PASSWORD", password)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(&sql)?;
    Ok(child.wait_with_output()?)
}

async fn refused(conn: &mut PgConnection, sql: &str) -> anyhow::Result<String> {
    match sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await
    {
        Ok(_) => anyhow::bail!("{sql}: allowed"),
        Err(e) => Ok(e.to_string()),
    }
}

async fn check_app_role(db: &TestDb, role: &str, password: &str) -> R {
    for _ in 0..2 {
        // idempotent: the admin service runs it after every migration
        let done = apply_role_sql(&db.name, role, password)?;
        anyhow::ensure!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    }
    let mut app = PgConnection::connect(&url(&db.name, Some((role, password)))).await?;
    sqlx::query("insert into audit_event (actor_kind, via, action) values ('system', 'job', 'test')")
        .execute(&mut app)
        .await?;
    let n: i64 = sqlx::query_scalar("select count(*) from audit_event")
        .fetch_one(&mut app)
        .await?;
    assert!(n >= 1);
    sqlx::query("select count(*) from profile").execute(&mut app).await?; // ordinary tables stay usable
    for sql in [
        "update audit_event set action = 'rewritten'",
        "delete from audit_event",
        "truncate audit_event",
    ] {
        let e = refused(&mut app, sql).await?;
        assert!(e.contains("permission denied"), "{sql}: {e}");
    }
    for sql in [
        "alter table audit_event disable trigger all",
        "drop trigger audit_event_no_update on audit_event",
    ] {
        let e = refused(&mut app, sql).await?;
        assert!(e.contains("must be owner"), "{sql}: {e}");
    }
    app.close().await?;

    // owner control: the same ALTER succeeds for the table owner
    let mut owner = db.pool.begin().await?;
    sqlx::query("alter table audit_event disable trigger all")
        .execute(&mut *owner)
        .await?;
    sqlx::query("update audit_event set action = 'rewritten'")
        .execute(&mut *owner)
        .await?;
    owner.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn app_role_can_append_and_read_but_not_rewrite_the_audit_log() -> R {
    let has_docker = ok(Command::new("docker").args(["exec", CONTAINER, "true"]));
    if !host_psql() && !has_docker {
        eprintln!("no psql 15+ and no test container: skipped");
        return Ok(());
    }
    let db = TestDb::new().await?;
    let role = format!("oh_test_app_{}", ownhealth_core::ids::token_hex(4));
    let password = ownhealth_core::ids::token_hex(16);
    let result = check_app_role(&db, &role, &password).await;
    let name = db.name.clone();
    drop(db); // drops the database, so the role owns nothing any more
    let mut admin = PgConnection::connect(&admin_url()).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!("drop role if exists \"{role}\"")))
        .execute(&mut admin)
        .await?;
    let left: i64 = sqlx::query_scalar("select count(*) from pg_database where datname = $1")
        .bind(&name)
        .fetch_one(&mut admin)
        .await?;
    assert_eq!(left, 0);
    result
}

// ------------------------------------------------------------------ compose ----

fn has_compose() -> bool {
    ok(Command::new("docker").args(["compose", "version"]))
}

/// `docker compose config --format json` of `compose` with `env_file` (+ `extra`) as its .env.
fn render(compose: &Path, env_file: &Path, extra: &str, files: &[(&str, String)]) -> anyhow::Result<Value> {
    let dir = tempfile::tempdir()?;
    std::fs::copy(compose, dir.path().join("compose.yaml"))?;
    std::fs::write(dir.path().join(".env"), std::fs::read_to_string(env_file)? + extra)?;
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body)?;
    }
    let out = Command::new("docker")
        .args(["compose", "config", "--format", "json"])
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into()))
        .output()?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(v["services"].clone())
}

fn cases() -> [(&'static str, PathBuf, PathBuf, &'static str); 2] {
    let r = root();
    [
        (
            "homelab",
            r.join("deploy/compose.homelab.yaml"),
            r.join("deploy/env.homelab.example"),
            "",
        ),
        (
            "quickstart",
            r.join("compose.yaml"),
            r.join(".env.example"),
            "OWNHEALTH_DB_PASSWORD=x\nOWNHEALTH_APP_DB_PASSWORD=y\n",
        ),
    ]
}

fn env_str(v: &Value, key: &str) -> String {
    v["environment"][key].as_str().unwrap_or_default().to_string()
}

#[test]
fn compose_app_binds_all_interfaces_as_the_non_owner_role() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    // the image's own ENV (Dockerfile); compose's env_file and environment override it
    let image_env = [
        ("OWNHEALTH_DATA_DIR", "/data"),
        ("OWNHEALTH_HOST", "0.0.0.0"),
        ("OWNHEALTH_PORT", "8080"),
    ];
    for (case, compose, env_file, extra) in cases() {
        let services = render(&compose, &env_file, extra, &[])?;
        let (app, admin) = (&services["app"], &services["admin"]);
        // OWNHEALTH_HOST is the bind address: a public name there crash-loops the app
        let mut env: HashMap<String, String> = image_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        for (k, v) in app["environment"].as_object().into_iter().flatten() {
            if k.starts_with("OWNHEALTH_") && !k.ends_with("_FILE") {
                env.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
            }
        }
        assert_eq!(Settings::from(&env)?.host, "0.0.0.0", "{case}");
        assert!(
            env_str(app, "OWNHEALTH_DATABASE_URL").starts_with("postgresql://ownhealth_app@"),
            "{case}"
        );
        assert_eq!(env_str(app, "OWNHEALTH_RUN_MIGRATIONS"), "0", "{case}");
        assert_eq!(
            app["depends_on"]["admin"]["condition"], "service_completed_successfully",
            "{case}"
        );
        assert!(
            env_str(admin, "OWNHEALTH_DATABASE_URL").starts_with("postgresql://ownhealth@"),
            "{case}"
        );
        let command: Vec<&str> = admin["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        assert!(command.join(" ").contains("app-role.sh"), "{case}: {command:?}");
    }
    Ok(())
}

/// restic.env holds the backup repository and its S3 keys, for the admin service only. The app is the
/// network-facing process, so a compromise of it must not hand over the keys to the backups. An
/// install made before restic.env existed keeps RESTIC_REPOSITORY in .env and must still render.
#[test]
fn homelab_app_gets_no_backup_credentials() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    let [(_, compose, env_file, _), _] = cases();
    let restic_env = std::fs::read_to_string(root().join("deploy/restic.env.example"))?
        + "AWS_ACCESS_KEY_ID=synthetic-key\nAWS_SECRET_ACCESS_KEY=synthetic-secret\n";
    let services = render(&compose, &env_file, "", &[("restic.env", restic_env)])?;
    let (app, admin) = (&services["app"], &services["admin"]);
    let leaked: Vec<&String> = app["environment"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("RESTIC_") || k.starts_with("AWS_"))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
    assert_eq!(env_str(admin, "AWS_ACCESS_KEY_ID"), "synthetic-key");
    assert!(env_str(admin, "RESTIC_REPOSITORY").starts_with("s3:"));
    assert_eq!(env_str(app, "OWNHEALTH_DEFAULT_LOCALE"), "es");

    let old = render(
        &compose,
        &env_file,
        "RESTIC_REPOSITORY=s3:https://s3.example.lan/old\n",
        &[],
    )?;
    assert_eq!(
        env_str(&old["admin"], "RESTIC_REPOSITORY"),
        "s3:https://s3.example.lan/old"
    );
    Ok(())
}
