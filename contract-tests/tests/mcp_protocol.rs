//! `/mcp` as a transport: both protocol versions on one stateless endpoint, the PAT door, the
//! Origin check, the protected-resource metadata, and tools/list, resources and prompts filtered by
//! the token and the grant. Python: tests/mcp/test_mcp_protocol.py, test_mcp_resources.py.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_exact;
use ownhealth_contract_tests::{
    ALL_SCOPES, Client, INSTRUCTIONS_PREFIX, LEGACY, MODERN, Mcp, assert_envelope, uniq,
};
use serde_json::{Value, json};

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// The server version differs by backend (0.1.1, 0.2.0); everything else must match.
fn without_version(mut v: Value) -> Value {
    for p in [
        "/serverInfo/version",
        "/_meta/io.modelcontextprotocol~1serverInfo/version",
    ] {
        if let Some(x) = v.pointer_mut(p) {
            *x = json!("<version>");
        }
    }
    v
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn both_protocol_versions_on_one_stateless_endpoint() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, plain) = c.token(json!({"scopes": ALL_SCOPES})).await?;
    let legacy = Mcp::new(&plain)?;
    let init = legacy.initialize().await?;
    assert_eq!(init["protocolVersion"], LEGACY);
    assert_eq!(init["serverInfo"]["name"], "OwnHealth");
    assert!(
        init["instructions"]
            .as_str()
            .is_some_and(|s| s.starts_with(INSTRUCTIONS_PREFIX) && s.contains("untrusted_text"))
    );
    assert_exact("mcp_initialize_legacy", &without_version(init))?;
    // stateless: no session id, and a call works without a handshake
    let raw = legacy
        .post_raw(
            &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"}),
            &[],
        )
        .await?;
    assert_eq!(raw.status, 200);
    assert!(
        raw.header("mcp-session-id").is_none(),
        "the server must be stateless"
    );
    assert!(
        raw.header("content-type")
            .is_some_and(|t| t.starts_with("application/json")),
        "JSON responses, not SSE"
    );
    assert_eq!(raw.json["id"], 7);
    assert_envelope(
        &legacy
            .ok("get_health_status", json!({"profile": "alex"}))
            .await?,
    )?;

    let modern = Mcp::new(&plain)?.modern();
    let disc = modern.result("server/discover", json!({})).await?;
    assert_eq!(disc["supportedVersions"], json!([MODERN]));
    assert_exact("mcp_discover_modern", &without_version(disc))?;
    let env = modern
        .ok("get_health_status", json!({"profile": "alex"}))
        .await?;
    assert_envelope(&env)?;
    // the modern list carries a private cache hint
    let list = modern.result("tools/list", json!({})).await?;
    assert_eq!(
        (list["ttlMs"].clone(), list["cacheScope"].clone()),
        (json!(60000), json!("private"))
    );
    // a modern request without its envelope or with mismatched headers is refused
    let raw = modern
        .post_raw(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
            &[("mcp-method", "tools/list")],
        )
        .await?;
    assert_eq!(raw.json["error"]["code"], -32602, "{}", raw.json);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn missing_bad_and_revoked_tokens_are_401_with_the_metadata_url() -> anyhow::Result<()> {
    let c = Client::new()?;
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let http = reqwest::Client::new();
    let url = format!("{}/mcp", c.base);
    let r = http
        .post(&url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", LEGACY)
        .body(msg.to_string())
        .send()
        .await?;
    assert_eq!(r.status().as_u16(), 401);
    let www = r
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        www,
        format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
            c.base
        )
    );
    let body: Value = r.json().await?;
    assert_eq!(
        body,
        json!({"error": "invalid_token", "message": "A bearer token is required."})
    );

    let meta = c
        .get("/.well-known/oauth-protected-resource")
        .await?
        .expect(200)?
        .json;
    let mut scopes = ALL_SCOPES.to_vec();
    scopes.sort_unstable();
    assert_eq!(
        meta,
        json!({"resource": format!("{}/mcp", c.base), "resource_name": "OwnHealth", "bearer_methods_supported": ["header"], "scopes_supported": scopes})
    );

    let bad = Mcp::new("oh_pat_AAAAAAAA_nope")?
        .post_raw(&msg, &[])
        .await?;
    assert_eq!(bad.status, 401);
    assert!(
        bad.header("www-authenticate")
            .is_some_and(|h| h.ends_with(", error=\"invalid_token\""))
    );
    assert_eq!(
        bad.json,
        json!({"error": "invalid_token", "message": "Unknown, expired or revoked token."})
    );

    // a browser session is no token: /mcp never accepts the cookie or dev auth
    let raw = http
        .post(&url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("cookie", "oh_session=whatever")
        .body(msg.to_string())
        .send()
        .await?;
    assert_eq!(raw.status().as_u16(), 401);

    let (t, plain) = c.token(json!({})).await?;
    let ok = Mcp::new(&plain)?.post_raw(&msg, &[]).await?;
    assert_eq!(ok.status, 200);
    c.delete(&format!("/api/tokens/{}", t["id"].as_str().context("id")?))
        .await?
        .expect(200)?;
    assert_eq!(Mcp::new(&plain)?.post_raw(&msg, &[]).await?.status, 401);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn origin_must_match_the_public_url() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, plain) = c.token(json!({})).await?;
    let m = Mcp::new(&plain)?;
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                      "params": {"protocolVersion": LEGACY, "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
    let r = m
        .post_raw(&init, &[("origin", "https://evil.example")])
        .await?;
    assert_eq!(r.status, 403);
    assert_eq!(
        r.json,
        json!({"error": "origin_not_allowed", "message": "Origin not allowed."})
    );
    let base = c.base.clone();
    let r = m.post_raw(&init, &[("origin", base.as_str())]).await?;
    assert_eq!(r.status, 200);
    let r = m
        .post_raw(&init, &[("origin", format!("{base}/").as_str())])
        .await?;
    assert_eq!(r.status, 200, "a trailing slash is the same origin");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn tools_list_is_sorted_annotated_and_exactly_as_shipped() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let tools = m.list_tools().await?;
    let n = names(&tools);
    let mut sorted = n.clone();
    sorted.sort();
    assert_eq!(n, sorted);
    assert_eq!(
        n,
        [
            "check_drug",
            "get_genome_findings",
            "get_health_status",
            "get_source",
            "get_trend",
            "lookup_variant",
            "propose",
            "search_observations"
        ]
    );
    for t in &tools {
        assert_eq!(t["annotations"]["openWorldHint"], false);
        assert_eq!(
            t["annotations"]["readOnlyHint"],
            json!(t["name"] != "propose")
        );
        assert_eq!(t["outputSchema"]["type"], "object");
        let blob = format!("{}{}", t["name"], t["inputSchema"]).to_lowercase();
        for w in [
            "share", "export", "send", "email", "url", "webhook", "upload",
        ] {
            assert!(!blob.contains(w), "{} mentions {w}", t["name"]);
        }
    }
    // names, titles, descriptions, input and output schemas and annotations, byte for byte
    assert_exact("mcp_tools_list_all_scopes", &json!(tools))?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn tools_list_follows_the_token_scopes() -> anyhow::Result<()> {
    let c = Client::new()?;
    let cases: [(&[&str], &[&str]); 6] = [
        (&["read:summary"], &["get_health_status", "get_source"]),
        (
            &["read:summary", "read:labs", "read:vitals"],
            &[
                "check_drug",
                "get_health_status",
                "get_source",
                "get_trend",
                "search_observations",
            ],
        ),
        (&["read:genome_raw"], &[]),
        (
            &["read:genome", "read:genome_raw"],
            &[
                "check_drug",
                "get_genome_findings",
                "get_source",
                "lookup_variant",
            ],
        ),
        (
            &["read:records", "write:propose"],
            &["check_drug", "get_source", "propose"],
        ),
        (
            &["read:wearables"],
            &["get_source", "get_trend", "search_observations"],
        ),
    ];
    for (scopes, want) in cases {
        let m = c.mcp(scopes, &["alex"]).await?;
        let mut got = m.tool_names().await?;
        got.sort();
        assert_eq!(got, want, "{scopes:?}");
        // a tool the token cannot use is refused by name, with the same error as an unknown scope
        if !got.contains(&"propose".to_string()) {
            let r = m
                .call(
                    "propose",
                    json!({"profile": "alex", "kind": "note", "payload": {"text": "x"}}),
                )
                .await?;
            assert_eq!(r.code(), Some("scope_missing"), "{scopes:?}");
        }
    }
    let m = c.mcp(&["read:summary"], &["alex"]).await?;
    let r = m.call("no_such_tool", json!({})).await?;
    assert_eq!(r.code(), Some("bad_input"));
    assert_eq!(r.sc["message"], "Unknown tool 'no_such_tool'.");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn tools_list_follows_the_grant_not_only_the_token() -> anyhow::Result<()> {
    let c = Client::new()?;
    let slug = c.new_relative().await?;
    let m = c.enrolled_member(&format!("Viewer {}", uniq())).await?;
    c.put(
        "/api/grants",
        &json!({"user_id": m.user_id, "profile": slug, "role": "viewer"}),
    )
    .await?
    .expect(200)?;
    let (_, plain) = m
        .client
        .token(json!({"scopes": ALL_SCOPES, "profiles": [slug]}))
        .await?;
    let mcp = Mcp::new(&plain)?;
    let mut got = mcp.tool_names().await?;
    got.sort();
    assert_eq!(
        got,
        ["get_health_status", "get_source"],
        "a viewer's grant is read:summary"
    );
    let r = mcp
        .call(
            "search_observations",
            json!({"profile": slug, "query": "ldl"}),
        )
        .await?;
    assert_eq!(r.code(), Some("scope_missing"));
    // a profile outside the token is profile_not_granted, naming the allowed slugs
    let r = mcp
        .call("get_health_status", json!({"profile": "alex"}))
        .await?;
    assert_eq!(r.code(), Some("profile_not_granted"));
    assert_eq!(r.sc["allowed"], json!([slug]));
    assert_eq!(r.sc["fix"], json!(format!("Use one of: {slug}.")));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn resources_templates_and_prompts() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let res = m.result("resources/list", json!({})).await?;
    assert_exact("mcp_resources_list_alex", &res)?;
    let tpl = m.result("resources/templates/list", json!({})).await?;
    assert_exact("mcp_resource_templates", &tpl)?;
    let prompts = m.result("prompts/list", json!({})).await?;
    assert_exact("mcp_prompts_all_scopes", &prompts)?;

    // the status resource is the tool's answer: same state_hash
    let read = m
        .result(
            "resources/read",
            json!({"uri": "ownhealth://profile/alex/status"}),
        )
        .await?;
    let text = read["contents"][0]["text"].as_str().context("contents")?;
    let env: Value = serde_json::from_str(text)?;
    let tool = m
        .ok("get_health_status", json!({"profile": "alex"}))
        .await?;
    assert_eq!(env["data"]["state_hash"], tool["data"]["state_hash"]);
    assert_eq!(read["contents"][0]["mimeType"], "application/json");
    let md = m
        .result(
            "resources/read",
            json!({"uri": "ownhealth://profile/alex/status.md"}),
        )
        .await?;
    assert_eq!(md["contents"][0]["mimeType"], "text/markdown");
    let profiles = m
        .result("resources/read", json!({"uri": "ownhealth://profiles"}))
        .await?;
    let p: Value = serde_json::from_str(
        profiles["contents"][0]["text"]
            .as_str()
            .context("profiles")?,
    )?;
    assert!(p.to_string().contains("alex"));
    let r = m
        .request(
            "resources/read",
            json!({"uri": "ownhealth://profile/sam/status"}),
        )
        .await?;
    assert!(
        !r["error"].is_null(),
        "a profile outside the token is refused: {r}"
    );

    // a summary-only token sees fewer prompts, and a rendered prompt names the profile
    let few = c.mcp(&["read:summary"], &["alex"]).await?;
    let fp = few.result("prompts/list", json!({})).await?;
    let all_n = prompts["prompts"].as_array().map(Vec::len).unwrap_or(0);
    let few_n = fp["prompts"].as_array().map(Vec::len).unwrap_or(0);
    assert!(few_n < all_n, "{few_n} < {all_n}");
    let got = m
        .result(
            "prompts/get",
            json!({"name": "what-to-measure-next", "arguments": {"profile": "alex", "lang": "en"}}),
        )
        .await?;
    assert!(got["messages"].to_string().contains("alex"), "{got}");
    let r = m
        .request(
            "prompts/get",
            json!({"name": "no-such-prompt", "arguments": {}}),
        )
        .await?;
    assert_eq!(r["error"]["code"], -32602);
    Ok(())
}
