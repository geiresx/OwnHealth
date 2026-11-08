//! The HTTP edge of `/mcp` answers as 0.1.1's Python SDK did: JSON-only clients are served, and a
//! bad request gets a JSON-RPC error body with the code Python used, never rmcp's plain text.

mod common;

use axum::body::Body;
use common::*;
use http::{Request, StatusCode};
use serde_json::{Value, json};

const LEGACY: &str = "2025-11-25";

fn raw(token: &str, accept: &str, content_type: &str, body: &str) -> anyhow::Result<Request<Body>> {
    Ok(Request::post("/mcp")
        .header("host", "127.0.0.1:8080")
        .header("content-type", content_type)
        .header("accept", accept)
        .header("mcp-protocol-version", LEGACY)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))?)
}

async fn post_json(w: &World, token: &str, accept: &str, body: &str) -> anyhow::Result<Http> {
    send(&w.real_app(), raw(token, accept, "application/json", body)?).await
}

fn content_type(r: &Http) -> String {
    r.headers
        .get("content-type")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default()
}

const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

#[tokio::test]
async fn a_json_only_client_is_served() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    for accept in ["application/json", "*/*", "application/*", "Application/JSON; q=0.9"] {
        let r = post_json(&w, &t, accept, PING).await?;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "{accept}: {}",
            String::from_utf8_lossy(&r.body)
        );
        assert!(content_type(&r).starts_with("application/json"), "{accept}");
        let v = r.json();
        assert_eq!((v["id"].clone(), v["result"].is_object()), (json!(1), true), "{v}");
    }
    Ok(())
}

#[tokio::test]
async fn a_client_that_takes_no_json_gets_python_406() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    for accept in ["text/event-stream", "text/html", ""] {
        let r = post_json(&w, &t, accept, PING).await?;
        assert_eq!(r.status, StatusCode::NOT_ACCEPTABLE, "{accept}");
        assert_eq!(content_type(&r), "application/json");
        assert_eq!(
            r.json(),
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600,
                   "message": "Not Acceptable: Client must accept application/json"}})
        );
    }
    Ok(())
}

fn error_of(r: &Http) -> (StatusCode, i64, String) {
    let v = r.json();
    (
        r.status,
        v["error"]["code"].as_i64().unwrap_or_default(),
        v["error"]["message"].as_str().unwrap_or_default().to_string(),
    )
}

#[tokio::test]
async fn a_body_that_is_not_json_is_a_parse_error() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    let r = post_json(&w, &t, "application/json", "{bad json").await?;
    assert_eq!(content_type(&r), "application/json");
    let (status, code, message) = error_of(&r);
    assert_eq!((status, code), (StatusCode::BAD_REQUEST, -32700), "{message}");
    assert_eq!(message, "Parse error: key must be a string at line 1 column 2");
    assert_eq!(r.json()["id"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn a_body_that_is_not_a_json_rpc_message_is_invalid_params() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    for body in [
        "[]",
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
        r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[1]}"#,
        r#"{"jsonrpc":"2.0","id":1}"#,
        "3",
    ] {
        let r = post_json(&w, &t, "application/json", body).await?;
        let (status, code, message) = error_of(&r);
        assert_eq!((status, code), (StatusCode::BAD_REQUEST, -32602), "{body}: {message}");
        assert!(message.starts_with("Validation error: "), "{body}: {message}");
    }
    Ok(())
}

#[tokio::test]
async fn a_message_with_a_null_id_is_a_notification() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    for body in [
        r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    ] {
        let r = post_json(&w, &t, "application/json", body).await?;
        assert_eq!(
            r.status,
            StatusCode::ACCEPTED,
            "{body}: {}",
            String::from_utf8_lossy(&r.body)
        );
        assert!(r.body.is_empty(), "{body}");
    }
    Ok(())
}

#[tokio::test]
async fn bad_params_of_a_real_method_are_invalid_params_not_method_not_found() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    for params in [json!({}), json!({"name": "get_trend", "arguments": "x"})] {
        let body = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": params}).to_string();
        let r = post_json(&w, &t, "application/json", &body).await?;
        let v = r.json();
        assert_eq!(
            v["error"],
            json!({"code": -32602, "message": "Invalid request parameters", "data": ""}),
            "{params}: {v}"
        );
        assert_eq!(v["id"], json!(7));
    }
    Ok(())
}

#[tokio::test]
async fn an_unknown_method_names_itself_in_data() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    let body = r#"{"jsonrpc":"2.0","id":3,"method":"bogus/method"}"#;
    let r = post_json(&w, &t, "application/json", body).await?;
    let v = r.json();
    assert_eq!(
        v["error"],
        json!({"code": -32601, "message": "Method not found", "data": "bogus/method"}),
        "{v}"
    );
    Ok(())
}

#[tokio::test]
async fn a_body_that_is_not_declared_json_is_refused_as_python_did() -> R {
    let w = world().await?;
    let t = w.tok().await?;
    let r = send(&w.real_app(), raw(&t, "application/json", "text/plain", PING)?).await?;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(String::from_utf8_lossy(&r.body), "Invalid Content-Type header");
    Ok(())
}
