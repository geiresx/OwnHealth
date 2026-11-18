//! The weekly ClinVar check (genomics.md 9), source kind `clinvar`. Once a week, for each household
//! with the switch on, the poller streams NCBI's GRCh38 weekly VCF (about 200 MB) to a temporary
//! file, runs [`clinvar::diff_release_in`] over that household's genomes, and deletes the file. A box
//! without internet sets `config.path` to a local copy (read, never deleted: it is the person's file),
//! or `config.url` to a mirror on its own network. The request is a plain GET of a public file: no
//! personal data, no token, no cookie.
//!
//! On by default for a household with a genome: the scheduler adds the source when the household has
//! a current genome dataset and no ClinVar source yet ([`add_default_sources`]), so a person who
//! switches it off keeps it off. The demo never adds it by itself, so a demo box makes no outbound
//! request nobody asked for. A household without a genome downloads nothing.
//!
//! OwnHealth only compares and shows. A call that ClinVar reclassified is made again by OwnGenome.
//!
//! Source `config`: `{url?: http(s) URL, allow_private?: bool, path?: absolute path to a .vcf or
//! .vcf.gz}`; `path` and `url` are exclusive.

use std::path::{Path, PathBuf};

use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::net::{self, Egress};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::clinvar;

pub const KIND: &str = "clinvar";
/// NCBI's newest weekly release (https://ftp.ncbi.nlm.nih.gov/pub/clinvar/vcf_GRCh38/weekly/).
pub const DEFAULT_URL: &str = "https://ftp.ncbi.nlm.nih.gov/pub/clinvar/vcf_GRCh38/weekly/clinvar.vcf.gz";
/// ClinVar publishes its VCF once a week.
pub const POLL_SECONDS: i32 = 7 * 24 * 3600;
pub const LABEL: &str = "ClinVar";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub allow_private: bool,
    #[serde(default)]
    pub path: Option<String>,
}

fn parse(config: &Value) -> Result<Config, String> {
    serde_json::from_value(config.clone())
        .map_err(|_| "the ClinVar source takes only url, allow_private and path".into())
}

/// Checks a `clinvar` source config before it is saved: an absolute `.vcf` or `.vcf.gz` path, or an
/// http(s) URL, not both.
pub fn check_config(config: &Value) -> Result<(), String> {
    let cfg = parse(config)?;
    let path = cfg.path.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let url = cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    if path.is_some() && url.is_some() {
        return Err("set either config.path or config.url, not both".into());
    }
    if let Some(p) = path {
        let name = p.to_ascii_lowercase();
        if !Path::new(p).is_absolute() || !(name.ends_with(".vcf") || name.ends_with(".vcf.gz")) {
            return Err("config.path must be an absolute path to a .vcf or .vcf.gz file".into());
        }
    }
    if let Some(u) = url {
        // The fetch parses it fully (`net::download`); here only the scheme and a host.
        let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"));
        if rest.is_none_or(|r| r.split(['/', '?', '#']).next().unwrap_or_default().is_empty()) {
            return Err("config.url must be an http(s) URL".into());
        }
    }
    Ok(())
}

/// Deletes the downloaded release however the run ends.
struct Downloaded(PathBuf);

impl Drop for Downloaded {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Current genome datasets of the household whose file stands: what a run would compare.
async fn genomes(ctx: &JobContext, household: Uuid) -> Result<i64, JobError> {
    let mut conn = ctx.pool.acquire().await?;
    Ok(sqlx::query_scalar(
        "select count(*) from genome_dataset d join profile p on p.id = d.profile_id \
         where p.household_id = $1 and d.status = 'current' and not exists \
         (select 1 from document doc where doc.id = d.document_id and doc.status = 'retracted')",
    )
    .bind(household)
    .fetch_one(&mut *conn)
    .await?)
}

/// The `clinvar` poller: one release compared with the household's genomes. A run counts datasets:
/// `new` compared, `duplicate` already compared with this release, a dataset whose calls could not be
/// read is a problem, and the other skips are counted by reason. The cursor keeps the release date
/// and how many events and alerts it wrote.
pub async fn poll_clinvar(ctx: JobContext, source: Source) -> Result<RunOutcome, JobError> {
    let cfg = parse(&source.config).map_err(JobError::permanent)?;
    if genomes(&ctx, source.household_id).await? == 0 {
        return Ok(RunOutcome {
            skipped: [("no_genome".to_string(), 1)].into(),
            ..RunOutcome::default()
        });
    }
    let local = cfg.path.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let (release, _downloaded) = match local {
        Some(p) => (PathBuf::from(p), None),
        None => {
            let url = cfg
                .url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .unwrap_or(DEFAULT_URL);
            // One name per source, so a run cut off by a restart leaves no second copy behind.
            let dest = ctx
                .blobs
                .root()
                .join("tmp")
                .join(format!("clinvar-{}.vcf.gz", source.id));
            let guard = Downloaded(dest.clone());
            match net::download(url, &Egress::reference_file(cfg.allow_private), &dest).await {
                Ok((200, _)) => {}
                Ok((status, _)) => return Err(JobError::retry(format!("clinvar: the server answered {status}"))),
                Err(e) => return Err(JobError::retry(format!("clinvar: {e}"))),
            }
            (dest, Some(guard))
        }
    };
    let report = clinvar::diff_release_in(&ctx.pool, &ctx.blobs, &release, Some(source.household_id)).await?;
    let mut out = RunOutcome::default();
    let (mut events, mut alerts) = (0, 0);
    for d in &report.datasets {
        out.seen += 1;
        events += d.events;
        alerts += d.alerts;
        match d.skipped {
            None => out.new += 1,
            Some("already_compared") => out.duplicate += 1,
            Some("unreadable") => out.problems.push(RunProblem {
                code: "unreadable_calls".into(),
                message: d.error.clone().unwrap_or_default(),
            }),
            Some(other) => *out.skipped.entry(other.to_string()).or_default() += 1,
        }
    }
    out.cursor = Some(json!({"release": report.release.to_string(), "events": events, "alerts": alerts}));
    Ok(out)
}

/// Takes the lock every writer of a new ClinVar source holds until its transaction ends, so two
/// schedulers (two replicas), or the scheduler and the API, never add a household's row twice.
async fn lock_new_sources(conn: &mut PgConnection) -> ownhealth_core::Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtext('ownhealth.clinvar_default_sources'))")
        .execute(conn)
        .await?;
    Ok(())
}

/// Whether the household already has its ClinVar source. Call it inside the transaction that adds
/// one: it takes the same lock as [`add_default_sources`], so the answer holds until that commit. A
/// household has one check: a second one would download the release again and could not be deleted.
pub async fn has_source(conn: &mut PgConnection, household: Uuid) -> ownhealth_core::Result<bool> {
    lock_new_sources(&mut *conn).await?;
    Ok(
        sqlx::query_scalar("select exists (select 1 from source where household_id = $1 and kind = $2)")
            .bind(household)
            .bind(KIND)
            .fetch_one(conn)
            .await?,
    )
}

/// The default: a ClinVar source, on and weekly, for each household with a current genome dataset and
/// no ClinVar source. Never in the demo. Returns the rows added.
pub async fn add_default_sources(ctx: JobContext) -> ownhealth_core::Result<u64> {
    if ctx.settings.demo {
        return Ok(0);
    }
    let mut tx = ctx.pool.begin().await?;
    lock_new_sources(&mut tx).await?;
    let added = sqlx::query(
        "insert into source (household_id, kind, label, poll_seconds) \
         select distinct p.household_id, $1, $2, $3::int from genome_dataset d join profile p on p.id = d.profile_id \
         where d.status = 'current' and not exists \
         (select 1 from source s where s.household_id = p.household_id and s.kind = $1)",
    )
    .bind(KIND)
    .bind(LABEL)
    .bind(POLL_SECONDS)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(added)
}

pub fn register() {
    ownhealth_core::sources::register_poller(KIND, |ctx, source| Box::pin(poll_clinvar(ctx, source)));
    ownhealth_core::sources::register_default_sources(KIND, |ctx| Box::pin(add_default_sources(ctx)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_is_a_local_vcf_or_an_http_url() {
        assert!(check_config(&json!({})).is_ok());
        assert!(check_config(&json!({"path": "/srv/clinvar/clinvar.vcf.gz"})).is_ok());
        assert!(check_config(&json!({"url": "http://mirror.lan/clinvar.vcf.gz", "allow_private": true})).is_ok());
        for bad in [
            json!({"path": "clinvar.vcf.gz"}),
            json!({"path": "/etc/passwd"}),
            json!({"url": "ftp://ftp.ncbi.nlm.nih.gov/x.vcf.gz"}),
            json!({"path": "/a.vcf", "url": "https://x.example/a.vcf"}),
            json!({"token": "not-a-real-token"}),
        ] {
            assert!(check_config(&bad).is_err(), "{bad}");
        }
    }
}
