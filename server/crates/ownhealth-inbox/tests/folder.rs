//! Lane B: the watched folder. Files are taken once still, routed by profile subfolder, moved to
//! `.done` or `.failed` with a reason, never imported twice, and links and temporary files are left
//! alone. Synthetic LabNorte PDFs only.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use ownhealth_core::jobs::JobContext;
use ownhealth_core::sources::{self, NewSource, Source};
use ownhealth_core::testing::TestDb;
use ownhealth_inbox::folder::{self, poll_root};
use serde_json::json;
use sqlx::PgPool;

type R = anyhow::Result<()>;

fn pdf(report_id: &str) -> Vec<u8> {
    lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, LUCIA",
        birth: "02/03/2016",
        collected: "14/09/2026",
        report_id,
        pages: vec![vec![LabRow::new("Glucosa", "88", "mg/dL", "70 - 110")]],
    })
    .0
}

struct Env {
    db: TestDb,
    _dir: tempfile::TempDir,
    inbox: std::path::PathBuf,
    blobs: BlobStore,
    fam: Family,
    source: Source,
}

async fn env(default_profile: Option<&str>) -> anyhow::Result<Env> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path().join("blobs"))?;
    let inbox = dir.path().join("inbox");
    std::fs::create_dir_all(&inbox)?;
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, "Rivera").await?;
    let id = sources::create_source(
        &mut c,
        &NewSource {
            profile_id: default_profile.map(|p| fam.profile(p)),
            poll_seconds: Some(folder::DEFAULT_POLL_SECONDS),
            ..NewSource::new(fam.household_id, "folder", "Escáner del salón")
        },
    )
    .await?;
    let source = sources::get_source(&mut c, id).await?.expect("source");
    drop(c);
    Ok(Env {
        db,
        _dir: dir,
        inbox,
        blobs,
        fam,
        source,
    })
}

async fn channels(pool: &PgPool) -> anyhow::Result<Vec<(uuid::Uuid, String, Option<String>)>> {
    Ok(sqlx::query_as(
        "select d.profile_id, s.channel, s.source_ref from document_source s join document d on d.id = s.document_id \
         order by s.source_ref, s.received_at",
    )
    .fetch_all(pool)
    .await?)
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[tokio::test]
async fn files_route_by_profile_folder_and_move_to_done_or_failed() -> R {
    let e = env(Some("alex")).await?;
    let lucia = e.inbox.join("lucia");
    std::fs::create_dir_all(&lucia)?;
    std::fs::write(lucia.join("analitica.pdf"), pdf("LN-F1"))?;
    std::fs::write(e.inbox.join("suelto.pdf"), pdf("LN-F2"))?;
    std::fs::create_dir_all(e.inbox.join("nadie"))?;
    std::fs::write(e.inbox.join("nadie").join("x.pdf"), pdf("LN-F3"))?;
    std::fs::write(lucia.join("notas.txt"), b"una nota")?;
    // left alone: hidden, half-downloaded, a link to a file outside the inbox
    std::fs::write(lucia.join(".DS_Store"), b"x")?;
    std::fs::write(lucia.join("descarga.pdf.part"), pdf("LN-F4"))?;
    let outside = e._dir.path().join("secreto.pdf");
    std::fs::write(&outside, b"%PDF-1.4\n% un fichero fuera del buzon\n")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, lucia.join("enlace.pdf"))?;

    let o = poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::ZERO).await?;
    assert_eq!((o.seen, o.new, o.duplicate), (4, 2, 0), "{o:?}");
    assert_eq!(o.problems.len(), 2, "{o:?}");
    assert!(o.problems.iter().all(|p| p.code == "file_failed"), "{o:?}");

    let got = channels(&e.db.pool).await?;
    assert_eq!(
        got,
        vec![
            (e.fam.profile("lucia"), "folder".into(), Some("analitica.pdf".into())),
            (e.fam.profile("alex"), "folder".into(), Some("suelto.pdf".into())),
        ]
    );
    let jobs: i64 = sqlx::query_scalar("select count(*) from job where kind = 'import.pdf'")
        .fetch_one(&e.db.pool)
        .await?;
    assert_eq!(jobs, 2);

    assert_eq!(files_in(&e.inbox.join(".done/lucia")), ["analitica.pdf"]);
    assert_eq!(files_in(&e.inbox.join(".done/_root")), ["suelto.pdf"]);
    assert_eq!(files_in(&e.inbox.join(".failed/nadie")), ["x.pdf", "x.pdf.reason.txt"]);
    assert_eq!(
        files_in(&e.inbox.join(".failed/lucia")),
        ["notas.txt", "notas.txt.reason.txt"]
    );
    let why = std::fs::read_to_string(e.inbox.join(".failed/nadie/x.pdf.reason.txt"))?;
    assert!(why.contains("not a profile"), "{why}");
    // what is left in the inbox is exactly what was skipped
    let mut left = vec![".DS_Store", "descarga.pdf.part"];
    if cfg!(unix) {
        left.insert(2, "enlace.pdf");
    }
    assert_eq!(files_in(&lucia), left);
    assert!(outside.exists());
    // the link's target was never even read into the blob store
    use sha2::Digest as _;
    let outside_sha = hex::encode(sha2::Sha256::digest(std::fs::read(&outside)?));
    assert!(!e.blobs.exists(&outside_sha));

    // a second pass finds nothing new
    let o = poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::ZERO).await?;
    assert_eq!((o.seen, o.new), (0, 0), "{o:?}");
    Ok(())
}

#[tokio::test]
async fn a_file_still_being_written_waits_and_a_copy_is_one_document() -> R {
    let e = env(None).await?;
    let alex = e.inbox.join("alex");
    std::fs::create_dir_all(&alex)?;
    let bytes = pdf("LN-F6");
    std::fs::write(alex.join("scan.pdf"), &bytes)?;
    let o = poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::from_secs(3600)).await?;
    assert_eq!((o.seen, o.new), (0, 0), "{o:?}");
    assert_eq!(files_in(&alex), ["scan.pdf"]);

    // once still it is taken; the same bytes again under another name are one more source row
    poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::ZERO).await?;
    std::fs::write(alex.join("scan (1).pdf"), &bytes)?;
    let o = poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::ZERO).await?;
    assert_eq!((o.seen, o.new, o.duplicate), (1, 0, 1), "{o:?}");
    let docs: i64 = sqlx::query_scalar("select count(*) from document")
        .fetch_one(&e.db.pool)
        .await?;
    assert_eq!(docs, 1);
    assert_eq!(channels(&e.db.pool).await?.len(), 2);
    assert_eq!(files_in(&e.inbox.join(".done/alex")), ["scan (1).pdf", "scan.pdf"]);

    // a file in the root with no default profile fails with a reason
    std::fs::write(e.inbox.join("suelto.pdf"), pdf("LN-F7"))?;
    let o = poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::ZERO).await?;
    assert_eq!(o.problems.len(), 1, "{o:?}");
    assert!(e.inbox.join(".failed/_root/suelto.pdf.reason.txt").exists());
    Ok(())
}

#[tokio::test]
async fn a_file_that_cannot_be_moved_fails_the_run_and_is_not_imported_twice() -> R {
    let e = env(Some("alex")).await?;
    std::fs::write(e.inbox.join("informe.pdf"), pdf("LN-F8"))?;
    // `.done` is a file, so the move fails (as when the app cannot write to the folder)
    std::fs::write(e.inbox.join(".done"), b"")?;
    // the run fails, so the source turns red instead of reading the stuck file every minute as healthy
    let Err(err) = poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::ZERO).await else {
        panic!("a pass that could not move its files must fail");
    };
    assert!(err.to_string().contains("could not be moved"), "{err}");
    assert!(
        poll_root(&e.db.pool, &e.blobs, &e.source, &e.inbox, Duration::ZERO)
            .await
            .is_err()
    );
    let rows: i64 = sqlx::query_scalar("select count(*) from document_source")
        .fetch_one(&e.db.pool)
        .await?;
    assert_eq!(
        rows, 1,
        "the document committed once, and the source ledger stops a second row"
    );
    Ok(())
}

#[tokio::test]
async fn the_poller_reads_the_inbox_dir_setting_and_records_the_run() -> R {
    let e = env(Some("alex")).await?;
    std::fs::write(e.inbox.join("informe.pdf"), pdf("LN-F9"))?;
    let dir = tempfile::tempdir()?;
    let mut settings = e.db.settings(dir.path())?;
    let ctx = |s: ownhealth_core::config::Settings| JobContext::new(e.db.pool.clone(), Arc::new(s), e.blobs.clone());

    // no OWNHEALTH_INBOX_DIR: a clear error, nothing read
    settings.inbox_dir = None;
    let Err(err) = folder::poll_folder(ctx(settings.clone()), e.source.clone()).await else {
        panic!("a poll without OWNHEALTH_INBOX_DIR must fail");
    };
    assert!(err.text().contains("OWNHEALTH_INBOX_DIR"), "{}", err.text());
    // a subdir that climbs out is refused
    settings.inbox_dir = Some(e.inbox.clone());
    let mut climbing = e.source.clone();
    climbing.config = json!({"subdir": "../etc"});
    assert!(folder::poll_folder(ctx(settings.clone()), climbing).await.is_err());

    // through the scheduler's job: the run is recorded on the source (the file is still too fresh)
    folder::register();
    sources::register();
    let mut c = e.db.pool.acquire().await?;
    sources::enqueue_poll(&mut c, e.source.id).await?;
    drop(c);
    let kinds = [sources::POLL_JOB.to_string()];
    let job = ownhealth_core::jobs::run_one(&ctx(settings.clone()), "core", Some(&kinds)).await?;
    assert!(job.is_some(), "a source.poll job ran");
    assert!(
        e.inbox.join("informe.pdf").exists(),
        "a file written a moment ago waits"
    );
    let mut c = e.db.pool.acquire().await?;
    let health = sources::source_health(&mut c, e.fam.household_id).await?;
    assert!(health[0].last_success_at.is_some(), "{health:?}");
    assert_eq!(health[0].consecutive_failures, 0);
    Ok(())
}

#[tokio::test]
async fn the_demo_folder_source_never_polls_even_with_an_inbox_dir() -> R {
    // the demo stack shares ./inbox with a real install: a polling demo source would take its files
    folder::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let blobs = BlobStore::new(dir.path().join("blobs"))?;
    let mut settings = db.settings(dir.path())?;
    settings.inbox_dir = Some(dir.path().join("inbox"));
    assert!(ownhealth_core::demo::seed_demo(&db.pool, &settings, &blobs, false).await?);
    let rows: Vec<(String, Option<i32>)> =
        sqlx::query_as("select kind, poll_seconds from source where kind = 'folder'")
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(rows, vec![("folder".into(), None)]);
    let mut c = db.pool.acquire().await?;
    let due: i64 = sqlx::query_scalar("select count(*) from source where poll_seconds is not null and kind = 'folder'")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(due, 0);
    Ok(())
}
