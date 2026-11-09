//! v0.2 Ops: the `ocr` compose profile and the image variant behind it, checked from outside the app.
//!
//! - Without the profile neither compose file starts an OCR worker; with `COMPOSE_PROFILES=ocr` both
//!   start one from `drumsergio/ownhealth:<app tag>-ocr`, as `ownhealth worker` on the `ocr` queue only,
//!   connected as the non-owner role, sharing the app's data volume. Rendered by `docker compose
//!   config`; skipped without docker compose.
//! - The homelab app seals inbox secrets with a key from a secret file, and the OCR worker gets no
//!   backup credentials.
//! - The Dockerfile's default target has no OCR engine and no models; `--target ocr` has both.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ownhealth_core::config::Settings;
use serde_json::Value;

type R = anyhow::Result<()>;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn has_compose() -> bool {
    Command::new("docker")
        .args(["compose", "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `docker compose config --format json` services of `compose`, with `env_file` + `extra` as its .env.
fn render(compose: &Path, env_file: &Path, extra: &str) -> anyhow::Result<Value> {
    let dir = tempfile::tempdir()?;
    std::fs::copy(compose, dir.path().join("compose.yaml"))?;
    std::fs::write(dir.path().join(".env"), std::fs::read_to_string(env_file)? + extra)?;
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

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

fn volume_sources(v: &Value) -> Vec<String> {
    v["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            format!(
                "{}:{}",
                m["source"].as_str().unwrap_or_default(),
                m["target"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

#[test]
fn the_ocr_profile_adds_one_worker_on_the_ocr_queue() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    for (case, compose, env_file, extra) in cases() {
        let off = render(&compose, &env_file, extra)?;
        assert!(
            off.get("ocr").is_none(),
            "{case}: the worker starts without the profile"
        );
        assert!(off.get("app").is_some(), "{case}");

        let on = render(
            &compose,
            &env_file,
            &format!("{extra}COMPOSE_PROFILES=ocr\nOWNHEALTH_OCR=1\n"),
        )?;
        let (app, ocr) = (&on["app"], &on["ocr"]);
        let app_image = app["image"].as_str().unwrap_or_default();
        assert!(app_image.starts_with("drumsergio/ownhealth:"), "{case}: {app_image}");
        assert_eq!(
            ocr["image"],
            format!("{app_image}-ocr"),
            "{case}: the worker runs the same version"
        );
        assert_eq!(strings(&ocr["command"]), ["ownhealth", "worker"], "{case}");
        assert_eq!(env_str(ocr, "OWNHEALTH_QUEUES"), "ocr", "{case}");
        assert_eq!(env_str(ocr, "OWNHEALTH_RUN_MIGRATIONS"), "0", "{case}");
        assert!(
            env_str(ocr, "OWNHEALTH_DATABASE_URL").starts_with("postgresql://ownhealth_app@"),
            "{case}: the worker must not hold the owner role"
        );
        assert_eq!(
            ocr["healthcheck"]["disable"], true,
            "{case}: a worker serves no /healthz"
        );
        assert_eq!(
            ocr["depends_on"]["admin"]["condition"], "service_completed_successfully",
            "{case}"
        );
        assert!(
            volume_sources(ocr).iter().any(|v| v.ends_with("ownhealth-data:/data")),
            "{case}: the worker reads the blobs the app stored: {:?}",
            volume_sources(ocr)
        );
        // the app reads OWNHEALTH_OCR from .env and keeps its own queue
        assert_eq!(env_str(app, "OWNHEALTH_OCR"), "1", "{case}");
        assert!(env_str(app, "OWNHEALTH_QUEUES").is_empty(), "{case}");

        // what the worker's environment parses to
        let mut env: HashMap<String, String> = HashMap::new();
        for (k, v) in ocr["environment"].as_object().into_iter().flatten() {
            if k.starts_with("OWNHEALTH_") && !k.ends_with("_FILE") {
                env.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
            }
        }
        let settings = Settings::from(&env)?;
        assert_eq!(settings.queues, vec!["ocr".to_string()], "{case}");
        assert!(!settings.run_migrations, "{case}");

        if case == "quickstart" {
            assert_eq!(app["build"]["target"], "app");
            assert_eq!(on["admin"]["build"]["target"], "app");
            assert_eq!(ocr["build"]["target"], "ocr");
        }
        if case == "homelab" {
            assert_eq!(ocr["read_only"], true);
            assert_eq!(strings(&ocr["cap_drop"]), ["ALL"]);
            let leaked: Vec<&String> = ocr["environment"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k)
                .filter(|k| k.starts_with("RESTIC_") || k.starts_with("AWS_"))
                .collect();
            assert!(leaked.is_empty(), "{leaked:?}");
        }
    }
    Ok(())
}

#[test]
fn homelab_app_seals_inbox_secrets_with_a_key_file_and_mounts_the_watched_folder() -> R {
    if !has_compose() {
        eprintln!("docker compose not available: skipped");
        return Ok(());
    }
    let [(_, compose, env_file, extra), (_, quick, quick_env, quick_extra)] = cases();
    let services = render(&compose, &env_file, extra)?;
    let app = &services["app"];
    assert_eq!(env_str(app, "OWNHEALTH_SECRETS_KEY_FILE"), "/run/secrets/secrets_key");
    let secrets: Vec<String> = app["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["source"].as_str().map(str::to_string))
        .collect();
    assert!(secrets.contains(&"secrets_key".to_string()), "{secrets:?}");
    for (case, services) in [
        ("homelab", services.clone()),
        ("quickstart", render(&quick, &quick_env, quick_extra)?),
    ] {
        let app = &services["app"];
        assert_eq!(env_str(app, "OWNHEALTH_INBOX_DIR"), "/inbox", "{case}");
        assert!(
            volume_sources(app).iter().any(|v| v.ends_with("/inbox:/inbox")),
            "{case}: {:?}",
            volume_sources(app)
        );
    }
    // `compose config` merges a repeated bind, but the engine refuses the container ("Duplicate
    // mount point"), so each file mounts the folder once
    for file in [&compose, &quick] {
        let text = std::fs::read_to_string(file)?;
        assert_eq!(text.matches(":/inbox\n").count(), 1, "{}", file.display());
    }
    Ok(())
}

/// The stages of the Dockerfile: `(name, instructions without comments)` in order.
fn stages(dockerfile: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in dockerfile.lines().filter(|l| !l.trim_start().starts_with('#')) {
        if let Some(from) = line.strip_prefix("FROM ") {
            let name = from.split(" AS ").nth(1).unwrap_or_default().trim().to_string();
            out.push((name, String::new()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

#[test]
fn the_default_image_has_no_ocr_engine_and_the_ocr_target_has_engine_and_models() -> R {
    let text = std::fs::read_to_string(root().join("Dockerfile"))?;
    let stages = stages(&text);
    let body = |name: &str| {
        stages
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.clone())
            .unwrap_or_default()
    };
    // `docker build .` builds the last stage
    let (last, default) = stages.last().cloned().unwrap_or_default();
    assert_eq!(last, "app");
    assert!(default.contains("COPY --from=build /out/ownhealth"), "{default}");
    assert!(!default.contains("ocr"), "the default image carries OCR: {default}");
    assert!(body("build").contains("cargo build --release --locked -p ownhealth \\"));
    assert!(!body("build").contains("--features"));
    assert!(!body("runtime").contains("ocr"), "the shared runtime carries OCR");

    assert!(body("build-ocr").contains("cargo build --release --locked -p ownhealth --features ocr"));
    assert!(body("ocr-models").contains("fetch-ocr-models.sh /opt/ownhealth/ocr-models"));
    let ocr = body("ocr");
    assert!(
        ocr.contains("COPY --from=build-ocr /out/ownhealth /usr/local/bin/ownhealth"),
        "{ocr}"
    );
    assert!(ocr.contains("COPY --from=ocr-models /opt/ownhealth/ocr-models /opt/ownhealth/ocr-models"));
    assert_eq!(
        Settings::default().ocr_models_dir,
        PathBuf::from("/opt/ownhealth/ocr-models")
    );
    Ok(())
}
