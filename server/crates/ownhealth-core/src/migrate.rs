//! Forward-only migration runner, compatible with the Python runner of 0.1.x (`db/migrate.py`).
//!
//! Migrations are `NNNN_name.sql`, applied in numeric order, each in its own transaction, recorded
//! in `schema_migrations (version, name, checksum = sha256 hex of the file bytes, applied_at)`.
//! A Postgres advisory lock keeps two processes from migrating at once. The runner refuses a
//! database that has a migration this code does not know (a schema newer than the code) and a
//! migration whose file changed after it was applied. The files are the Python ones, unchanged
//! (`migrations`), built into the binary, so a 0.1.1 database is up to date.

use sqlx::AssertSqlSafe;
use std::collections::BTreeMap;
use std::path::Path;

use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, PgPool, Row};

use crate::error::{Error, Result};

/// The same constant as the Python runner (`_LOCK_KEY = 0x0E7_4EA1`).
pub const LOCK_KEY: i64 = 0x0E7_4EA1;

#[derive(RustEmbed)]
#[folder = "../../../migrations"]
#[include = "*.sql"]
struct Embedded;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub version: i32,
    pub name: String,
    pub sql: String,
    /// sha256 hex of the file bytes.
    pub checksum: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MigrationError(pub String);

impl From<MigrationError> for Error {
    fn from(e: MigrationError) -> Self {
        Error::Other(anyhow::Error::new(e))
    }
}

fn parse_name(file: &str) -> Option<(i32, String)> {
    let stem = file.strip_suffix(".sql")?;
    let (num, name) = stem.split_once('_')?;
    let ok_num = num.len() == 4 && num.bytes().all(|b| b.is_ascii_digit());
    let ok_name = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !(ok_num && ok_name) {
        return None;
    }
    Some((num.parse().ok()?, name.to_string()))
}

fn build(files: Vec<(String, Vec<u8>)>) -> Result<Vec<Migration>, MigrationError> {
    let mut found: BTreeMap<i32, Migration> = BTreeMap::new();
    for (file, bytes) in files {
        let (version, name) = parse_name(&file)
            .ok_or_else(|| MigrationError(format!("bad migration file name: {file} (want NNNN_name.sql)")))?;
        if found.contains_key(&version) {
            return Err(MigrationError(format!("two migrations share number {version:04}")));
        }
        let checksum = hex::encode(Sha256::digest(&bytes));
        // Python reads the file with universal newlines; the shipped files use \n already.
        let sql = String::from_utf8(bytes)
            .map_err(|_| MigrationError(format!("{file} is not UTF-8")))?
            .replace("\r\n", "\n");
        found.insert(
            version,
            Migration {
                version,
                name,
                sql,
                checksum,
            },
        );
    }
    Ok(found.into_values().collect())
}

/// The migrations built into this binary.
pub fn embedded() -> Result<Vec<Migration>, MigrationError> {
    let mut files: Vec<(String, Vec<u8>)> = Embedded::iter()
        .filter_map(|f| Embedded::get(&f).map(|c| (f.to_string(), c.data.into_owned())))
        .collect();
    files.sort();
    build(files)
}

/// The `*.sql` files of a directory (tests, or a newer set beside the binary).
pub fn discover(dir: &Path) -> Result<Vec<Migration>, MigrationError> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| MigrationError(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let path = entry.map_err(|e| MigrationError(e.to_string()))?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let file = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let bytes = std::fs::read(&path).map_err(|e| MigrationError(format!("{file}: {e}")))?;
        files.push((file, bytes));
    }
    files.sort();
    build(files)
}

/// `version -> checksum` of what the database has applied (empty when the table does not exist).
pub async fn applied_versions(conn: &mut PgConnection) -> Result<BTreeMap<i32, String>> {
    let exists: bool = sqlx::query_scalar("select to_regclass('schema_migrations') is not null")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(BTreeMap::new());
    }
    let rows = sqlx::query("select version, checksum from schema_migrations")
        .fetch_all(&mut *conn)
        .await?;
    let mut out = BTreeMap::new();
    for r in rows {
        out.insert(r.try_get::<i32, _>("version")?, r.try_get::<String, _>("checksum")?);
    }
    Ok(out)
}

/// Apply pending migrations. Returns the versions applied in this call.
pub async fn migrate(pool: &PgPool, migrations: &[Migration]) -> Result<Vec<i32>> {
    let mut lock = pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *lock)
        .await?;
    let result = migrate_locked(pool, migrations).await;
    let unlocked = sqlx::query("select pg_advisory_unlock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *lock)
        .await;
    let applied = result?;
    unlocked?;
    Ok(applied)
}

/// `migrate` with the migrations built into the binary.
pub async fn migrate_embedded(pool: &PgPool) -> Result<Vec<i32>> {
    migrate(pool, &embedded()?).await
}

async fn migrate_locked(pool: &PgPool, migrations: &[Migration]) -> Result<Vec<i32>> {
    let mut conn = pool.acquire().await?;
    sqlx::query(
        "create table if not exists schema_migrations ( version int primary key, name text not null, \
         checksum text not null, applied_at timestamptz not null default now())",
    )
    .execute(&mut *conn)
    .await?;
    let done = applied_versions(&mut conn).await?;
    let unknown: Vec<i32> = done
        .keys()
        .filter(|v| !migrations.iter().any(|m| m.version == **v))
        .copied()
        .collect();
    if !unknown.is_empty() {
        return Err(MigrationError(format!(
            "database has migrations this code does not know: {unknown:?}; the schema is newer than the code, \
             refusing to start"
        ))
        .into());
    }
    let mut applied_now = Vec::new();
    for m in migrations {
        if let Some(sum) = done.get(&m.version) {
            if sum != &m.checksum {
                return Err(MigrationError(format!(
                    "migration {:04}_{} changed after it was applied; migrations are forward-only, add a new file \
                     instead",
                    m.version, m.name
                ))
                .into());
            }
            continue;
        }
        let mut tx = conn.begin().await?;
        // No bind parameters: sqlx sends the file as one simple query, several statements at once.
        sqlx::raw_sql(AssertSqlSafe(m.sql.as_str())).execute(&mut *tx).await?;
        sqlx::query("insert into schema_migrations (version, name, checksum) values ($1, $2, $3)")
            .bind(m.version)
            .bind(&m.name)
            .bind(&m.checksum)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        applied_now.push(m.version);
    }
    Ok(applied_now)
}

/// The versions of `migrations` this database has not applied yet.
pub async fn pending(conn: &mut PgConnection, migrations: &[Migration]) -> Result<Vec<i32>> {
    let done = applied_versions(conn).await?;
    Ok(migrations
        .iter()
        .filter(|m| !done.contains_key(&m.version))
        .map(|m| m.version)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_files_are_numbered_unique_and_match_the_python_checksums() {
        let ms = embedded().expect("embedded");
        let versions: Vec<i32> = ms.iter().map(|m| m.version).collect();
        let mut sorted = versions.clone();
        sorted.dedup();
        assert_eq!(versions, sorted);
        assert_eq!(versions[0], 1);
        // the checksum is of the bytes on disk, as Python's hashlib.sha256(path.read_bytes())
        let first = &ms[0];
        let disk = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations/0001_foundation.sql"))
            .expect("read");
        assert_eq!(first.checksum, hex::encode(Sha256::digest(&disk)));
        assert_eq!(first.name, "foundation");
    }

    #[test]
    fn a_bad_file_name_is_rejected() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("1_bad.sql"), "select 1;").expect("write");
        assert!(discover(dir.path()).is_err());
        assert!(parse_name("0001_Foundation.sql").is_none());
        assert_eq!(
            parse_name("0150_pending_document.sql"),
            Some((150, "pending_document".into()))
        );
    }
}
