//! The weekly ClinVar check (clinvar_fetch.rs) against a fake HTTP server that serves a synthetic mini
//! release: the poller fetches, diffs and deletes the file; with the switch off nothing is fetched; a
//! failed fetch shows on the source's health and writes no event. All data is synthetic.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Env, R, env};
use noodles::bgzf;
use ownhealth_core::demo::create_rivera_family;
use ownhealth_core::jobs::{self, JobContext};
use ownhealth_core::sources::{self, NewSource, Source, SourcePatch};
use ownhealth_genome::clinvar_fetch;
use ownhealth_genome::synthetic;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// One planted record Alex carries (synthetic.rs alex_facts, MTHFR chr1:11796321 G>A): Likely
/// pathogenic with 1 star, which no fact reports, so the first run against the facts alerts.
const RELEASE: &str = "##fileformat=VCFv4.1\n##fileDate=20261005\n##source=ClinVar (synthetic test release)\n\
#CHROM\tPOS\tID\tREF\tALT\tQUAL\tFILTER\tINFO\n\
1\t11796321\t900030\tG\tA\t.\t.\tALLELEID=900030;CLNREVSTAT=criteria_provided,_single_submitter;CLNSIG=Likely_pathogenic;GENEINFO=MTHFR:1\n";

fn bgzipped(text: &str) -> anyhow::Result<Vec<u8>> {
    let mut w = bgzf::io::Writer::new(Vec::new());
    w.write_all(text.as_bytes())?;
    Ok(w.finish()?)
}

/// A server that answers every GET with `status` and `body`, and counts the requests.
async fn serve(status: u16, body: Vec<u8>) -> anyhow::Result<(String, Arc<AtomicUsize>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes()).await;
            let _ = s.write_all(&body).await;
        }
    });
    Ok((
        format!("http://{addr}/pub/clinvar/vcf_GRCh38/weekly/clinvar.vcf.gz"),
        hits,
    ))
}

struct Fx {
    e: Env,
    ctx: JobContext,
}

async fn fx(demo: bool) -> anyhow::Result<Fx> {
    ownhealth_core::sources::register();
    ownhealth_genome::register();
    let e = env().await?;
    let mut settings = e.db.settings(e.dir.path())?;
    settings.demo = demo;
    let ctx = JobContext::new(e.db.pool.clone(), Arc::new(settings), e.blobs.clone());
    Ok(Fx { e, ctx })
}

impl Fx {
    /// Alex's facts, no calls VCF: the diff reads the alt sites the facts cite.
    async fn alex_genome(&self) -> anyhow::Result<()> {
        let mut c = self.e.conn().await?;
        self.e.seed(&mut c, "alex", &synthetic::alex_facts(), &[]).await?;
        Ok(())
    }

    async fn source(&self, config: Value, enabled: bool) -> anyhow::Result<Source> {
        let mut c = self.e.conn().await?;
        let id = sources::create_source(
            &mut c,
            &NewSource {
                config,
                poll_seconds: Some(clinvar_fetch::POLL_SECONDS),
                ..NewSource::new(self.e.fam.household_id, "clinvar", "ClinVar")
            },
        )
        .await?;
        let patch = SourcePatch {
            enabled: Some(enabled),
            ..SourcePatch::default()
        };
        Ok(sources::update_source(&mut c, id, &patch).await?)
    }

    /// One `source.poll` through the real job path; the run it recorded.
    async fn poll(&self, source: Uuid) -> anyhow::Result<Option<sources::SourceRun>> {
        let mut c = self.e.conn().await?;
        sources::enqueue_poll(&mut c, source).await?;
        jobs::drain(&self.ctx, "core", 10, None).await?;
        Ok(sources::list_runs(&mut c, source, 1).await?.into_iter().next())
    }

    async fn scalar(&self, sql: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_one(&self.e.db.pool)
            .await?)
    }

    fn tmp_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        Ok(std::fs::read_dir(self.e.blobs.root().join("tmp"))?
            .filter_map(|d| d.ok().map(|d| d.path()))
            .collect())
    }
}

#[tokio::test]
async fn the_weekly_poll_fetches_the_release_diffs_it_and_deletes_the_file() -> R {
    let f = fx(false).await?;
    f.alex_genome().await?;
    // Another household's genome: its own switch decides, so this household's run leaves it alone.
    let other = {
        let mut c = f.e.conn().await?;
        let fam = create_rivera_family(&mut c, "Ortega").await?;
        let mut facts = synthetic::alex_facts();
        facts["subject"]["sample_id"] = json!("CO-02");
        synthetic::seed_genome(
            &mut c,
            &f.e.blobs,
            fam.household_id,
            fam.profile("alex"),
            &facts,
            &[],
            "g.json",
            None,
        )
        .await?;
        fam.household_id
    };
    let (url, hits) = serve(200, bgzipped(RELEASE)?).await?;
    let s = f.source(json!({"url": url, "allow_private": true}), true).await?;

    let run = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
    assert_eq!(
        (run.status.as_str(), run.items_seen, run.items_new),
        ("ok", 1, 1),
        "{run:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let alex = f.e.profile("alex");
    assert_eq!(
        f.scalar(&format!(
            "select count(*) from genome_event where profile_id = '{alex}' and variation_id = 900030 and alerted"
        ))
        .await?,
        1
    );
    assert_eq!(
        f.scalar(&format!(
            "select count(*) from review_item where profile_id = '{alex}' and kind = 'genome_reinterpretation'"
        ))
        .await?,
        1
    );
    assert!(
        f.tmp_files()?.is_empty(),
        "the downloaded release is deleted: {:?}",
        f.tmp_files()?
    );
    let mut c = f.e.conn().await?;
    let after = sources::get_source(&mut c, s.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("gone"))?;
    assert!(after.last_success_at.is_some());
    assert_eq!(
        after.cursor,
        Some(json!({"release": "2026-10-05", "events": 1, "alerts": 1}))
    );
    assert_eq!(
        f.scalar(&format!(
            "select count(*) from genome_dataset d join profile p on p.id = d.profile_id \
             where p.household_id = '{other}' and d.clinvar_compared is not null"
        ))
        .await?,
        0,
        "another household's genome is compared only by its own source"
    );

    // The same release a week later: already compared, nothing new, no second alert.
    let again = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
    assert_eq!(
        (again.status.as_str(), again.items_new, again.items_duplicate),
        ("ok", 0, 1)
    );
    assert_eq!(f.scalar("select count(*) from genome_event").await?, 1);
    assert!(f.tmp_files()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn with_the_switch_off_nothing_is_fetched() -> R {
    let f = fx(false).await?;
    f.alex_genome().await?;
    let (url, hits) = serve(200, bgzipped(RELEASE)?).await?;
    let s = f.source(json!({"url": url, "allow_private": true}), false).await?;

    let mut c = f.e.conn().await?;
    assert!(
        sources::schedule_due(&mut c).await?.is_empty(),
        "a switched-off source is never scheduled"
    );
    // A poll queued before the switch went off finds it off and runs nothing.
    assert!(f.poll(s.id).await?.is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(f.scalar("select count(*) from genome_event").await?, 0);
    // The default never turns a switched-off source back on, nor adds a second one.
    assert_eq!(sources::add_default_sources(&f.ctx).await?, 0);
    assert_eq!(
        f.scalar("select count(*) from source where kind = 'clinvar' and not enabled")
            .await?,
        1
    );
    assert_eq!(f.scalar("select count(*) from source where kind = 'clinvar'").await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_failed_fetch_shows_on_the_health_row_and_writes_no_event() -> R {
    let f = fx(false).await?;
    f.alex_genome().await?;
    let (url, hits) = serve(503, b"busy".to_vec()).await?;
    let s = f.source(json!({"url": url, "allow_private": true}), true).await?;

    let run = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
    assert_eq!(run.status, "error");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let mut c = f.e.conn().await?;
    let health = sources::source_health(&mut c, f.e.fam.household_id).await?;
    let row = health
        .iter()
        .find(|h| h.id == s.id)
        .ok_or_else(|| anyhow::anyhow!("no row"))?;
    assert_eq!(row.consecutive_failures, 1);
    assert_eq!(row.last_run_status.as_deref(), Some("error"));
    assert_eq!(
        row.last_error.as_deref(),
        Some("JobError: clinvar: the server answered 503")
    );
    assert_eq!(f.scalar("select count(*) from genome_event").await?, 0);
    assert_eq!(
        f.scalar("select count(*) from review_item where kind = 'genome_reinterpretation'")
            .await?,
        0
    );
    assert!(f.tmp_files()?.is_empty());

    // A truncated release (half a bgzip file) also fails the run before any event is written.
    let gz = bgzipped(RELEASE)?;
    let (cut, _) = serve(200, gz[..gz.len() / 2].to_vec()).await?;
    sources::update_source(
        &mut c,
        s.id,
        &SourcePatch {
            config: Some(json!({"url": cut, "allow_private": true})),
            ..SourcePatch::default()
        },
    )
    .await?;
    let run = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
    assert_eq!(run.status, "error", "{run:?}");
    assert_eq!(f.scalar("select count(*) from genome_event").await?, 0);
    assert!(f.tmp_files()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_failed_fetch_is_tried_again_within_hours_not_a_week() -> R {
    let f = fx(false).await?;
    f.alex_genome().await?;
    let (url, _) = serve(503, b"busy".to_vec()).await?;
    let s = f.source(json!({"url": url, "allow_private": true}), true).await?;
    let mut c = f.e.conn().await?;
    let ran = |ago: &str| {
        format!(
            "update source set last_run_at = now() - interval '{ago}' where id = '{}'",
            s.id
        )
    };
    for (failures, waited, due) in [
        (1, "50 minutes", false),
        (1, "61 minutes", true),
        (2, "61 minutes", false),
        (2, "121 minutes", true),
        // the wait doubles up to 64 hours, still sooner than the weekly poll
        (20, "63 hours", false),
        (20, "65 hours", true),
    ] {
        let run = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
        assert_eq!(run.status, "error");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "update source set consecutive_failures = {failures} where id = '{}'",
            s.id
        )))
        .execute(&mut *c)
        .await?;
        sqlx::query(sqlx::AssertSqlSafe(ran(waited))).execute(&mut *c).await?;
        let queued = sources::schedule_due(&mut c).await?;
        assert_eq!(!queued.is_empty(), due, "{failures} failures, {waited} ago");
        jobs::drain(&f.ctx, "core", 10, None).await?;
    }
    // After a success the weekly interval holds again.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "update source set consecutive_failures = 0 where id = '{}'",
        s.id
    )))
    .execute(&mut *c)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(ran("65 hours")))
        .execute(&mut *c)
        .await?;
    assert!(sources::schedule_due(&mut c).await?.is_empty());
    Ok(())
}

/// A default-sources hook that fails (a database error, say) is logged and skipped: the ClinVar hook
/// still turns the check on, and the pass still queues the polls.
#[tokio::test]
async fn a_failing_default_sources_hook_stops_no_other_hook_or_poll() -> R {
    sources::register_default_sources("zz_failing_test_hook", |_| {
        Box::pin(async { Err(ownhealth_core::Error::Invalid("this hook always fails".into())) })
    });
    let f = fx(false).await?;
    f.alex_genome().await?;
    let queued = sources::scheduler_tick(&f.ctx).await?;
    assert_eq!(queued.len(), 1, "{queued:?}");
    Ok(())
}

#[tokio::test]
async fn a_scheduler_pass_turns_the_check_on_and_queues_it() -> R {
    let f = fx(false).await?;
    f.alex_genome().await?;
    let queued = sources::scheduler_tick(&f.ctx).await?;
    let mut c = f.e.conn().await?;
    let cv: Vec<Source> = sources::list_sources(&mut c, f.e.fam.household_id)
        .await?
        .into_iter()
        .filter(|s| s.kind == "clinvar")
        .collect();
    assert_eq!(cv.len(), 1);
    assert!(cv[0].enabled);
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(
        f.scalar(&format!(
            "select count(*) from job where kind = 'source.poll' and state = 'queued' and payload ->> 'source_id' = '{}'",
            cv[0].id
        ))
        .await?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_local_file_compressed_with_plain_gzip_is_read() -> R {
    let f = fx(false).await?;
    f.alex_genome().await?;
    let local: &Path = &f.e.dir.path().join("clinvar_20261005.vcf.gz");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(RELEASE.as_bytes())?;
    std::fs::write(local, gz.finish()?)?;
    let s = f.source(json!({"path": local.display().to_string()}), true).await?;
    let run = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
    assert_eq!((run.status.as_str(), run.items_new), ("ok", 1), "{run:?}");
    assert_eq!(f.scalar("select count(*) from genome_event where alerted").await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_household_without_a_genome_downloads_nothing() -> R {
    let f = fx(false).await?;
    let (url, hits) = serve(200, bgzipped(RELEASE)?).await?;
    let s = f.source(json!({"url": url, "allow_private": true}), true).await?;
    let run = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
    assert_eq!(run.status, "ok");
    assert_eq!(run.skipped, json!({"no_genome": 1}));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn a_box_without_internet_reads_a_local_file_and_keeps_it() -> R {
    let f = fx(false).await?;
    f.alex_genome().await?;
    let local: &Path = &f.e.dir.path().join("clinvar_20261005.vcf");
    std::fs::write(local, RELEASE)?;
    let s = f.source(json!({"path": local.display().to_string()}), true).await?;
    let run = f.poll(s.id).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
    assert_eq!((run.status.as_str(), run.items_new), ("ok", 1), "{run:?}");
    assert_eq!(f.scalar("select count(*) from genome_event where alerted").await?, 1);
    assert!(local.exists(), "the person's own file is never deleted");
    Ok(())
}

#[tokio::test]
async fn the_check_is_on_by_default_once_a_household_has_a_genome_but_never_in_the_demo() -> R {
    let f = fx(false).await?;
    assert_eq!(sources::add_default_sources(&f.ctx).await?, 0, "no genome, no source");
    f.alex_genome().await?;
    assert_eq!(sources::add_default_sources(&f.ctx).await?, 1);
    assert_eq!(sources::add_default_sources(&f.ctx).await?, 0, "never a second one");
    let mut c = f.e.conn().await?;
    let s = sources::list_sources(&mut c, f.e.fam.household_id).await?;
    let cv: Vec<&Source> = s.iter().filter(|s| s.kind == "clinvar").collect();
    assert_eq!(cv.len(), 1);
    assert!(cv[0].enabled);
    assert_eq!(cv[0].poll_seconds, Some(7 * 24 * 3600));
    assert_eq!(cv[0].profile_id, None);
    assert_eq!(cv[0].config, json!({}));

    let demo = fx(true).await?;
    demo.alex_genome().await?;
    assert_eq!(sources::add_default_sources(&demo.ctx).await?, 0);
    Ok(())
}
