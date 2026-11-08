//! `ownhealth serve | migrate | demo-seed | worker | healthcheck | backup | restore | verify-restore`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ownhealth::app;
use ownhealth_core::config::Settings;
use ownhealth_core::{Runtime, demo, jobs, migrate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(name = "ownhealth", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the web app, MCP server and job loop.
    Serve,
    /// Apply pending database migrations.
    Migrate,
    /// Load the synthetic Rivera family.
    DemoSeed {
        /// Seed even if the demo household exists.
        #[arg(long)]
        force: bool,
    },
    /// Run only the job loop (OWNHEALTH_QUEUES).
    Worker,
    /// Exit 0 when /healthz answers.
    Healthcheck,
    /// Write a consistent backup directory (database dump, blobs, manifest) for restic.
    Backup { path: PathBuf },
    /// Load a backup directory into this empty instance and check every state_hash.
    Restore { path: PathBuf },
    /// Restore a backup into a scratch database, compare with its manifest, drop it.
    VerifyRestore { path: PathBuf },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Logs go to stderr, as Python's logging did: stdout carries the reports `verify-restore` and
    // `restore` print, which deploy/restic/restore-test.sh and cron logs read as JSON.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        // colour only on a terminal: container logs and cron mail get plain text
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();
    match run(cli.cmd).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn report(r: &serde_json::Value) -> anyhow::Result<ExitCode> {
    println!("{}", serde_json::to_string_pretty(r)?);
    Ok(if r["ok"] == serde_json::Value::Bool(true) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn run(cmd: Cmd) -> anyhow::Result<ExitCode> {
    ownhealth::register_lanes();
    let settings = Settings::from_env()?;
    match cmd {
        Cmd::Serve => {
            app::serve(settings).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Backup { path } => {
            println!("{}", ownhealth_ops::backup(&settings, &path).await?.display());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Restore { path } => report(&ownhealth_ops::restore(&settings, &path).await?),
        Cmd::VerifyRestore { path } => report(&ownhealth_ops::verify_restore(&settings, &path).await?),
        Cmd::Healthcheck => Ok(if healthcheck(settings.port).await {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }),
        Cmd::Migrate => {
            let rt = Runtime::new(settings)?;
            let applied = migrate::migrate_embedded(&rt.pool).await?;
            if applied.is_empty() {
                println!("applied: nothing");
            } else {
                println!("applied: {applied:?}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::DemoSeed { force } => {
            let rt = Runtime::new(settings)?;
            migrate::migrate_embedded(&rt.pool).await?;
            if demo::seed_demo(&rt.pool, &rt.settings, &rt.blobs, force).await? {
                println!("demo household seeded");
            } else {
                println!("demo household exists; use --force to add another");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Worker => {
            let rt = Runtime::new(settings)?;
            app::sweep_spools(&rt);
            let (stop, rx) = tokio::sync::watch::channel(false);
            let workers = jobs::start_workers(&rt.job_context(), rx);
            app::shutdown_signal().await;
            let _ = stop.send(true);
            for w in workers {
                let _ = w.await;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// A plain HTTP/1.0 GET of /healthz on loopback, 3 s budget, like Python's urlopen check.
async fn healthcheck(port: u16) -> bool {
    let probe = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").await?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await?;
        Ok::<bool, std::io::Error>(buf.starts_with(b"HTTP/1.0 200") || buf.starts_with(b"HTTP/1.1 200"))
    };
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), probe).await,
        Ok(Ok(true))
    )
}
