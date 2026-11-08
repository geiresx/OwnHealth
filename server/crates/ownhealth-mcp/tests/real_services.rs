//! `tests/mcp/test_mcp_real_services.py`: the same contract against lanes D and E as they ship,
//! without the fakes: every tool answers in the envelope with a src on its facts, nothing crashes,
//! and the genome and sensitive gates hold.

mod common;

use std::collections::BTreeSet;

use common::*;
use serde_json::{Value, json};

fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("get_health_status", json!({"profile": "alex"})),
        (
            "search_observations",
            json!({"profile": "alex", "query": "LDL", "detail": "full"}),
        ),
        (
            "get_trend",
            json!({"profile": "alex", "code": "13457-7", "window": "all"}),
        ),
        ("check_drug", json!({"profile": "alex", "drug": "simvastatin"})),
        ("get_genome_findings", json!({"profile": "alex", "detail": "full"})),
        ("get_source", json!({"src_id": "vitd"})),
    ]
}

#[tokio::test]
async fn every_tool_answers_in_the_envelope() -> R {
    let w = world().await?;
    let c = w.connect_real(&w.tok().await?).await?;
    for (name, mut args) in calls() {
        if let Some(k) = args["src_id"].as_str()
            && let Some(s) = w.src.get(k)
        {
            args["src_id"] = json!(s);
        }
        let env = c.call(name, args).await?.ok();
        assert_envelope(&env);
        assert!(facts(&env["data"]).iter().all(|f| truthy(&f["src"])), "{name}");
    }
    let e = c
        .call_tool("lookup_variant", json!({"profile": "alex", "rsid": "rs1"}))
        .await?
        .err();
    assert_ne!(e["code"], "internal_error");
    Ok(())
}

#[tokio::test]
async fn real_services_hold_the_genome_and_sensitive_gates() -> R {
    let w = world().await?;
    let genome: BTreeSet<String> = ["slco1b1", "apoe", "hfe"].iter().map(|k| w.s(k)).collect();
    let c = w
        .connect_real(&w.tok_scopes(without(&["read:genome", "read:genome_raw"])).await?)
        .await?;
    for (name, args) in calls().into_iter().take(4) {
        let env = c.call_tool(name, args).await?.ok();
        assert!(
            all_srcs(&env).is_disjoint(&genome) && !dumps(&env).contains("Decreased function"),
            "{name}"
        );
    }
    w.set_switch("alex", true).await?;
    let c = w
        .connect_real(&w.tok_scopes(without(&["read:sensitive"])).await?)
        .await?;
    let env = c
        .call_tool("get_genome_findings", json!({"profile": "alex", "detail": "full"}))
        .await?
        .ok();
    assert!(!all_srcs(&env).contains(&w.s("apoe")) && !dumps(&env).contains("APOE"));
    let c = w.connect_real(&w.tok().await?).await?;
    let env = c
        .call_tool("get_genome_findings", json!({"profile": "alex", "detail": "full"}))
        .await?
        .ok();
    assert!(all_srcs(&env).contains(&w.s("apoe")));
    Ok(())
}

#[tokio::test]
async fn genome_gate_control_check_drug_shows_the_phenotype_with_read_genome() -> R {
    let w = world().await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let env = c
        .call_tool("check_drug", json!({"profile": "alex", "drug": "simvastatin"}))
        .await?
        .ok();
    assert!(
        dumps(&env).contains("Decreased function") && all_srcs(&env).contains(&w.s("slco1b1")),
        "{env}"
    );
    Ok(())
}

#[tokio::test]
async fn lookup_of_a_withheld_sensitive_site_is_counted_not_crashed() -> R {
    let w = world().await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let env = c
        .call_tool(
            "lookup_variant",
            json!({"profile": "alex", "hgvs": "NC_000019.10:g.44908684T>C"}),
        )
        .await?
        .ok();
    assert_eq!(env["data"]["withheld"], "withheld_sensitive");
    assert_eq!(env["data"]["calls"], json!([]));
    assert!(env["withheld"]["sensitive"].as_i64().unwrap_or(0) >= 1);
    Ok(())
}
