//! Backup, restore and verify-restore (`ops/__init__.py`, architecture.md 14).
//!
//! A backup is a plain directory. restic snapshots it (deploy/restic/), so restic does the dedup,
//! encryption and retention, and this module only has to make the directory consistent:
//!
//! ```text
//! <dest>/manifest.json    app version, migrations, row counts, blob list, state_hash per profile
//! <dest>/db.dump          pg_dump -Fc, taken from the same database snapshot as the counts and hashes
//! <dest>/blobs/sha256/..  the blob store, hard-linked when on the same disk (blobs never change)
//! ```
//!
//! pg_dump and pg_restore come from PATH, or from the directory in OWNHEALTH_PG_BINDIR. They read and
//! write only stdin and stdout, so a wrapper that runs them in another container works too.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use chrono::{NaiveDate, SecondsFormat, Utc};
use ownhealth_core::config::Settings;
use ownhealth_core::{Error, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use tokio::process::Command;
use uuid::Uuid;

pub const MANIFEST_SCHEMA: &str = "ownhealth.backup/v1";

/// A backup or restore step failed; the message says which and why (Python's `OpsError`).
fn ops_error(msg: impl Into<String>) -> Error {
    Error::Other(anyhow::anyhow!(msg.into()))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn pg_tool(name: &str) -> Result<PathBuf> {
    let found = match std::env::var_os("OWNHEALTH_PG_BINDIR").filter(|d| !d.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir).join(name)),
        None => std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| is_executable(p))
        }),
    };
    found.filter(|p| is_executable(p)).ok_or_else(|| {
        ops_error(format!(
            "{name} not found: install the PostgreSQL 17 client or set OWNHEALTH_PG_BINDIR"
        ))
    })
}

fn libpq_url(url: &str) -> String {
    ownhealth_core::config::sqlx_url(url)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = b.get(i + 1..i + 3)
            && let Some(v) = std::str::from_utf8(hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// (the `--dbname` value without the password, the password for PGPASSWORD): argv is world-readable.
pub(crate) fn pg_conn(url: &str) -> (String, Option<String>) {
    let url = libpq_url(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url, None);
    };
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return (url, None);
    };
    let Some((user, password)) = userinfo.split_once(':') else {
        return (url, None);
    };
    (
        format!("{scheme}://{user}@{host}{tail}"),
        Some(percent_decode(password)),
    )
}

/// The same URL naming another database (query kept).
fn with_db(url: &str, db: &str) -> String {
    let url = libpq_url(url);
    let (base, rest) = url.rsplit_once('/').unwrap_or((&url, ""));
    match rest.split_once('?') {
        Some((_, query)) if !query.is_empty() => format!("{base}/{db}?{query}"),
        _ => format!("{base}/{db}"),
    }
}

fn db_name(url: &str) -> String {
    let url = libpq_url(url);
    let last = url.rsplit_once('/').map_or("", |(_, r)| r);
    last.split_once('?').map_or(last, |(d, _)| d).to_string()
}

/// The argv after the tool name, and the password that goes in PGPASSWORD instead.
fn pg_args(url: &str, args: &[&str]) -> (Vec<String>, Option<String>) {
    let (dsn, password) = pg_conn(url);
    let mut argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    argv.extend(["--dbname".to_string(), dsn]);
    (argv, password)
}

fn dump_args(url: &str, snapshot: &str) -> (Vec<String>, Option<String>) {
    pg_args(url, &["-Fc", &format!("--snapshot={snapshot}")])
}

fn restore_args(url: &str) -> (Vec<String>, Option<String>) {
    pg_args(
        url,
        &[
            "--no-owner",
            "--no-privileges",
            "--exit-on-error",
            "--single-transaction",
        ],
    )
}

fn pg_command(tool: &str, (argv, password): (Vec<String>, Option<String>)) -> Result<Command> {
    let mut cmd = Command::new(pg_tool(tool)?);
    cmd.args(argv);
    if let Some(pw) = password {
        cmd.env("PGPASSWORD", pw);
    }
    Ok(cmd)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Every file under `<root>/sha256`, by file name, in path order.
fn blob_files(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let base = root.join("sha256");
    let mut files = Vec::new();
    if base.is_dir() {
        walk_files(&base, &mut files)?;
    }
    files.sort();
    Ok(files
        .into_iter()
        .filter_map(|p| Some((p.file_name()?.to_string_lossy().into_owned(), p)))
        .collect())
}

async fn table_counts(conn: &mut PgConnection) -> Result<BTreeMap<String, i64>> {
    let names: Vec<String> =
        sqlx::query_scalar("select tablename::text from pg_tables where schemaname = 'public' order by 1")
            .fetch_all(&mut *conn)
            .await?;
    let mut out = BTreeMap::new();
    for n in names {
        let q = format!("select count(*) from \"{}\"", n.replace('"', "\"\""));
        let c: i64 = sqlx::query_scalar(AssertSqlSafe(q)).fetch_one(&mut *conn).await?;
        out.insert(n, c);
    }
    Ok(out)
}

async fn state_hashes(conn: &mut PgConnection, as_of: NaiveDate) -> Result<Vec<(String, String)>> {
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from profile order by id")
        .fetch_all(&mut *conn)
        .await?;
    let mut out = Vec::new();
    for pid in ids {
        let state = ownhealth_rules::build_state(&mut *conn, pid, as_of, None).await?;
        let hash = state["state_hash"]
            .as_str()
            .ok_or_else(|| ops_error(format!("build_state gave no state_hash for {pid}")))?;
        out.push((pid.to_string(), hash.to_string()));
    }
    Ok(out)
}

/// Every blob hash a document or genome dataset row points at, so verify can tell a blob the live
/// store had already lost from one the backup lost.
async fn referenced_blobs(conn: &mut PgConnection) -> Result<Vec<String>> {
    let cols: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns where table_schema = 'public' \
         and table_name = 'genome_dataset' and data_type = 'bytea' and column_name like '%\\_blob'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut parts = vec!["select blob_sha256 as h from document".to_string()];
    parts.extend(cols.iter().map(|c| format!("select \"{c}\" from genome_dataset")));
    let q = format!(
        "select distinct encode(h, 'hex') from ({}) x where h is not null order by 1",
        parts.join(" union ")
    );
    Ok(sqlx::query_scalar(AssertSqlSafe(q)).fetch_all(&mut *conn).await?)
}

fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            link_or_copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Recursively sorted keys: Python's `json.dumps(..., sort_keys=True)`.
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<String, Value> = m.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Write a consistent backup directory at `dest` (must be missing or empty). Returns `dest`.
pub async fn backup(settings: &Settings, dest: &Path) -> Result<PathBuf> {
    if dest.exists() && fs::read_dir(dest)?.next().is_some() {
        return Err(ops_error(format!("{} is not empty", dest.display())));
    }
    fs::create_dir_all(dest)?;
    let url = settings.sqlx_url();
    let mut conn = PgConnection::connect(&url).await?;
    sqlx::raw_sql("begin isolation level repeatable read")
        .execute(&mut conn)
        .await?;
    // pg_dump reads the snapshot this transaction exported, so the dump, the counts and the state
    // hashes all describe the same instant even while the app keeps writing.
    let snapshot: String = sqlx::query_scalar("select pg_export_snapshot()")
        .fetch_one(&mut conn)
        .await?;
    let out = fs::File::create(dest.join("db.dump"))?;
    let mut cmd = pg_command("pg_dump", dump_args(&url, &snapshot))?;
    let dump = cmd.stdout(out).stderr(Stdio::piped()).spawn()?;
    let facts = async {
        let referenced = referenced_blobs(&mut conn).await?;
        let migrations: Vec<i32> = sqlx::query_scalar("select version from schema_migrations order by 1")
            .fetch_all(&mut conn)
            .await?;
        let tables = table_counts(&mut conn).await?;
        let as_of = Utc::now().date_naive();
        let states = state_hashes(&mut conn, as_of).await?;
        Ok::<_, Error>((referenced, migrations, tables, as_of, states))
    }
    .await;
    // The dump must finish before the exporting transaction ends, whatever the facts did.
    let done = dump.wait_with_output().await?;
    // build_state may store snapshots; the backup leaves the database as it was.
    sqlx::raw_sql("rollback").execute(&mut conn).await?;
    conn.close().await?;
    let (referenced, migrations, tables, as_of, states) = facts?;
    if !done.status.success() {
        return Err(ops_error(format!(
            "pg_dump failed: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        )));
    }

    // Blobs are written before the rows that reference them commit, so every blob the snapshot can
    // see exists by now. Later blobs are harmless extras.
    let src = settings.blobs_path().join("sha256");
    if src.is_dir() {
        copy_tree(&src, &dest.join("blobs").join("sha256"))?;
    }
    let files = blob_files(&dest.join("blobs"))?;
    let mut bytes = 0u64;
    for p in files.values() {
        bytes += fs::metadata(p)?.len();
    }
    let manifest = json!({
        "schema": MANIFEST_SCHEMA,
        "created_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false),
        "app_version": env!("CARGO_PKG_VERSION"),
        "migrations": migrations,
        "rules_version": ownhealth_rules::rules_version()?,
        "as_of": as_of.to_string(),
        "tables": tables,
        "blobs": {
            "count": files.len(),
            "bytes": bytes,
            "sha256": files.keys().collect::<Vec<_>>(),
            "referenced": referenced,
        },
        "state_hash": states.into_iter().map(|(p, h)| (p, Value::String(h))).collect::<Map<String, Value>>(),
    });
    let text = serde_json::to_string_pretty(&sort_keys(manifest))? + "\n";
    fs::write(dest.join("manifest.json"), text)?;
    Ok(dest.to_path_buf())
}

fn py_repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => format!("'{s}'"),
        Some(other) => other.to_string(),
    }
}

fn read_manifest(archive: &Path) -> Result<Value> {
    let path = archive.join("manifest.json");
    if !path.is_file() {
        return Err(ops_error(format!(
            "{} not found: point at the directory `ownhealth backup` wrote",
            path.display()
        )));
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&path)?)?;
    if manifest.get("schema").and_then(Value::as_str) != Some(MANIFEST_SCHEMA) {
        return Err(ops_error(format!(
            "unknown backup schema {}, expected {MANIFEST_SCHEMA}",
            py_repr(manifest.get("schema"))
        )));
    }
    Ok(manifest)
}

async fn pg_restore(url: &str, archive: &Path) -> Result<()> {
    let dump = fs::File::open(archive.join("db.dump"))?;
    let mut cmd = pg_command("pg_restore", restore_args(url))?;
    let done = cmd
        .stdin(dump)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !done.status.success() {
        return Err(ops_error(format!(
            "pg_restore failed: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        )));
    }
    Ok(())
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn check_blobs(manifest: &Value, root: &Path) -> Result<Value> {
    let files = blob_files(root)?;
    let listed: BTreeSet<String> = str_list(&manifest["blobs"]["sha256"]).into_iter().collect();
    let mut expected = listed.clone();
    expected.extend(str_list(&manifest["blobs"]["referenced"])); // older manifests have no "referenced"
    let missing: Vec<&String> = expected.iter().filter(|s| !files.contains_key(*s)).collect();
    let mut corrupt = Vec::new();
    for (sha, p) in &files {
        if listed.contains(sha) && &sha256_file(p)? != sha {
            corrupt.push(sha.clone());
        }
    }
    Ok(json!({"checked": files.len(), "missing": missing, "corrupt": corrupt}))
}

/// Row counts, blob hashes and state_hash per profile against the manifest.
async fn compare(url: &str, manifest: &Value, blob_root: &Path) -> Result<Value> {
    let as_of_text = manifest["as_of"].as_str().unwrap_or_default();
    let as_of = NaiveDate::parse_from_str(as_of_text, "%Y-%m-%d")
        .map_err(|e| ops_error(format!("bad as_of {as_of_text:?} in the manifest: {e}")))?;
    let mut conn = PgConnection::connect(url).await?;
    let mut tx = conn.begin().await?;
    let counts = table_counts(&mut tx).await?; // before build_state, which may add state_snapshot rows
    let states = state_hashes(&mut tx, as_of).await?;
    tx.commit().await?;
    conn.close().await?;

    let mut tables = Map::new();
    if let Some(expected) = manifest["tables"].as_object() {
        for (name, n) in expected {
            let actual = counts.get(name).copied();
            if actual.and_then(|a| n.as_i64().map(|n| n == a)) != Some(true) {
                tables.insert(name.clone(), json!({"expected": n, "actual": actual}));
            }
        }
    }
    let actual: BTreeMap<&str, &str> = states.iter().map(|(p, h)| (p.as_str(), h.as_str())).collect();
    let mut profiles = Map::new();
    let empty = Map::new();
    let expected = manifest["state_hash"].as_object().unwrap_or(&empty);
    for (pid, h) in expected {
        let got = actual.get(pid.as_str()).copied();
        let ok = got.is_some() && h.as_str() == got;
        profiles.insert(pid.clone(), json!({"expected": h, "actual": got, "ok": ok}));
    }
    for (pid, h) in &states {
        if !expected.contains_key(pid) {
            profiles.insert(pid.clone(), json!({"expected": null, "actual": h, "ok": false}));
        }
    }
    let blobs = check_blobs(manifest, blob_root)?;
    let ok = tables.is_empty()
        && profiles.values().all(|p| p["ok"] == json!(true))
        && blobs["missing"].as_array().is_some_and(Vec::is_empty)
        && blobs["corrupt"].as_array().is_some_and(Vec::is_empty);
    Ok(json!({"ok": ok, "as_of": as_of_text, "tables": tables, "profiles": profiles, "blobs": blobs}))
}

/// Load a backup into the configured, empty database and blob directory, then check it.
/// Returns the same report as `verify_restore`, computed on the restored instance.
pub async fn restore(settings: &Settings, archive: &Path) -> Result<Value> {
    let manifest = read_manifest(archive)?;
    let url = settings.sqlx_url();
    let mut conn = PgConnection::connect(&url).await?;
    let tables: i64 = sqlx::query_scalar("select count(*) from pg_tables where schemaname = 'public'")
        .fetch_one(&mut conn)
        .await?;
    conn.close().await?;
    if tables > 0 {
        return Err(ops_error(
            "the target database is not empty: restore needs a fresh instance (new volume, app not started)",
        ));
    }
    // Check every blob before the database is loaded: a corrupt one found after pg_restore would
    // leave a populated database that a second attempt refuses as "not empty".
    let backup_blobs = blob_files(&archive.join("blobs"))?;
    for (sha, path) in &backup_blobs {
        if &sha256_file(path)? != sha {
            return Err(ops_error(format!("blob {sha} in the backup is corrupt")));
        }
    }
    pg_restore(&url, archive).await?;
    let root = settings.blobs_path();
    for (sha, path) in &backup_blobs {
        if sha.len() < 4 {
            continue;
        }
        let target = root.join("sha256").join(&sha[..2]).join(&sha[2..4]).join(sha);
        if !target.exists() {
            let parent = target.parent().unwrap_or(&root);
            fs::create_dir_all(parent)?;
            let part = parent.join(format!(".{sha}.part")); // copied whole, then renamed: never a truncated blob
            fs::copy(path, &part)?;
            fs::rename(&part, &target)?;
        }
    }
    compare(&url, &manifest, &root).await
}

/// Restore into a scratch database `<db>_restore_check`, compare with the manifest, drop it.
/// `ok` is true only when every row count, every blob hash and every profile's state_hash match.
pub async fn verify_restore(settings: &Settings, archive: &Path) -> Result<Value> {
    let manifest = read_manifest(archive)?;
    let live = settings.sqlx_url();
    let scratch = format!("{}_restore_check", db_name(&live));
    let drop = format!("drop database if exists \"{scratch}\" with (force)");
    let mut admin = PgConnection::connect(&live).await?;
    sqlx::query(AssertSqlSafe(drop.clone())).execute(&mut admin).await?;
    sqlx::query(AssertSqlSafe(format!("create database \"{scratch}\"")))
        .execute(&mut admin)
        .await?;
    let target = with_db(&live, &scratch);
    let result = async {
        pg_restore(&target, archive).await?;
        compare(&target, &manifest, &archive.join("blobs")).await
    }
    .await;
    let dropped = sqlx::query(AssertSqlSafe(drop)).execute(&mut admin).await;
    admin.close().await?;
    let report = result?;
    dropped?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_stays_out_of_the_pg_tools_argv() {
        let (dsn, pw) = pg_conn("postgresql+psycopg://oh:s3cr%2Ft@db:5432/ownhealth?sslmode=require");
        assert!(!dsn.contains("s3cr") && dsn.starts_with("postgresql://oh@db:5432/ownhealth"));
        assert!(dsn.contains("sslmode=require"));
        assert_eq!(pw.as_deref(), Some("s3cr/t"));
        assert_eq!(
            pg_conn("postgresql://oh@db/ownhealth"),
            ("postgresql://oh@db/ownhealth".into(), None)
        );
    }

    #[test]
    fn backup_and_restore_never_put_the_password_on_a_command_line() {
        let url = "postgresql://oh:hunter2@db:5432/ownhealth";
        for (argv, pw) in [dump_args(url, "00000003-1"), restore_args(url)] {
            assert!(argv.iter().all(|a| !a.contains("hunter2")), "{argv:?}");
            assert!(argv.contains(&"postgresql://oh@db:5432/ownhealth".to_string()));
            assert_eq!(pw.as_deref(), Some("hunter2"));
        }
    }

    #[test]
    fn with_db_keeps_the_query() {
        assert_eq!(
            with_db("postgresql://u:p@h:1/live?sslmode=require", "x"),
            "postgresql://u:p@h:1/x?sslmode=require"
        );
        assert_eq!(with_db("postgresql+psycopg://u@h/live", "x"), "postgresql://u@h/x");
        assert_eq!(db_name("postgresql://u@h/live?a=b"), "live");
    }

    #[test]
    fn sort_keys_sorts_every_level() {
        let v = sort_keys(json!({"b": {"y": 1, "x": 2}, "a": [{"d": 1, "c": 2}]}));
        assert_eq!(
            serde_json::to_string(&v).unwrap_or_default(),
            r#"{"a":[{"c":2,"d":1}],"b":{"x":2,"y":1}}"#
        );
    }
}
