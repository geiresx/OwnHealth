//! `tests/mcp/test_mcp_limits.py`: per-token rate limit, daily egress budget, usage counters and the
//! audit log (no payloads). `test_usage_route` lives in `ownhealth-api/tests/mcp_usage.rs`.

mod common;

use chrono::{Duration, TimeZone, Utc};
use common::*;
use ownhealth_mcp::limits::take_call;
use serde_json::{Value, json};

#[tokio::test]
async fn rate_limit_per_token() -> R {
    let w = world().await?;
    let (slow, other) = (w.tok().await?, w.tok().await?);
    w.set_limit(&slow, "rate_per_min", 2).await?;
    let c = w.connect(&slow).await?;
    for _ in 0..2 {
        c.call_tool("get_health_status", json!({"profile": "alex"})).await?.ok();
    }
    let e = c
        .call_tool("get_health_status", json!({"profile": "alex"}))
        .await?
        .err();
    assert_eq!(e["code"], "rate_limited");
    assert!(e["retry_after_s"].as_f64().unwrap_or(0.0) > 0.0);
    w.connect(&other)
        .await?
        .call_tool("get_health_status", json!({"profile": "alex"}))
        .await?
        .ok();
    Ok(())
}

#[tokio::test]
async fn bucket_refills() -> R {
    let w = world().await?;
    let tid = w.token_id(&w.tok().await?).await?;
    let t0 = Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).single();
    let mut c = w.conn().await?;
    let mut results = Vec::new();
    for _ in 0..21 {
        results.push(take_call(&mut c, tid, 60, t0).await?);
    }
    assert!(results[..20].iter().all(Option::is_none));
    assert!(results[20].is_some());
    let t1 = t0.map(|t| t + Duration::seconds(1));
    assert!(take_call(&mut c, tid, 60, t1).await?.is_none());
    assert!(take_call(&mut c, tid, 60, t1).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn daily_egress_budget() -> R {
    let w = world().await?;
    let tok = w.tok().await?;
    w.set_limit(&tok, "egress_facts_per_day", 10).await?;
    let c = w.connect(&tok).await?;
    let args = json!({"profile": "alex", "loinc": "13457-7", "detail": "full"});
    let first = c.call_tool("search_observations", args.clone()).await?.ok();
    assert_eq!(first["data"]["results"].as_array().map(Vec::len), Some(8));
    let e = c.call_tool("search_observations", args).await?.err();
    assert_eq!(e["code"], "egress_budget_exhausted");
    assert_eq!(e["facts_today"], 8);
    let (calls, facts): (i32, i32) = sqlx::query_as("select calls, facts from mcp_usage where token_id = $1")
        .bind(w.token_id(&tok).await?)
        .fetch_one(&w.db.pool)
        .await?;
    assert_eq!((calls, facts), (2, 8));
    Ok(())
}

#[tokio::test]
async fn every_call_is_audited_without_payloads() -> R {
    let w = world().await?;
    let tok = w.tok().await?;
    let secret_query = "colesterol de mi padre";
    let c = w.connect(&tok).await?;
    c.call_tool(
        "search_observations",
        json!({"profile": "alex", "query": "Colesterol LDL"}),
    )
    .await?
    .ok();
    c.call_tool("search_observations", json!({"profile": "alex", "query": secret_query}))
        .await?;
    c.call_tool("get_health_status", json!({"profile": "carmen"})).await?;
    c.call_tool("get_trend", json!({"profile": "alex", "code": "13457-7"}))
        .await?;
    let rows: Vec<(Value,)> = sqlx::query_as("select to_jsonb(a) from audit_event a where token_id = $1 order by id")
        .bind(w.token_id(&tok).await?)
        .fetch_all(&w.db.pool)
        .await?;
    let rows: Vec<Value> = rows.into_iter().map(|r| r.0).collect();
    let tools: Vec<&str> = rows.iter().map(|r| r["tool"].as_str().unwrap_or_default()).collect();
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
        assert_eq!(r["via"], "mcp");
        assert_eq!(r["actor_kind"], "token");
        assert!(
            r["action"].as_str().unwrap_or_default().starts_with("tools/call:"),
            "{r}"
        );
    }
    let kinds: Vec<&str> = rows
        .iter()
        .map(|r| r["result_kind"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kinds, ["ok", "ok", "denied", "ok"]);
    assert_eq!(rows[0]["src_ids"], json!([w.s("ldl_latest")]));
    assert_eq!(rows[0]["rows_out"], 1);
    assert!(rows[0]["bytes_out"].as_i64().unwrap_or(0) > 0);
    assert_eq!(rows[0]["profile_id"], json!(w.pid("alex")));
    assert_eq!(rows[0]["client"], "Claude Code · test");
    assert!(
        rows[3]["args_summary"]
            .as_str()
            .unwrap_or_default()
            .contains("code=13457-7")
    );
    let everything = dumps(&json!(rows));
    assert!(!everything.contains(secret_query) && !everything.contains("Colesterol"));
    let summaries = dumps(&json!(
        rows.iter().map(|r| r["args_summary"].clone()).collect::<Vec<_>>()
    ));
    assert!(!summaries.contains("97"));
    assert!(
        rows[1]["args_summary"]
            .as_str()
            .unwrap_or_default()
            .contains("query=h:")
    );
    Ok(())
}

#[tokio::test]
async fn propose_daily_cap() -> R {
    let w = world().await?;
    let tok = w.tok().await?;
    w.set_limit(&tok, "proposals_per_day", 1).await?;
    let c = w.connect(&tok).await?;
    let note = |t: &str| json!({"profile": "alex", "kind": "note", "payload": {"text": t}});
    c.call_tool("propose", note("first")).await?.ok();
    let again = c.call_tool("propose", note("first")).await?.ok();
    assert_eq!(again["data"]["duplicate_of_open_proposal"], true);
    let e = c.call_tool("propose", note("second")).await?.err();
    assert_eq!(e["code"], "propose_cap");
    Ok(())
}
