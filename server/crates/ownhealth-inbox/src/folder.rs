//! Lane B: the watched folder (ingestion.md 3, connectors.md 2.1). Polling, because inotify is
//! unreliable on SMB and NFS mounts. `<OWNHEALTH_INBOX_DIR>/<profile-slug>/<file>` goes to that
//! profile; a file directly in the root goes to `source.profile_id`. A file is taken once nothing has
//! written to it for STABLE_SECONDS (its mtime and ctime, so a copy that keeps the original mtime still
//! waits); after its document commits it moves to `<root>/.done/<slug>/`, on failure to
//! `<root>/.failed/<slug>/` next to a `<name>.reason.txt`. The per-source dedup key is the relative
//! path, size and mtime, so a file that could not be moved is not imported twice.
//!
//! Hidden files, temporary downloads (`.part`, `.crdownload`, `.tmp`, `~$...`) and symlinks are
//! skipped: a link could point anywhere the server can read. The file is opened once and checked on
//! the open handle (same inode as the scan saw, same size and mtime), so a file swapped for a link
//! between the scan and the read is refused, not followed. Source `config` (kind `folder`): `{}`
//! (the root is OWNHEALTH_INBOX_DIR) or `{"subdir": "..."}`, a relative path below it.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use ownhealth_core::arrive::{Arrival, Arrived, arrive};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::config::Settings;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source};
use sqlx::PgPool;
use uuid::Uuid;

pub const STABLE_SECONDS: u64 = 10;
pub const DEFAULT_POLL_SECONDS: i32 = 60;
pub const CHANNEL: &str = "folder";
pub const DONE_DIR: &str = ".done";
pub const FAILED_DIR: &str = ".failed";
/// Files in the root (no profile subfolder) are filed under this name in `.done` and `.failed`.
const ROOT_BUCKET: &str = "_root";

/// The folder a source watches: OWNHEALTH_INBOX_DIR, or `config.subdir` below it.
pub fn root(settings: &Settings, source: &Source) -> Result<PathBuf> {
    let base = settings
        .inbox_dir
        .clone()
        .ok_or_else(|| Error::Invalid("OWNHEALTH_INBOX_DIR is not set".into()))?;
    Ok(match subdir(&source.config)? {
        Some(sub) => base.join(sub),
        None => base,
    })
}

/// `config.subdir`, a plain relative path, or None for the root.
fn subdir(config: &serde_json::Value) -> Result<Option<&Path>> {
    let Some(sub) = config.get("subdir").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let p = Path::new(sub);
    if !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(Error::Invalid("the folder subdir must be a plain relative path".into()));
    }
    Ok(Some(p))
}

/// Checks a `folder` source config before it is saved.
pub fn check_config(config: &serde_json::Value) -> Result<()> {
    subdir(config).map(|_| ())
}

/// A file ready to be taken.
#[derive(Debug, Clone)]
struct Ready {
    path: PathBuf,
    name: String,
    /// The profile subfolder, None in the root.
    slug: Option<String>,
    size: u64,
    mtime_ns: i128,
    /// (device, inode) at scan time; the handle we read must be the same file.
    id: (u64, u64),
}

impl Ready {
    fn key(&self) -> String {
        let dir = self.slug.as_deref().unwrap_or("");
        format!("{dir}/{}:{}:{}", self.name, self.size, self.mtime_ns)
    }

    fn bucket(&self) -> &str {
        self.slug.as_deref().unwrap_or(ROOT_BUCKET)
    }
}

fn skipped_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    name.starts_with('.')
        || name.starts_with('~')
        || [".part", ".partial", ".crdownload", ".tmp", ".download"]
            .iter()
            .any(|s| lower.ends_with(s))
}

fn nanos(t: SystemTime) -> i128 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

/// When anything last touched the file: the later of mtime and (on Unix) ctime.
fn last_change(meta: &std::fs::Metadata) -> std::io::Result<SystemTime> {
    let m = meta.modified()?;
    #[cfg(unix)]
    let m = {
        use std::os::unix::fs::MetadataExt;
        let c =
            SystemTime::UNIX_EPOCH + Duration::new(u64::try_from(meta.ctime()).unwrap_or(0), meta.ctime_nsec() as u32);
        m.max(c)
    };
    Ok(m)
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// Read a scanned file into the blob store from one open handle. None when the handle is not the
/// file the scan saw (swapped, for a link or anything else) or the file changed while we read it.
fn read_stable(blobs: &BlobStore, f: &Ready) -> std::io::Result<Option<(String, u64)>> {
    let file = std::fs::File::open(&f.path)?;
    let same = |m: &std::fs::Metadata| -> std::io::Result<bool> {
        Ok(m.is_file() && file_id(m) == f.id && m.len() == f.size && nanos(m.modified()?) == f.mtime_ns)
    };
    if !same(&file.metadata()?)? {
        return Ok(None);
    }
    let (sha, size) = blobs.put_reader(&file).map_err(std::io::Error::other)?;
    if size != f.size || !same(&file.metadata()?)? {
        return Ok(None);
    }
    Ok(Some((sha, size)))
}

/// The files of `root` and its profile subfolders that have been still for `stable`, and how many
/// are still being written.
fn scan(root: &Path, stable: Duration) -> std::io::Result<(Vec<Ready>, i32)> {
    let now = SystemTime::now();
    let (mut ready, mut waiting) = (Vec::new(), 0);
    let mut look =
        |dir: &Path, slug: Option<&str>, ready: &mut Vec<Ready>| -> std::io::Result<Vec<(PathBuf, String)>> {
            let mut subdirs = Vec::new();
            let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if skipped_name(&name) {
                    continue;
                }
                let meta = std::fs::symlink_metadata(e.path())?;
                if meta.is_dir() {
                    subdirs.push((e.path(), name));
                    continue;
                }
                if !meta.is_file() {
                    continue; // symlinks, sockets, devices
                }
                let changed = last_change(&meta)?;
                if now.duration_since(changed).unwrap_or(Duration::ZERO) < stable {
                    waiting += 1;
                    continue;
                }
                ready.push(Ready {
                    path: e.path(),
                    name,
                    slug: slug.map(str::to_string),
                    size: meta.len(),
                    mtime_ns: nanos(meta.modified()?),
                    id: file_id(&meta),
                });
            }
            Ok(subdirs)
        };
    for (dir, slug) in look(root, None, &mut ready)? {
        // one level of profile folders; deeper folders are not read
        look(&dir, Some(&slug), &mut ready)?;
    }
    Ok((ready, waiting))
}

/// A free path for `name` in `dir`: `name`, else `stem-1.ext`, `stem-2.ext`, ...
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    (1..)
        .map(|k| dir.join(format!("{stem}-{k}{ext}")))
        .find(|c| !c.exists())
        .unwrap_or(first)
}

/// Move a handled file to `.done/<bucket>/` or `.failed/<bucket>/` (with its reason).
fn file_away(root: &Path, f: &Ready, failed: Option<&str>) -> std::io::Result<()> {
    let dir = root
        .join(if failed.is_some() { FAILED_DIR } else { DONE_DIR })
        .join(f.bucket());
    std::fs::create_dir_all(&dir)?;
    let dest = free_path(&dir, &f.name);
    std::fs::rename(&f.path, &dest)?;
    if let Some(reason) = failed {
        let mut why = dest.clone().into_os_string();
        why.push(".reason.txt");
        std::fs::write(why, format!("{reason}\n"))?;
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> std::io::Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("folder task failed: {e}")))?
        .map_err(Error::from)
}

async fn profile_for(pool: &PgPool, source: &Source, slug: Option<&str>) -> Result<Uuid> {
    match slug {
        None => source
            .profile_id
            .ok_or_else(|| Error::Invalid("a file in the root needs a profile folder (no default profile)".into())),
        Some(s) => {
            let id: Option<Uuid> = sqlx::query_scalar("select id from profile where household_id = $1 and slug = $2")
                .bind(source.household_id)
                .bind(s)
                .fetch_optional(pool)
                .await?;
            id.ok_or_else(|| Error::Invalid("the folder name is not a profile of this household".into()))
        }
    }
}

/// What happened to one file: arrived (or a file that changed while we read it: None, retried).
async fn take(pool: &PgPool, blobs: &BlobStore, source: &Source, f: &Ready) -> Result<Option<Arrived>> {
    let profile_id = profile_for(pool, source, f.slug.as_deref()).await?;
    let (b, g) = (blobs.clone(), f.clone());
    let Some((sha, size)) = blocking(move || read_stable(&b, &g)).await? else {
        return Ok(None);
    };
    let key = f.key();
    let mut tx = pool.begin().await?;
    let a = arrive(
        &mut tx,
        blobs,
        &Arrival {
            source_id: Some(source.id),
            source_ref: Some(&f.name),
            external_id: Some(&key),
            filename: Some(&f.name),
            ..Arrival::new(
                source.household_id,
                profile_id,
                CHANNEL,
                &sha,
                i64::try_from(size).unwrap_or(i64::MAX),
            )
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Some(a))
}

/// One pass over a folder. `stable` is how long a file must be untouched (STABLE_SECONDS in
/// production). Every file ends in `.done` or `.failed`, except one still being written. When a
/// handled file cannot be moved (the folder is not writable by the app), the run fails after the
/// pass: its documents are committed and the per-source key stops a second import, but the source
/// must not look healthy while it reads the stuck files again every poll.
pub async fn poll_root(
    pool: &PgPool,
    blobs: &BlobStore,
    source: &Source,
    root: &Path,
    stable: Duration,
) -> Result<RunOutcome> {
    let dir = root.to_path_buf();
    if !blocking(move || Ok(dir.is_dir())).await? {
        return Err(Error::Invalid(
            "the inbox folder does not exist or is not a folder".into(),
        ));
    }
    let dir = root.to_path_buf();
    let (ready, _waiting) = blocking(move || scan(&dir, stable)).await?;
    let mut out = RunOutcome::default();
    let mut stuck: Option<(usize, String)> = None;
    for f in ready {
        let failed = match take(pool, blobs, source, &f).await {
            Ok(None) => continue,
            Ok(Some(a)) => {
                out.seen += 1;
                match a {
                    Arrived::Queued { .. } => {
                        out.new += 1;
                        None
                    }
                    Arrived::Duplicate { .. } | Arrived::AlreadySeen { .. } => {
                        out.duplicate += 1;
                        None
                    }
                    Arrived::Unsupported { .. } => Some("OwnHealth does not import this file type".to_string()),
                }
            }
            Err(e) => {
                out.seen += 1;
                Some(e.to_string())
            }
        };
        if let Some(reason) = &failed {
            out.problems.push(RunProblem {
                code: "file_failed".into(),
                message: format!("{reason}; moved to {FAILED_DIR}"),
            });
        }
        let (r, g) = (root.to_path_buf(), f.clone());
        let reason = failed.clone();
        if let Err(e) = blocking(move || file_away(&r, &g, reason.as_deref())).await {
            let n = stuck.as_ref().map_or(0, |(n, _)| *n);
            stuck = Some((n + 1, stuck.map_or_else(|| e.to_string(), |(_, first)| first)));
        }
    }
    match stuck {
        Some((n, first)) => Err(Error::Invalid(format!(
            "{n} handled file(s) could not be moved out of the inbox folder ({first}); the app needs \
             write access to it"
        ))),
        None => Ok(out),
    }
}

/// The `folder` poller.
pub async fn poll_folder(ctx: JobContext, source: Source) -> std::result::Result<RunOutcome, JobError> {
    let root = root(&ctx.settings, &source)?;
    Ok(poll_root(
        &ctx.pool,
        &ctx.blobs,
        &source,
        &root,
        Duration::from_secs(STABLE_SECONDS),
    )
    .await?)
}

/// Demo data (RUST-CONTRACTS 10.6, order 1020): the household's scanner folder. It is never polled:
/// the demo stack shares the checkout's `./inbox` with a real install, and a polling demo source
/// would take the real household's files (and move them to `.failed`).
async fn seed_demo(conn: &mut sqlx::PgConnection, ctx: &mut ownhealth_core::demo::DemoContext) -> Result<()> {
    ownhealth_core::sources::create_source(
        conn,
        &ownhealth_core::sources::NewSource {
            poll_seconds: None,
            ..ownhealth_core::sources::NewSource::new(ctx.family.household_id, "folder", "Escáner del salón")
        },
    )
    .await?;
    Ok(())
}

fn demo_hook<'c>(
    conn: &'c mut sqlx::PgConnection,
    ctx: &'c mut ownhealth_core::demo::DemoContext,
) -> futures::future::BoxFuture<'c, Result<()>> {
    Box::pin(seed_demo(conn, ctx))
}

pub fn register() {
    ownhealth_core::sources::register_poller("folder", |ctx, source| Box::pin(poll_folder(ctx, source)));
    ownhealth_core::demo::register_demo_hook("inbox_folder", 1020, demo_hook);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_and_hidden_files_are_left_alone() {
        for n in [
            ".DS_Store",
            "~$scan.pdf",
            "scan.pdf.part",
            "a.crdownload",
            "x.TMP",
            ".done",
        ] {
            assert!(skipped_name(n), "{n}");
        }
        for n in ["scan.pdf", "informe LabNorte.PDF", "foto.jpg"] {
            assert!(!skipped_name(n), "{n}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_link_after_the_scan_is_not_read() -> std::io::Result<()> {
        let d = tempfile::tempdir()?;
        let blobs = BlobStore::new(d.path().join("blobs"))?;
        let inbox = d.path().join("inbox");
        std::fs::create_dir_all(&inbox)?;
        // same length as the scanned file, so only the identity of the open file can tell them apart
        let secret = d.path().join("secret.txt");
        std::fs::write(&secret, b"clave-sintetica-0001")?;
        std::fs::write(inbox.join("scan.pdf"), b"%PDF-1.7 informe-0001")?;
        let (ready, _) = scan(&inbox, Duration::ZERO)?;
        let mut f = ready
            .into_iter()
            .next()
            .ok_or_else(|| std::io::Error::other("not scanned"))?;
        std::fs::write(&secret, b"%PDF-1.7 informe-000")?;
        let secret_mtime = nanos(std::fs::metadata(&secret)?.modified()?);
        f.mtime_ns = secret_mtime; // even a matching mtime does not help
        f.size = std::fs::metadata(&secret)?.len();
        std::fs::remove_file(&f.path)?;
        std::os::unix::fs::symlink(&secret, &f.path)?;
        assert_eq!(read_stable(&blobs, &f)?, None);
        Ok(())
    }

    #[test]
    fn a_taken_name_gets_a_number() -> std::io::Result<()> {
        let d = tempfile::tempdir()?;
        assert_eq!(free_path(d.path(), "a.pdf"), d.path().join("a.pdf"));
        std::fs::write(d.path().join("a.pdf"), b"x")?;
        std::fs::write(d.path().join("a-1.pdf"), b"x")?;
        assert_eq!(free_path(d.path(), "a.pdf"), d.path().join("a-2.pdf"));
        Ok(())
    }
}
