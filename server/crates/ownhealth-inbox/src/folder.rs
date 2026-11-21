//! Lane B: the watched folder (ingestion.md 3, connectors.md 2.1). Polling, because inotify is
//! unreliable on SMB and NFS mounts. `<OWNHEALTH_INBOX_DIR>/<profile-slug>/<file>` goes to that
//! profile, when the person who set the source up manages that profile (owner, or guardian of an
//! unclaimed profile); a file in the folder of a profile they do not manage stays where it is, for the
//! source of someone who does. A file directly in the root goes to `source.profile_id`. A file is
//! taken once nothing has
//! written to it for STABLE_SECONDS (its mtime and ctime, so a copy that keeps the original mtime still
//! waits); after its document commits it moves to `<root>/.done/<slug>/`, on failure to
//! `<root>/.failed/<slug>/` next to a `<name>.reason.txt`, when the failure is about the file itself.
//! A failure of our own (the database or the blob store is down) leaves the file in place for the
//! next poll. The per-source dedup key is the relative path, size and mtime, so a file that could not
//! be moved is not imported twice.
//!
//! Hidden files, temporary downloads (`.part`, `.crdownload`, `.tmp`, `~$...`) and symlinks are
//! skipped: a link could point anywhere the server can read. The file is opened once, without
//! following a link and without waiting on a pipe, and checked on the open handle (a regular file,
//! same inode as the scan saw, same size and mtime), so a file swapped for a link or a FIFO between
//! the scan and the read is refused, not followed. Source `config` (kind `folder`): `{}` (the root is
//! OWNHEALTH_INBOX_DIR) or `{"subdir": "..."}`, a relative path below it. The subdir may not start
//! with a hidden folder (`.done`, `.failed`) or a profile's folder, so a source cannot read another
//! source's archive or another person's drops as its own root files, and two folder sources never share a
//! root or sit one inside the other ([`check_overlap`]). The demo never reads the folder:
//! the demo stack shares `./inbox` with a real install.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use ownhealth_core::arrive::{Arrival, Arrived, arrive};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::config::Settings;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::jobs::{EnqueueOptions, HandlerResult, Job, JobContext, JobError, enqueue, register_handler};
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
    if p.components()
        .any(|c| c.as_os_str().to_str().is_none_or(|n| n.starts_with('.')))
    {
        return Err(Error::Invalid(
            "the folder subdir may not name a hidden folder such as .done or .failed".into(),
        ));
    }
    Ok(Some(p))
}

/// After a profile is deleted, remove its files from a folder source: its drop folder
/// `<root>/<slug>/` and the copies filed in `.done/<slug>/` and `.failed/<slug>/` when `slug_folders`,
/// plus `.done/_root/` and `.failed/_root/` when `root_buckets` (the source was the profile's own, so its
/// root files went to that profile). [`forget_folders`] decides both, so a new person or a new folder
/// inbox that took over a name keeps its files. The paths are built from the checked subdir and slug
/// only. A root that leads out of the inbox folder is skipped, never an error: OwnHealth never filed
/// anything there. Nothing below the root is followed: a link in the way is removed, never its target,
/// and a folder reached through a link below the root is left alone.
pub fn forget_profile(
    settings: &Settings,
    config: &serde_json::Value,
    slug: &str,
    slug_folders: bool,
    root_buckets: bool,
) -> Result<()> {
    if slug.is_empty()
        || !slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error::Invalid("not a profile's short name".into()));
    }
    let Some(base) = settings.inbox_dir.clone() else {
        return Ok(());
    };
    let root = match subdir(config)? {
        Some(sub) => base.join(sub),
        None => base.clone(),
    };
    // the same test as check_root: the root must resolve inside the inbox folder
    let root = match (root.canonicalize(), base.canonicalize()) {
        (Ok(root), Ok(base)) if root.starts_with(&base) => root,
        (Ok(_), Ok(_)) => {
            tracing::warn!("a folder inbox leads out of the inbox folder: its folders are left alone");
            return Ok(());
        }
        // no folder, so nothing to remove
        (Err(e), _) | (_, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        (Err(e), _) | (_, Err(e)) => return Err(e.into()),
    };
    let mut gone = Vec::new();
    if slug_folders {
        gone.extend([vec![slug], vec![DONE_DIR, slug], vec![FAILED_DIR, slug]]);
    }
    if root_buckets {
        gone.extend([vec![DONE_DIR, ROOT_BUCKET], vec![FAILED_DIR, ROOT_BUCKET]]);
    }
    let mut first = None;
    for parts in gone {
        if let Err(e) = remove_below(&root, &parts) {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), |e| Err(e.into()))
}

/// The job that retries removing a deleted profile's files from the folder inboxes when the first try
/// after the delete failed (a busy or read-only disk).
pub const FORGET_JOB: &str = "folder.forget";

/// Queue the retry of [`forget_profile`] for each folder inbox (its settings, and whether it was the
/// profile's own), in the delete's own transaction. It waits a minute, so the delete's own try comes
/// first; the caller then drops it, or keeps it when a folder could not be cleaned. The payload holds the
/// household (a retry that runs out of attempts shows in its review queue), the short name and the
/// inbox settings, never a file name.
pub async fn queue_forget(
    conn: &mut sqlx::PgConnection,
    household_id: Uuid,
    slug: &str,
    folders: &[(serde_json::Value, bool)],
) -> Result<Option<i64>> {
    let folders: Vec<serde_json::Value> = folders
        .iter()
        .map(|(config, own)| serde_json::json!({"config": config, "own": own}))
        .collect();
    Ok(enqueue(
        conn,
        FORGET_JOB,
        &serde_json::json!({"household_id": household_id, "slug": slug, "folders": folders}),
        EnqueueOptions {
            run_after: Some(chrono::Utc::now() + chrono::Duration::seconds(60)),
            ..EnqueueOptions::default()
        },
    )
    .await?)
}

/// Run [`forget_profile`] on each folder inbox off the async runtime; returns the ones that failed.
/// The short name is free again once the delete commits, so it checks each folder first, on the
/// delete's own try and on every retry. It keeps `<slug>/` and its filed copies when a profile with that
/// short name now lives in the household, or in a household whose folder inbox reads the same folder,
/// or when a folder inbox now reads a folder inside `<slug>/`. It keeps `_root` when a folder inbox, in
/// any household, now reads that folder. "The same folder" is the real one on disk ([`what_to_remove`]).
pub async fn forget_folders(
    pool: &PgPool,
    settings: &Settings,
    household_id: Uuid,
    slug: &str,
    folders: Vec<(serde_json::Value, bool)>,
) -> Vec<(serde_json::Value, bool)> {
    if folders.is_empty() {
        return folders;
    }
    // the households with a profile of that short name, and every folder inbox with its household
    let named: sqlx::Result<Vec<Uuid>> = sqlx::query_scalar("select household_id from profile where slug = $1")
        .bind(slug)
        .fetch_all(pool)
        .await;
    let readers: sqlx::Result<Vec<(Uuid, serde_json::Value)>> =
        sqlx::query_as("select household_id, config from source where kind = 'folder'")
            .fetch_all(pool)
            .await;
    let (named, readers) = match (named, readers) {
        (Ok(named), Ok(readers)) => (named, readers),
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(error = %e, "could not check who uses the folders now: a job retries them");
            return folders;
        }
    };
    let readers: Vec<(Uuid, PathBuf)> = readers
        .into_iter()
        .filter_map(|(h, config)| rel(&config).map(|p| (h, p)))
        .collect();
    let (named, readers) = (std::sync::Arc::new(named), std::sync::Arc::new(readers));
    let mut failed = Vec::new();
    for (config, own) in folders {
        let (s, c, sl) = (settings.clone(), config.clone(), slug.to_string());
        let (named, readers) = (named.clone(), readers.clone());
        // the checks read the disk too, so they run off the async runtime with the removal
        let run = move || {
            let (slug_folders, root_buckets) = what_to_remove(&s, household_id, &sl, &named, &readers, &c, own);
            forget_profile(&s, &c, &sl, slug_folders, root_buckets)
        };
        match tokio::task::spawn_blocking(run).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "removing a deleted profile's folder failed: a job retries it");
                failed.push((config, own));
            }
            Err(e) => {
                tracing::warn!(error = %e, "removing a deleted profile's folder failed: a job retries it");
                failed.push((config, own));
            }
        }
    }
    failed
}

/// A folder inbox's settings as a path below the inbox folder ("" for the root), or None when they do
/// not parse.
fn rel(config: &serde_json::Value) -> Option<PathBuf> {
    subdir(config)
        .ok()
        .map(|p| p.map(Path::to_path_buf).unwrap_or_default())
}

/// What one folder inbox may lose of a deleted profile: (`slug_folders`, `root_buckets`) for
/// [`forget_profile`], by the rules of [`forget_folders`]. Another inbox reads the same folder, or a
/// folder inside the drop, when its settings say so or when its real folder on disk does: through a
/// link, or a letter-case variant on a file system that ignores case. An inbox folder whose place
/// cannot be read counts as the same folder, so the files stay.
fn what_to_remove(
    settings: &Settings,
    household_id: Uuid,
    slug: &str,
    named: &[Uuid],
    readers: &[(Uuid, PathBuf)],
    config: &serde_json::Value,
    own: bool,
) -> (bool, bool) {
    let Some(mine) = rel(config) else {
        // forget_profile refuses the settings itself
        return (true, own);
    };
    let Some(base) = settings.inbox_dir.as_deref() else {
        // no inbox folder, so forget_profile has nothing to remove
        return (false, false);
    };
    let drop = mine.join(slug);
    let (mine_at, drop_at) = (real(&base.join(&mine)), real(&base.join(&drop)));
    // whether a reader's real folder is `to` (`exact`) or a folder inside it
    let leads = |r: &Path, to: &Option<PathBuf>, exact: bool| match (real(&base.join(r)), to) {
        (Some(at), Some(to)) if exact => at == *to,
        (Some(at), Some(to)) => at.starts_with(to),
        _ => true,
    };
    let on_it: Vec<Uuid> = readers
        .iter()
        .filter(|(_, r)| *r == mine || leads(r, &mine_at, true))
        .map(|(h, _)| *h)
        .collect();
    let taken = named.iter().any(|h| *h == household_id || on_it.contains(h));
    let inside = readers
        .iter()
        .any(|(_, r)| r.starts_with(&drop) || leads(r, &drop_at, false));
    (!taken && !inside, own && on_it.is_empty())
}

/// Where `path` leads on disk: the real path of the longest part of it that exists (links followed,
/// letter case as stored), then the rest as written, which is where a folder not made yet would be
/// made. None when a part of it cannot be read.
fn real(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        match at.canonicalize() {
            Ok(found) => return Some(rest.iter().rev().fold(found, |p, part| p.join(part))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                rest.push(at.file_name()?);
                at = at.parent()?;
            }
            Err(_) => return None,
        }
    }
}

async fn forget_job(ctx: JobContext, job: Job) -> HandlerResult {
    // The demo never reads a folder, and its stack shares the inbox folder with the real install.
    if ctx.settings.demo {
        return Ok(Some(serde_json::json!({"skipped": "demo"})));
    }
    // a job that ended once has no short name left: nothing to redo
    let Some(slug) = job.str("slug") else {
        return Ok(Some(serde_json::json!({"skipped": "no short name"})));
    };
    let household_id = job.uuid("household_id")?;
    let folders: Vec<(serde_json::Value, bool)> = job
        .payload
        .get("folders")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .map(|f| {
                    let own = f.get("own").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    (f.get("config").cloned().unwrap_or_default(), own)
                })
                .collect()
        })
        .unwrap_or_default();
    let n = folders.len();
    let failed = forget_folders(&ctx.pool, &ctx.settings, household_id, slug, folders).await;
    let last = failed.is_empty() || job.attempts >= job.max_attempts;
    if last {
        // the job ends now, done or dead, and its row stays: it keeps no trace of the person's name
        sqlx::query("update job set payload = payload - 'slug' where id = $1")
            .bind(job.id)
            .execute(&ctx.pool)
            .await?;
    }
    if !failed.is_empty() {
        return Err(JobError::retry(format!(
            "{} folder inboxes could not be cleaned yet",
            failed.len()
        )));
    }
    Ok(Some(serde_json::json!({"folders": n})))
}

/// Remove `root/<parts>` without following a link: a link among the parents stops it, and a link at
/// the end is removed itself.
fn remove_below(root: &Path, parts: &[&str]) -> std::io::Result<()> {
    let mut path = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let last = i + 1 == parts.len();
        if meta.file_type().is_symlink() {
            return if last { std::fs::remove_file(&path) } else { Ok(()) };
        }
        if !meta.is_dir() {
            return Ok(());
        }
        if last {
            return std::fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

/// Checks a `folder` source config before it is saved.
pub fn check_config(config: &serde_json::Value) -> Result<()> {
    subdir(config).map(|_| ())
}

/// Refuses a `folder` source whose root equals, contains or sits inside the root of another `folder`
/// source on this server (`source_id` is the one being saved, None for a new one). Two sources on
/// one folder would race for its files and file one person's drops under the other's profile.
pub async fn check_overlap(
    conn: &mut sqlx::PgConnection,
    source_id: Option<Uuid>,
    config: &serde_json::Value,
) -> Result<()> {
    let mine = subdir(config)?.map(Path::to_path_buf).unwrap_or_default();
    let others: Vec<serde_json::Value> =
        sqlx::query_scalar("select config from source where kind = 'folder' and id is distinct from $1")
            .bind(source_id)
            .fetch_all(conn)
            .await?;
    for other in others {
        // a stored config that no longer parses reads nothing, so it is in nobody's way
        let Ok(theirs) = subdir(&other).map(|p| p.map(Path::to_path_buf).unwrap_or_default()) else {
            continue;
        };
        if mine.starts_with(&theirs) || theirs.starts_with(&mine) {
            return Err(Error::Invalid(
                "another folder inbox already reads this folder, a folder inside it or a folder around it".into(),
            ));
        }
    }
    Ok(())
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

#[cfg(unix)]
const ELOOP: i32 = libc::ELOOP;
#[cfg(not(unix))]
const ELOOP: i32 = -1;

/// Open for reading without following a final link and without blocking on a FIFO that has no
/// writer (a plain `open` would hang the poll for good). A regular file ignores O_NONBLOCK.
#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Read a scanned file into the blob store from one open handle. None when the handle is not the
/// file the scan saw (swapped, for a link or anything else) or the file changed while we read it.
fn read_stable(blobs: &BlobStore, f: &Ready) -> std::io::Result<Option<(String, u64)>> {
    let file = match open_no_follow(&f.path) {
        Ok(file) => file,
        // a link put in its place (O_NOFOLLOW), or the file is gone: not the file the scan saw
        Err(e) if e.raw_os_error() == Some(ELOOP) || e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
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

/// The metadata of one directory entry, without following a link; None when the entry went away
/// between the listing and the look (someone moved or deleted it): the next poll sees the folder again.
fn entry_meta(path: &Path) -> std::io::Result<Option<(std::fs::Metadata, SystemTime)>> {
    let gone = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(e)
        }
    };
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return gone(e),
    };
    if !meta.is_file() {
        return Ok(Some((meta, SystemTime::UNIX_EPOCH)));
    }
    match last_change(&meta) {
        Ok(t) => Ok(Some((meta, t))),
        Err(e) => gone(e),
    }
}

/// What one scan found.
#[derive(Debug, Default)]
struct Scan {
    ready: Vec<Ready>,
    /// Directory entries that could not be read at all.
    unreadable: i32,
}

/// The files of `root` and its profile subfolders that have been still for `stable`, and how many
/// entries could not be read.
fn scan(root: &Path, stable: Duration) -> std::io::Result<Scan> {
    let now = SystemTime::now();
    let (mut ready, mut unreadable) = (Vec::new(), 0);
    let mut look =
        |dir: &Path, slug: Option<&str>, ready: &mut Vec<Ready>| -> std::io::Result<Vec<(PathBuf, String)>> {
            let mut subdirs = Vec::new();
            let mut entries = Vec::new();
            for e in std::fs::read_dir(dir)? {
                match e {
                    Ok(e) => entries.push(e),
                    Err(_) => unreadable += 1,
                }
            }
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if skipped_name(&name) {
                    continue;
                }
                let Some((meta, changed)) = entry_meta(&e.path())? else {
                    continue;
                };
                if meta.is_dir() {
                    subdirs.push((e.path(), name));
                    continue;
                }
                if !meta.is_file() {
                    continue; // symlinks, sockets, devices
                }
                if now.duration_since(changed).unwrap_or(Duration::ZERO) < stable {
                    continue; // still being written
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
        match look(&dir, Some(&slug), &mut ready) {
            Ok(_) => {}
            // the folder went away between the two listings
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Scan { ready, unreadable })
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

/// Where a file goes.
enum Route {
    Profile(Uuid),
    /// A profile folder of someone the source's creator does not manage: left for their own source.
    NotManaged,
}

/// The profile a file goes to: the source's profile for a root file, else the profile named by the
/// folder, when the person who set the source up still manages it (the same right the Paperless tag
/// and sender rules need).
async fn profile_for(pool: &PgPool, source: &Source, slug: Option<&str>) -> Result<Route> {
    let Some(s) = slug else {
        return source
            .profile_id
            .map(Route::Profile)
            .ok_or_else(|| Error::Invalid("a file in the root needs a profile folder (no default profile)".into()));
    };
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        "select p.id, exists (select 1 from profile_grant g where g.profile_id = p.id and g.user_id = $3 \
         and (g.expires_at is null or g.expires_at > now()) \
         and (g.role = 'owner' or (g.role = 'guardian' and p.user_id is null))) \
         from profile p where p.household_id = $1 and p.slug = $2",
    )
    .bind(source.household_id)
    .bind(s)
    .bind(source.created_by)
    .fetch_optional(pool)
    .await?;
    match row {
        Some((id, true)) => Ok(Route::Profile(id)),
        Some((_, false)) => Ok(Route::NotManaged),
        None => Err(Error::Invalid(
            "the folder name is not a profile of this household".into(),
        )),
    }
}

/// Our own failure (database, disk, blob store), not the file's: leave the file for the next poll.
fn transient(e: &Error) -> bool {
    matches!(e, Error::Db(_) | Error::Io(_) | Error::Other(_))
}

/// What became of one file.
enum Taken {
    Arrived(Arrived),
    /// Changed while we read it, or not ours to route: left in place.
    Left,
    NotManaged,
}

/// What happened to one file.
async fn take(pool: &PgPool, blobs: &BlobStore, source: &Source, f: &Ready) -> Result<Taken> {
    let profile_id = match profile_for(pool, source, f.slug.as_deref()).await? {
        Route::Profile(id) => id,
        Route::NotManaged => return Ok(Taken::NotManaged),
    };
    let (b, g) = (blobs.clone(), f.clone());
    let Some((sha, size)) = blocking(move || read_stable(&b, &g)).await? else {
        return Ok(Taken::Left);
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
    Ok(Taken::Arrived(a))
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
    let found = blocking(move || scan(&dir, stable)).await?;
    let mut out = RunOutcome::default();
    if found.unreadable > 0 {
        out.problems.push(RunProblem {
            code: "entry_unreadable".into(),
            message: format!("{} folder entries could not be read", found.unreadable),
        });
    }
    let mut stuck: Option<(usize, String)> = None;
    let mut not_managed: Vec<String> = Vec::new();
    for f in found.ready {
        let failed = match take(pool, blobs, source, &f).await {
            Ok(Taken::Left) => continue,
            Ok(Taken::NotManaged) => {
                let slug = f.bucket().to_string();
                if !not_managed.contains(&slug) {
                    out.problems.push(RunProblem {
                        code: "folder_not_managed".into(),
                        message: format!(
                            "files in {slug}/ were left in place: whoever set up this source does not manage that profile"
                        ),
                    });
                    not_managed.push(slug);
                }
                continue;
            }
            Err(e) if transient(&e) => {
                out.seen += 1;
                out.problems.push(RunProblem {
                    code: "file_retry".into(),
                    message: format!("{e}; left in place for the next poll"),
                });
                continue;
            }
            Ok(Taken::Arrived(a)) => {
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

/// A subdir must not lead (through a link on the share either) into a hidden folder or a profile's
/// folder of the inbox: its files would count as this source's root files and go to its profile.
async fn check_root(pool: &PgPool, settings: &Settings, source: &Source, root: &Path) -> Result<()> {
    if subdir(&source.config)?.is_none() {
        return Ok(());
    }
    let (base, dir) = (settings.inbox_dir.clone(), root.to_path_buf());
    let first: Option<String> = blocking(move || {
        let (Some(base), Ok(dir)) = (base, dir.canonicalize()) else {
            return Ok(None);
        };
        let base = base.canonicalize()?;
        Ok(match dir.strip_prefix(&base) {
            Ok(rel) => rel
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned()),
            // a link out of the inbox folder altogether
            Err(_) => Some(".".into()),
        })
    })
    .await?;
    let Some(first) = first else { return Ok(()) };
    let slug_taken: bool =
        sqlx::query_scalar("select exists (select 1 from profile where household_id = $1 and slug = $2)")
            .bind(source.household_id)
            .bind(&first)
            .fetch_one(pool)
            .await?;
    if first.starts_with('.') || slug_taken {
        return Err(Error::Invalid(
            "the folder subdir leads into a hidden folder, a profile's folder or out of the inbox folder".into(),
        ));
    }
    Ok(())
}

/// The `folder` poller.
pub async fn poll_folder(ctx: JobContext, source: Source) -> std::result::Result<RunOutcome, JobError> {
    if ctx.settings.demo {
        return Err(JobError::permanent(
            "the demo never reads the inbox folder: it shares ./inbox with a real install",
        ));
    }
    let root = root(&ctx.settings, &source)?;
    check_root(&ctx.pool, &ctx.settings, &source, &root).await?;
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
/// would take the real household's files (and move them to `.failed`). It is off, so "Sync now" is
/// off too, and `poll_folder` refuses to run in the demo even if someone turns it on.
async fn seed_demo(conn: &mut sqlx::PgConnection, ctx: &mut ownhealth_core::demo::DemoContext) -> Result<()> {
    let id = ownhealth_core::sources::create_source(
        &mut *conn,
        &ownhealth_core::sources::NewSource {
            poll_seconds: None,
            ..ownhealth_core::sources::NewSource::new(ctx.family.household_id, "folder", "Escáner del salón")
        },
    )
    .await?;
    sqlx::query("update source set enabled = false where id = $1")
        .bind(id)
        .execute(conn)
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
    register_handler(FORGET_JOB, |ctx, job| Box::pin(forget_job(ctx, job)));
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
        let mut f = scan(&inbox, Duration::ZERO)?
            .ready
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

    /// A FIFO with no writer put in place of a scanned file: a blocking open would hang the poll
    /// for good. The read must give up at once.
    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_fifo_after_the_scan_does_not_hang_the_poll() -> std::io::Result<()> {
        let d = tempfile::tempdir()?;
        let blobs = BlobStore::new(d.path().join("blobs"))?;
        let inbox = d.path().join("inbox");
        std::fs::create_dir_all(&inbox)?;
        std::fs::write(inbox.join("scan.pdf"), b"%PDF-1.7 informe-0002")?;
        let f = scan(&inbox, Duration::ZERO)?
            .ready
            .into_iter()
            .next()
            .ok_or_else(|| std::io::Error::other("not scanned"))?;
        std::fs::remove_file(&f.path)?;
        let status = std::process::Command::new("mkfifo").arg(&f.path).status()?;
        assert!(status.success(), "mkfifo");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_stable(&blobs, &f).map_err(|e| e.to_string()));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| std::io::Error::other("the read hung on the FIFO"))?;
        assert_eq!(got, Ok(None));
        Ok(())
    }

    #[test]
    fn an_entry_that_vanished_after_the_listing_is_skipped() -> std::io::Result<()> {
        let d = tempfile::tempdir()?;
        assert!(entry_meta(&d.path().join("gone.pdf"))?.is_none());
        std::fs::write(d.path().join("here.pdf"), b"x")?;
        assert!(entry_meta(&d.path().join("here.pdf"))?.is_some());
        Ok(())
    }

    #[test]
    fn a_subdir_may_not_name_a_hidden_folder() {
        for bad in [".done/ortega", ".failed", "scans/.done", "../etc", "/etc"] {
            assert!(check_config(&serde_json::json!({"subdir": bad})).is_err(), "{bad}");
        }
        for ok in ["escaner", "casa/escaner"] {
            assert!(check_config(&serde_json::json!({"subdir": ok})).is_ok(), "{ok}");
        }
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
