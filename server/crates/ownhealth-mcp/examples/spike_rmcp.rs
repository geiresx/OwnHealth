//! Spike (a): one rmcp tool inside axum at /mcp, stateless, JSON responses.
//! `cargo run -p ownhealth-mcp --example spike_rmcp -- 127.0.0.1:18765`, then call it with the
//! Python client in `scripts/spike_mcp_client.py`.

use std::sync::Arc;

use axum::Router;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::json;

#[derive(Clone)]
struct Spike;

fn ping_tool() -> Tool {
    let schema = json!({"type": "object", "properties": {"echo": {"type": "string"}}, "additionalProperties": false});
    let serde_json::Value::Object(map) = schema else {
        unreachable!()
    };
    Tool::new("ping", "Answers pong and echoes its argument.", Arc::new(map))
}

impl ServerHandler for Spike {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("OwnHealth", "0.2.0-spike"))
            .with_instructions("spike")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![ping_tool()])
            .with_ttl_ms(60_000)
            .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let auth = context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|p| p.headers.get("authorization"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string();
        let echo = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("echo"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let structured = json!({"pong": true, "echo": echo, "authorization_seen": auth != "-"});
        Ok(CallToolResult::structured(structured).into())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:18765".into());
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(["127.0.0.1", "localhost"]);
    let service: StreamableHttpService<Spike, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(Spike), Default::default(), config);
    let _ = ContentBlock::text("");
    let app = Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("listening on http://{addr}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}
