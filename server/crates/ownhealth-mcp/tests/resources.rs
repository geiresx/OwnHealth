//! `tests/mcp/test_mcp_resources.py`: resources and prompts (mcp.md 6, 7), listed per token, read
//! under the same gates as the tools, every read audited, one refusal answer whatever the reason.

mod common;

use std::collections::BTreeSet;

use common::*;
use serde_json::{Value, json};

async fn audited(w: &World, prefix: &str) -> anyhow::Result<Vec<(String, Option<String>)>> {
    Ok(sqlx::query_as(
        "select e.action, e.result_kind from audit_event e join api_token t on t.id = e.token_id \
         where t.prefix = $1 order by e.id",
    )
    .bind(prefix)
    .fetch_all(&w.db.pool)
    .await?)
}

fn set(v: &[String]) -> BTreeSet<String> {
    v.iter().cloned().collect()
}

async fn json_resource(c: &Client, uri: &str) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&c.read_text(uri).await?)?)
}

#[tokio::test]
async fn resources_follow_the_token() -> R {
    let w = world().await?;
    let c = w
        .connect(
            &w.token(Tok {
                profiles: vec!["alex", "sam"],
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let uris = set(&c.list_resources().await?);
    assert!(uris.contains("ownhealth://profiles") && uris.contains("ownhealth://profile/alex/access-log"));
    assert!(uris.contains("ownhealth://profile/sam/status"));
    assert!(!uris.contains("ownhealth://profile/sam/access-log"));
    let t = c
        .rpc("resources/templates/list", json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let templates: BTreeSet<String> = t["resourceTemplates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x["uriTemplate"].as_str().map(str::to_string))
        .collect();
    assert!(templates.contains("ownhealth://source/{src_id}"));
    let c = w.connect(&w.tok_scopes(scopes(&["read:labs"])).await?).await?;
    let uris = set(&c.list_resources().await?);
    assert!(!uris.contains("ownhealth://profiles") && !uris.iter().any(|u| u.contains("/status")));
    Ok(())
}

#[tokio::test]
async fn profiles_hide_names_when_pseudonymous() -> R {
    let w = world().await?;
    let c = w
        .connect(
            &w.token(Tok {
                profiles: vec!["alex", "sam"],
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let full = json_resource(&c, "ownhealth://profiles").await?;
    let profiles = full["profiles"].as_array().cloned().unwrap_or_default();
    let slugs: BTreeSet<String> = profiles
        .iter()
        .filter_map(|p| p["slug"].as_str().map(str::to_string))
        .collect();
    assert_eq!(slugs, ["alex", "sam"].iter().map(|s| s.to_string()).collect());
    assert!(profiles.iter().all(|p| truthy(&p["display_name"])));
    let c = w
        .connect(
            &w.token(Tok {
                identity_mode: "pseudonymous",
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let pseudo = json_resource(&c, "ownhealth://profiles").await?;
    assert!(!dumps(&pseudo).contains("display_name"));
    Ok(())
}

#[tokio::test]
async fn status_resource_matches_the_tool() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let tool = c.call_tool("get_health_status", json!({"profile": "alex"})).await?.ok();
    let res = c
        .read_resource("ownhealth://profile/alex/status")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(res["contents"][0]["mimeType"], "application/json");
    let env: Value = serde_json::from_str(res["contents"][0]["text"].as_str().unwrap_or_default())?;
    assert_eq!(env["data"]["state_hash"], tool["data"]["state_hash"]);
    let md = c
        .read_resource("ownhealth://profile/alex/status.md")
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(md["contents"][0]["mimeType"], "text/markdown");
    assert!(
        md["contents"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .starts_with("OwnHealth get_health_status")
    );
    Ok(())
}

#[tokio::test]
async fn source_and_rules() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = json_resource(&c, &format!("ownhealth://source/{}", w.s("vitd"))).await?;
    assert_eq!(env["data"]["fact"]["table"], "observation");
    assert!(env["data"]["text"].is_null());
    let rules: Vec<String> = c
        .list_resources()
        .await?
        .into_iter()
        .filter(|u| u.contains("/rules/"))
        .collect();
    assert!(!rules.is_empty());
    let body = json_resource(&c, &rules[0]).await?;
    assert!(truthy(&body["version"]) && truthy(&body["files"]));
    assert!(c.read_resource("ownhealth://rules/0.0.0-nope").await.is_err());
    Ok(())
}

#[tokio::test]
async fn access_log_owner_only_and_reads_are_audited() -> R {
    let w = world().await?;
    let tok = w
        .token(Tok {
            profiles: vec!["alex", "sam"],
            ..Tok::default()
        })
        .await?;
    let c = w.connect(&tok).await?;
    c.call_tool("get_health_status", json!({"profile": "alex"})).await?;
    let log = json_resource(&c, "ownhealth://profile/alex/access-log").await?;
    assert!(
        log["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e["tool"] == "get_health_status")
    );
    let denied = c
        .read_resource("ownhealth://profile/sam/access-log")
        .await
        .expect_err("denied");
    let missing = c
        .read_resource("ownhealth://source/src_doesnotexist")
        .await
        .expect_err("missing");
    assert_eq!(denied.code, missing.code);
    let reads: Vec<(String, Option<String>)> = audited(&w, &tok[..15])
        .await?
        .into_iter()
        .filter(|r| r.0 == "resources/read")
        .collect();
    let want = |k: &str| ("resources/read".to_string(), Some(k.to_string()));
    assert_eq!(reads, vec![want("ok"), want("denied"), want("denied")]);
    Ok(())
}

async fn prompt_names(c: &Client) -> anyhow::Result<BTreeSet<String>> {
    let r = c
        .rpc("prompts/list", json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(r["prompts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["name"].as_str().map(str::to_string))
        .collect())
}

#[tokio::test]
async fn prompts_filtered_and_rendered() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok_scopes(scopes(&["read:labs"])).await?).await?;
    assert_eq!(
        prompt_names(&c).await?,
        ["check-before-new-drug".to_string()].into_iter().collect()
    );
    let get = |name: &str, args: Value| json!({"name": name, "arguments": args});
    assert!(
        c.rpc("prompts/get", get("prepare-doctor-visit", json!({"profile": "alex"})))
            .await
            .is_err()
    );
    let c = w.connect(&w.tok().await?).await?;
    let want: BTreeSet<String> = ["prepare-doctor-visit", "check-before-new-drug", "what-to-measure-next"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(prompt_names(&c).await?, want);
    let got = c
        .rpc(
            "prompts/get",
            get(
                "check-before-new-drug",
                json!({"profile": "alex", "drug": "simvastatin", "lang": "es"}),
            ),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let body = got["messages"][0]["content"]["text"].as_str().unwrap_or_default();
    assert!(
        body.contains("simvastatin") && body.contains("check_drug") && body.contains("Coméntalo"),
        "{body}"
    );
    assert!(
        c.rpc("prompts/get", get("check-before-new-drug", json!({"profile": "alex"})))
            .await
            .is_err()
    );
    assert!(c.rpc("prompts/get", get("no-such-prompt", json!({}))).await.is_err());
    Ok(())
}
