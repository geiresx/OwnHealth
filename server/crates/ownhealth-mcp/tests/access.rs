//! `tests/mcp/test_mcp_access.py`: effective = token.scopes ∩ grant.categories − profile.locks, plus
//! the genome, sensitive and minor gates, on every tool. The fakes ignore the gates on purpose.

mod common;

use common::*;
use ownhealth_core::access::set_locks;
use serde_json::json;

fn no_genome() -> ownhealth_core::access::Scopes {
    without(&["read:genome", "read:genome_raw"])
}

#[tokio::test]
async fn ungranted_profile_lists_the_allowed_slugs() -> R {
    let w = world().await?;
    let c = w
        .connect(
            &w.token(Tok {
                profiles: vec!["alex", "lucia"],
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let e = c
        .call_tool("get_health_status", json!({"profile": "carmen"}))
        .await?
        .err();
    assert_eq!(e["code"], "profile_not_granted");
    assert_eq!(e["fix"], "Use one of: alex, lucia.");
    assert_eq!(e["allowed"], json!(["alex", "lucia"]));
    let e = c.call_tool("get_health_status", json!({"profile": "sam"})).await?.err();
    assert_eq!(e["code"], "profile_not_granted");
    assert_eq!(
        c.call_tool("get_health_status", json!({})).await?.ok()["profile"],
        "alex"
    );
    Ok(())
}

#[tokio::test]
async fn locks_bind_everyone_but_the_owner() -> R {
    let w = world().await?;
    set_locks(&mut *w.conn().await?, w.pid("alex"), &scopes(&["read:labs"])).await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call_tool("search_observations", json!({"profile": "alex", "query": "ldl"}))
        .await?
        .ok();
    assert!(truthy(&env["data"]["results"]));
    set_locks(&mut *w.conn().await?, w.pid("lucia"), &scopes(&["read:genome"])).await?;
    let c = w
        .connect(
            &w.token(Tok {
                profiles: vec!["lucia"],
                user: "sam",
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    assert!(!c.tool_names().await?.contains("get_genome_findings"));
    let e = c
        .call_tool("get_genome_findings", json!({"profile": "lucia"}))
        .await?
        .err();
    assert_eq!(e["code"], "scope_missing");
    Ok(())
}

#[tokio::test]
async fn without_read_genome_no_genome_fact_appears_anywhere() -> R {
    let w = world().await?;
    let genome: std::collections::BTreeSet<String> = ["slco1b1", "apoe", "hfe"].iter().map(|k| w.s(k)).collect();
    let c = w.connect(&w.tok_scopes(no_genome()).await?).await?;
    let names = c.tool_names().await?;
    assert!(!names.contains("get_genome_findings") && !names.contains("lookup_variant"));
    let status = c.call_tool("get_health_status", json!({"profile": "alex"})).await?.ok();
    assert!(all_srcs(&status).is_disjoint(&genome));
    let flags = status["data"]["flags"].as_array().cloned().unwrap_or_default();
    assert!(!flags.iter().any(|f| f["kind"] == "gene_drug"));
    assert!(strs(&status["withheld"]["by_scope"]).contains(&"genome".to_string()));
    assert!(status["data"]["data_freshness"].get("genome").is_none());
    let drug = c
        .call_tool("check_drug", json!({"profile": "alex", "drug": "atorvastatin"}))
        .await?
        .ok();
    assert!(drug["data"].get("pgx").is_none() && drug["data"].get("guidance").is_none());
    assert!(strs(&drug["withheld"]["by_scope"]).contains(&"genome".to_string()));
    assert!(all_srcs(&drug).is_disjoint(&genome) && !dumps(&drug).contains("SLCO1B1"));
    let e = c
        .call_tool("get_source", json!({"src_id": w.s("slco1b1")}))
        .await?
        .err();
    assert_eq!(e["code"], "unknown");
    let e2 = c
        .call_tool("get_source", json!({"src_id": "src_neverexisted"}))
        .await?
        .err();
    assert_eq!((&e["code"], &e["message"]), (&e2["code"], &e2["message"]));
    let c = w.connect(&w.tok().await?).await?;
    let status = c.call_tool("get_health_status", json!({"profile": "alex"})).await?.ok();
    let flags = status["data"]["flags"].as_array().cloned().unwrap_or_default();
    assert!(flags.iter().any(|f| f["kind"] == "gene_drug"));
    Ok(())
}

#[tokio::test]
async fn sensitive_genome_needs_switch_and_both_scopes() -> R {
    let w = world().await?;
    let apoe = w.s("apoe");
    let full = w.tok().await?;
    let no_sensitive = w.tok_scopes(without(&["read:sensitive"])).await?;
    let env = w
        .connect(&full)
        .await?
        .call_tool("get_genome_findings", json!({"profile": "alex"}))
        .await?
        .ok();
    assert!(!all_srcs(&env).contains(&apoe) && !dumps(&env).contains("APOE"));
    assert_eq!(env["withheld"]["sensitive"], 1);
    w.set_switch("alex", true).await?;
    let env = w
        .connect(&no_sensitive)
        .await?
        .call_tool("get_genome_findings", json!({"profile": "alex"}))
        .await?
        .ok();
    assert!(!all_srcs(&env).contains(&apoe));
    assert_eq!(env["withheld"]["sensitive"], 1);
    let env = w
        .connect(&full)
        .await?
        .call_tool("get_genome_findings", json!({"profile": "alex"}))
        .await?
        .ok();
    assert!(all_srcs(&env).contains(&apoe));
    assert_eq!(env["withheld"]["sensitive"], 0);
    Ok(())
}

#[tokio::test]
async fn minor_rule_withholds_carrier_but_shows_pgx() -> R {
    let w = world().await?;
    w.set_switch("lucia", true).await?;
    let c = w
        .connect(
            &w.token(Tok {
                profiles: vec!["lucia"],
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let env = c
        .call_tool("get_genome_findings", json!({"profile": "lucia"}))
        .await?
        .ok();
    let srcs = all_srcs(&env);
    assert!(srcs.contains(&w.s("lucia_pgx")) && !srcs.contains(&w.s("lucia_carrier")));
    assert_eq!(env["withheld"]["minor_rule"], 1);
    assert!(!dumps(&env).contains("CFTR"));
    Ok(())
}

#[tokio::test]
async fn sensitive_observations_need_read_sensitive() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok_scopes(without(&["read:sensitive"])).await?).await?;
    let env = c
        .call_tool(
            "search_observations",
            json!({"profile": "alex", "query": "PHQ-9", "detail": "full"}),
        )
        .await?
        .ok();
    assert_eq!(env["data"]["results"], json!([]));
    assert_eq!(env["withheld"]["sensitive"], 1);
    let e = c.call_tool("get_source", json!({"src_id": w.s("phq9")})).await?.err();
    assert_eq!(e["code"], "unknown");
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call_tool("search_observations", json!({"profile": "alex", "query": "PHQ-9"}))
        .await?
        .ok();
    let got: Vec<_> = env["data"]["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| r["src"].clone())
        .collect();
    assert_eq!(got, vec![json!(w.s("phq9"))]);
    Ok(())
}

#[tokio::test]
async fn category_scopes_split_one_search() -> R {
    let w = world().await?;
    let c = w
        .connect(&w.tok_scopes(scopes(&["read:summary", "read:vitals"])).await?)
        .await?;
    let env = c
        .call_tool("search_observations", json!({"profile": "alex", "query": "LDL"}))
        .await?
        .ok();
    assert_eq!(env["data"]["results"], json!([]));
    assert_eq!(env["withheld"]["by_scope"], json!(["labs"]));
    let env = c
        .call_tool("search_observations", json!({"profile": "alex", "query": "Peso"}))
        .await?
        .ok();
    let got: Vec<_> = env["data"]["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| r["src"].clone())
        .collect();
    assert_eq!(got, vec![json!(w.s("weight"))]);
    let e = c.call_tool("get_source", json!({"src_id": w.s("vitd")})).await?.err();
    assert_eq!(e["code"], "scope_missing");
    Ok(())
}

#[tokio::test]
async fn another_profiles_src_is_unknown() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let e = c
        .call_tool("get_source", json!({"src_id": w.s("sam_ldl")}))
        .await?
        .err();
    assert_eq!(e["code"], "unknown");
    Ok(())
}
