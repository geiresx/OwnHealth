//! The real-client contract: the official Python MCP SDK client (mcp 2.3, the version 0.1.1 shipped,
//! pinned in `tests/py/mcp_client.py.lock`) talks to the Rust server over a real socket in each mode
//! (2026-07-28, legacy 2025-11-25, auto). It runs through `uv run --script`; skipped when `uv` is not
//! on PATH, except in CI.

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;

use common::*;
use serde_json::Value;

#[tokio::test]
async fn python_sdk_client_in_every_mode() -> R {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let has_uv = std::process::Command::new("uv")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !has_uv {
        anyhow::ensure!(
            std::env::var_os("CI").is_none(),
            "uv absent in CI: the Python client contract cannot run"
        );
        eprintln!("uv absent: skipping the Python client contract");
        return Ok(());
    }
    let w = world().await?;
    let tok = w.tok().await?;
    w.set_limit(&tok, "rate_per_min", 1000).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = w.app().into_make_service_with_connect_info::<SocketAddr>();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let out = tokio::process::Command::new("uv")
        .args(["run", "--quiet", "--locked", "--script"])
        .arg(root.join("tests/py/mcp_client.py"))
        .arg(format!("http://{addr}"))
        .arg(&tok)
        .output()
        .await?;
    server.abort();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(lines.len(), 3);
    let mut reg: Vec<String> = ownhealth_mcp::tools::REGISTRY
        .iter()
        .map(|s| s.name.to_string())
        .collect();
    reg.sort();
    for (line, expected) in lines.iter().zip(["2026-07-28", "2025-11-25", "2026-07-28"]) {
        let mode = &line["mode"];
        assert_eq!(line["protocol_version"], expected, "{mode}");
        assert_eq!(strs(&line["tools"]), reg, "{mode}");
        assert_eq!(line["annotations"]["propose"], false);
        assert_eq!(line["annotations"]["get_source"], true);
        assert_eq!(line["is_error"], false, "{mode}: {}", line["structured"]);
        assert_envelope(&line["structured"]);
        assert_eq!(line["structured"]["profile"], "alex");
        assert!(
            line["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with("OwnHealth get_health_status")
        );
        assert_eq!(line["bad"]["is_error"], true);
        assert_eq!(line["bad"]["code"], "profile_not_granted");
        assert!(line["profiles"]["profiles"].as_array().is_some_and(|p| !p.is_empty()));
        assert_eq!(strs(&line["prompts"]).len(), 3);
    }
    // the audit trail recorded each negotiated version (client address included: a real socket)
    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "select protocol_version, host(ip) from audit_event where tool = 'get_health_status' \
         and result_kind = 'ok' order by id",
    )
    .fetch_all(&w.db.pool)
    .await?;
    let pv: Vec<Option<&str>> = rows.iter().map(|r| r.0.as_deref()).collect();
    assert_eq!(pv, [Some("2026-07-28"), Some("2025-11-25"), Some("2026-07-28")]);
    assert!(rows.iter().all(|r| r.1.as_deref() == Some("127.0.0.1")));
    Ok(())
}
