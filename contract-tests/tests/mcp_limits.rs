//! Per-token limits and the audit of every call (mcp/limits.py, mcp/tools/__init__.py): the rate
//! bucket, the daily egress budget, the lookup and proposal caps, and the audit rows with no
//! payloads. Limits have no route in 0.1.1, so the tests set them in the database (`psql`).
//! Python: tests/mcp/test_mcp_limits.py, test_mcp_tools.py::test_lookup_daily_cap.

use anyhow::Context;
use ownhealth_contract_tests::{ALL_SCOPES, Client, Mcp, psql, uniq};
use serde_json::{Value, json};

fn set_limit(plain: &str, column: &str, value: i64) -> anyhow::Result<()> {
    psql(&format!(
        "update api_token set {column} = {value} where prefix = '{}'",
        &plain[..15]
    ))?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn the_rate_bucket_is_per_token() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, slow) = c.token(json!({"scopes": ["read:summary"]})).await?;
    let (_, other) = c.token(json!({"scopes": ["read:summary"]})).await?;
    set_limit(&slow, "rate_per_min", 2)?; // burst min(20, 2)
    let m = Mcp::new(&slow)?;
    for _ in 0..2 {
        m.ok("get_health_status", json!({"profile": "alex"}))
            .await?;
    }
    let r = m
        .call("get_health_status", json!({"profile": "alex"}))
        .await?;
    assert_eq!(r.code(), Some("rate_limited"), "{}", r.sc);
    assert!(r.sc["retry_after_s"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(
        r.sc["fix"]
            .as_str()
            .is_some_and(|f| f.starts_with("Retry in ") && f.ends_with(" s."))
    );
    Mcp::new(&other)?
        .ok("get_health_status", json!({"profile": "alex"}))
        .await?;
    // the default bucket: a burst of 20, then refused
    let (_, fast) = c.token(json!({"scopes": ["read:summary"]})).await?;
    let m = Mcp::new(&fast)?;
    let mut codes = Vec::new();
    for _ in 0..21 {
        codes.push(
            m.call("no_such_tool", json!({}))
                .await?
                .code()
                .map(str::to_string),
        );
    }
    assert!(
        codes[..20]
            .iter()
            .all(|c| c.as_deref() == Some("bad_input")),
        "{codes:?}"
    );
    assert_eq!(codes[20].as_deref(), Some("rate_limited"));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn the_daily_egress_budget_refuses_and_delivers_nothing() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (t, plain) = c.token(json!({"scopes": ALL_SCOPES})).await?;
    let m = Mcp::new(&plain)?;
    let args = json!({"profile": "alex", "loinc": "13457-7", "detail": "full"});
    let first = m.ok("search_observations", args.clone()).await?;
    let n = first["data"]["results"]
        .as_array()
        .map(Vec::len)
        .context("results")? as i64;
    assert!(n > 0);
    set_limit(&plain, "egress_facts_per_day", n + 1)?;
    let r = m.call("search_observations", args).await?;
    assert_eq!(r.code(), Some("egress_budget_exhausted"), "{}", r.sc);
    assert_eq!(r.sc["facts_today"], json!(n));
    assert!(r.sc["bytes_today"].as_i64().unwrap_or(0) > 0);
    let u = c
        .get(&format!(
            "/api/tokens/{}/usage",
            t["id"].as_str().context("id")?
        ))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        (u["today"]["calls"].clone(), u["today"]["facts"].clone()),
        (json!(2), json!(n)),
        "the refused call delivered nothing"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn the_lookup_cap() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, plain) = c.token(json!({"scopes": ALL_SCOPES})).await?;
    set_limit(&plain, "genome_raw_lookups_per_day", 1)?;
    let m = Mcp::new(&plain)?;
    let env = m
        .ok(
            "lookup_variant",
            json!({"profile": "alex", "rsid": "rs4244285"}),
        )
        .await?;
    assert_eq!(
        env["data"]["quota"],
        json!({"used_today": 1, "daily_cap": 1})
    );
    let r = m
        .call(
            "lookup_variant",
            json!({"profile": "alex", "rsid": "rs4244285"}),
        )
        .await?;
    assert_eq!(r.code(), Some("genome_lookup_cap"), "{}", r.sc);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn the_proposal_cap_and_duplicates() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let (_, plain) = c
        .token(json!({"scopes": ALL_SCOPES, "profiles": [p]}))
        .await?;
    set_limit(&plain, "proposals_per_day", 1)?;
    let m = Mcp::new(&plain)?;
    let first = m
        .ok(
            "propose",
            json!({"profile": p, "kind": "note", "payload": {"text": "first"}}),
        )
        .await?;
    assert_eq!(first["data"]["status"], "queued_for_review");
    let again = m
        .ok(
            "propose",
            json!({"profile": p, "kind": "note", "payload": {"text": "first"}}),
        )
        .await?;
    assert_eq!(again["data"]["duplicate_of_open_proposal"], true);
    assert_eq!(again["data"]["proposal_id"], first["data"]["proposal_id"]);
    let r = m
        .call(
            "propose",
            json!({"profile": p, "kind": "note", "payload": {"text": "second"}}),
        )
        .await?;
    assert_eq!(r.code(), Some("propose_cap"));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn every_call_is_audited_without_payloads() -> anyhow::Result<()> {
    let c = Client::new()?;
    let label = format!("Claude Code · {}", uniq());
    let (t, plain) = c
        .token(json!({"label": label, "scopes": ALL_SCOPES}))
        .await?;
    let m = Mcp::new(&plain)?;
    let secret = "colesterol de mi padre";
    let first = m
        .ok(
            "search_observations",
            json!({"profile": "alex", "query": "Colesterol LDL"}),
        )
        .await?;
    m.call(
        "search_observations",
        json!({"profile": "alex", "query": secret}),
    )
    .await?;
    m.call("get_health_status", json!({"profile": "carmen"}))
        .await?;
    m.ok("get_trend", json!({"profile": "alex", "code": "13457-7"}))
        .await?;
    let audit = c.audit(500).await?;
    let mut rows: Vec<&Value> = audit
        .iter()
        .filter(|e| e["actor"] == json!(format!("{label} (token)")))
        .collect();
    rows.reverse();
    let tools: Vec<&str> = rows.iter().filter_map(|e| e["tool"].as_str()).collect();
    assert_eq!(
        tools,
        [
            "search_observations",
            "search_observations",
            "get_health_status",
            "get_trend"
        ]
    );
    for r in &rows {
        assert_eq!(
            (r["via"].clone(), r["actor_kind"].clone()),
            (json!("mcp"), json!("token"))
        );
        assert_eq!(
            r["action"],
            json!(format!(
                "tools/call:{}",
                r["tool"].as_str().unwrap_or_default()
            ))
        );
    }
    let kinds: Vec<&str> = rows
        .iter()
        .filter_map(|e| e["result_kind"].as_str())
        .collect();
    assert_eq!(
        kinds,
        ["ok", "ok", "denied", "ok"],
        "carmen is not on the token: denied"
    );
    assert_eq!(rows[0]["profile"], "alex");
    assert_eq!(
        rows[0]["src_ids"],
        json!(
            ownhealth_contract_tests::src_ids(&first["data"])
                .into_iter()
                .take(1)
                .collect::<Vec<_>>()
        )
    );
    assert!(
        rows[0]["rows_out"].as_i64().unwrap_or(0) >= 1
            && rows[0]["bytes_out"].as_i64().unwrap_or(0) > 0
    );
    assert_eq!(
        (rows[2]["rows_out"].clone(), rows[2]["bytes_out"].clone()),
        (json!(0), json!(0))
    );
    assert!(
        rows[3]["args_summary"]
            .as_str()
            .is_some_and(|a| a.contains("code=13457-7")),
        "{}",
        rows[3]
    );
    let summary1 = rows[1]["args_summary"].as_str().context("args")?;
    let re = regex::Regex::new(r"^profile=alex query=h:[0-9a-f]{12}$")?;
    assert!(re.is_match(summary1), "{summary1}");
    let everything = serde_json::to_string(&rows)?;
    assert!(
        !everything.contains(secret) && !everything.contains("Colesterol"),
        "a payload reached the audit log"
    );
    // the token's own row names it
    assert!(
        audit
            .iter()
            .any(|e| e["action"] == "token_create" && e["resource_ids"] == json!([t["id"]]))
    );
    Ok(())
}
