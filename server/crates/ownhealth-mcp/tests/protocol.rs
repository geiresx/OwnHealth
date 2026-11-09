//! `tests/mcp/test_mcp_protocol.py`: transport, protocol versions, the PAT door and the shape of
//! tools/list.

mod common;

use axum::body::Body;
use common::*;
use http::Request;
use ownhealth_mcp::INSTRUCTIONS;
use serde_json::{Value, json};

const FORBIDDEN_WORDS: [&str; 7] = ["share", "export", "send", "email", "url", "webhook", "upload"];

#[tokio::test]
async fn both_protocol_versions_on_one_endpoint() -> R {
    let w = world().await?;
    for (mode, expected) in [
        ("2026-07-28", "2026-07-28"),
        ("legacy", "2025-11-25"),
        ("auto", "2026-07-28"),
    ] {
        let tok = w.tok().await?;
        let c = Client::connect(w.app(), Some(&tok), mode).await?;
        assert_eq!(c.protocol_version, expected);
        let env = c.call_tool("get_health_status", json!({"profile": "alex"})).await?.ok();
        assert_envelope(&env);
        let pv: Option<String> = sqlx::query_scalar(
            "select protocol_version from audit_event where tool = 'get_health_status' order by id desc limit 1",
        )
        .fetch_one(&w.db.pool)
        .await?;
        assert_eq!(pv.as_deref(), Some(expected), "{mode}");
    }
    Ok(())
}

#[tokio::test]
async fn missing_bad_and_revoked_tokens_are_401() -> R {
    let w = world().await?;
    let app = w.app();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let hdrs = [("mcp-protocol-version", "2025-11-25")];
    let tok = w.tok().await?;
    let r = send(&app, post(&body, &hdrs)?).await?;
    assert_eq!(r.status, 401);
    let www = r.headers["www-authenticate"].to_str()?;
    assert!(
        www.contains(r#"resource_metadata="http://127.0.0.1:8080/.well-known/oauth-protected-resource""#),
        "{www}"
    );
    let meta = send(
        &app,
        Request::get("/.well-known/oauth-protected-resource").body(Body::empty())?,
    )
    .await?;
    assert_eq!(meta.status, 200);
    let m = meta.json();
    assert_eq!(m["resource"], "http://127.0.0.1:8080/mcp");
    assert_eq!(m["bearer_methods_supported"], json!(["header"]));
    assert!(m.get("authorization_servers").is_none());
    let r = send(
        &app,
        post(&body, &[hdrs[0], ("authorization", "Bearer oh_pat_AAAAAAAA_nope")])?, // ggignore: deliberately invalid token
    )
    .await?;
    assert_eq!(r.status, 401);
    assert!(
        r.headers["www-authenticate"]
            .to_str()?
            .contains(r#"error="invalid_token""#)
    );
    let auth = format!("Bearer {tok}");
    let r = send(&app, post(&body, &[hdrs[0], ("authorization", &auth)])?).await?;
    assert_ne!(r.status, 401);
    sqlx::query("update api_token set revoked_at = now() where prefix = $1")
        .bind(&tok[..15])
        .execute(&w.db.pool)
        .await?;
    let r = send(&app, post(&body, &[hdrs[0], ("authorization", &auth)])?).await?;
    assert_eq!(r.status, 401);
    Ok(())
}

#[tokio::test]
async fn origin_must_match_the_public_url() -> R {
    let w = world().await?;
    let app = w.app();
    let tok = w.tok().await?;
    let auth = format!("Bearer {tok}");
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
    let base = [("authorization", auth.as_str()), ("mcp-protocol-version", "2025-11-25")];
    let r = send(
        &app,
        post(&init, &[base[0], base[1], ("origin", "https://evil.example")])?,
    )
    .await?;
    assert_eq!(r.status, 403);
    assert_eq!(
        r.json(),
        json!({"error": "origin_not_allowed", "message": "Origin not allowed."})
    );
    let r = send(
        &app,
        post(&init, &[base[0], base[1], ("origin", "http://127.0.0.1:8080")])?,
    )
    .await?;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    Ok(())
}

#[tokio::test]
async fn tool_list_is_sorted_annotated_and_has_no_outbound_tool() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let tools = c.list_tools().await?;
    let names: Vec<String> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    let mut reg: Vec<String> = ownhealth_mcp::tools::REGISTRY
        .iter()
        .map(|s| s.name.to_string())
        .collect();
    reg.sort();
    assert_eq!(names, reg);
    // the eight tools of 0.2.0 and the two of v0.2 agent extraction
    assert_eq!(names.len(), 10);
    assert!(!names.contains(&"ping".to_string()));
    for t in &tools {
        assert_eq!(t["annotations"]["openWorldHint"], json!(false));
        assert_eq!(t["annotations"]["readOnlyHint"], json!(t["name"] != "propose"));
        let blob = format!("{}{}", t["name"].as_str().unwrap_or_default(), dumps(&t["inputSchema"])).to_lowercase();
        assert!(!FORBIDDEN_WORDS.iter().any(|w| blob.contains(w)), "{}", t["name"]);
        assert_eq!(t["outputSchema"]["type"], "object");
    }
    assert!(INSTRUCTIONS.contains("untrusted_text"));
    Ok(())
}

#[tokio::test]
async fn tools_list_follows_scopes() -> R {
    let w = world().await?;
    let cases: [(&[&str], &[&str]); 6] = [
        (&["read:summary"], &["get_health_status", "get_source"]),
        (
            &["read:summary", "read:labs", "read:vitals"],
            &[
                "get_health_status",
                "get_source",
                "search_observations",
                "get_trend",
                "check_drug",
            ],
        ),
        (&["read:genome_raw"], &[]),
        (
            &["read:genome", "read:genome_raw"],
            &["get_genome_findings", "lookup_variant", "check_drug", "get_source"],
        ),
        (
            &["read:records", "write:propose"],
            &["check_drug", "get_source", "list_pending_documents", "propose"],
        ),
        (
            &["read:documents_text", "write:propose"],
            &["get_document_page", "list_pending_documents", "propose"],
        ),
    ];
    for (s, expected) in cases {
        let c = w.connect(&w.tok_scopes(scopes(s)).await?).await?;
        let names = c.tool_names().await?;
        let want: std::collections::BTreeSet<String> = expected.iter().map(|x| x.to_string()).collect();
        assert_eq!(names, want, "{s:?}");
    }
    Ok(())
}

#[tokio::test]
async fn tools_list_follows_the_grant_not_only_the_token() -> R {
    let w = world().await?;
    let tok = w
        .token(Tok {
            user: "sam",
            ..Tok::default()
        })
        .await?;
    let c = w.connect(&tok).await?;
    let names = c.tool_names().await?;
    assert_eq!(
        names,
        ["get_health_status", "get_source"]
            .iter()
            .map(|x| x.to_string())
            .collect()
    );
    let r = c
        .call_tool("search_observations", json!({"profile": "alex", "query": "ldl"}))
        .await?;
    assert!(r.is_error);
    assert_eq!(r.sc["code"], "scope_missing");
    let _: Value = Value::Null;
    Ok(())
}

/// 2026-07-28 results carry `resultType: "complete"` (the Python SDK client refuses a list without
/// it); a legacy peer gets the old shape without it.
#[tokio::test]
async fn result_type_follows_the_protocol_version() -> R {
    let w = world().await?;
    let tok = w.tok().await?;
    for (mode, want) in [("2026-07-28", Some("complete")), ("legacy", None)] {
        let c = Client::connect(w.app(), Some(&tok), mode).await?;
        for (method, params) in [
            ("tools/list", json!({})),
            ("resources/list", json!({})),
            ("prompts/list", json!({})),
            (
                "tools/call",
                json!({"name": "get_health_status", "arguments": {"profile": "alex"}}),
            ),
            ("resources/read", json!({"uri": "ownhealth://profiles"})),
        ] {
            let r = c
                .rpc(method, params)
                .await
                .map_err(|e| anyhow::anyhow!("{method}: {e}"))?;
            assert_eq!(r.get("resultType").and_then(Value::as_str), want, "{mode} {method}");
        }
    }
    Ok(())
}
