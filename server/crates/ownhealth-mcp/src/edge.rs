//! The HTTP edge of a POST to `/mcp`, in front of rmcp, answering as 0.1.1's Python SDK did
//! (`mcp/server/streamable_http.py` with `json_response=True`): a client that accepts JSON alone
//! is served, and a request rmcp would refuse in plain text gets Python's status and JSON-RPC body.
//! GET and DELETE pass through untouched.

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use rmcp::ErrorData;
use rmcp::model::ErrorCode;
use serde_json::{Map, Value, json};

/// Python's `DEFAULT_MAX_REQUEST_BODY_SIZE`, which is also rmcp's default.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// The request methods Python validates by shape before routing (`mcp_types.methods.SPEC_CLIENT_METHODS`).
const SPEC_CLIENT_METHODS: [&str; 15] = [
    "completion/complete",
    "initialize",
    "logging/setLevel",
    "ping",
    "prompts/get",
    "prompts/list",
    "resources/list",
    "resources/read",
    "resources/subscribe",
    "resources/templates/list",
    "resources/unsubscribe",
    "server/discover",
    "subscriptions/listen",
    "tools/call",
    "tools/list",
];

/// The error for a request rmcp routed to no handler: a spec method there failed to parse its
/// params, anything else is unknown (Python's `runner.py`).
pub fn unrouted(method: &str) -> ErrorData {
    if SPEC_CLIENT_METHODS.contains(&method) {
        ErrorData::invalid_params("Invalid request parameters", Some(json!("")))
    } else {
        ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Method not found", Some(json!(method)))
    }
}

pub async fn json_rpc_edge(req: Request, next: Next) -> Response {
    if req.method() != Method::POST {
        return next.run(req).await;
    }
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).to_lowercase());
    // TransportSecurity.validate_request runs first, even with DNS-rebinding protection off
    let Some(content_type) = content_type.filter(|c| c.starts_with("application/json")) else {
        return plain(StatusCode::BAD_REQUEST, "Invalid Content-Type header");
    };
    if !accepts_json(req.headers()) {
        return rpc_error(
            StatusCode::NOT_ACCEPTABLE,
            -32600,
            "Not Acceptable: Client must accept application/json",
        );
    }
    let essence = content_type.split(';').next().unwrap_or_default();
    if !essence.split(',').any(|p| p.trim() == "application/json") {
        return rpc_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            -32600,
            "Unsupported Media Type: Content-Type must be application/json",
        );
    }
    let (mut parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_BODY).await else {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
    };
    let message: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        // serde_json and pydantic's jiter word their errors alike ("key must be a string at line 1 column 2")
        Err(e) => return rpc_error(StatusCode::BAD_REQUEST, -32700, &format!("Parse error: {e}")),
    };
    match classify(&message) {
        Kind::Invalid(why) => {
            return rpc_error(StatusCode::BAD_REQUEST, -32602, &format!("Validation error: {why}"));
        }
        Kind::Accepted => return accepted(),
        Kind::Forward => {}
    }
    // rmcp insists on both media types; the answer stays JSON because json_response is on
    parts.headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    let res = next.run(Request::from_parts(parts, Body::from(bytes))).await;
    if res.status() != StatusCode::UNSUPPORTED_MEDIA_TYPE {
        return res;
    }
    // rmcp could not read a message the checks above let through: a shape error to Python
    let text = to_bytes(res.into_body(), MAX_BODY).await.unwrap_or_default();
    rpc_error(
        StatusCode::BAD_REQUEST,
        -32602,
        &format!("Validation error: {}", String::from_utf8_lossy(&text)),
    )
}

/// Python's `check_accept_headers`, JSON half.
fn accepts_json(headers: &HeaderMap) -> bool {
    let accept = headers
        .get(header::ACCEPT)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    accept
        .split(',')
        .map(|t| t.trim().split(';').next().unwrap_or_default().trim().to_lowercase())
        .any(|t| matches!(t.as_str(), "*/*" | "application/json" | "application/*"))
}

enum Kind {
    Invalid(&'static str),
    /// A notification rmcp cannot read, a response or an error: Python answers 202 and moves on.
    Accepted,
    Forward,
}

/// `jsonrpc_message_adapter`: a request (`id` a strict int or a string), a notification (any other
/// `id` is ignored), a response or an error; extra keys are ignored.
fn classify(message: &Value) -> Kind {
    let Some(m) = message.as_object() else {
        return Kind::Invalid("a JSON-RPC message is an object");
    };
    if m.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Kind::Invalid("jsonrpc must be \"2.0\"");
    }
    let id = m.get("id");
    let request_id = id.is_some_and(|v| v.is_string() || v.is_i64() || v.is_u64());
    if let Some(method) = m.get("method") {
        if !method.is_string() {
            return Kind::Invalid("method must be a string");
        }
        if !matches!(m.get("params"), None | Some(Value::Null | Value::Object(_))) {
            return Kind::Invalid("params must be an object");
        }
        return if request_id || id.is_none() {
            Kind::Forward
        } else {
            Kind::Accepted
        };
    }
    if request_id && m.get("result").is_some_and(Value::is_object) {
        return Kind::Accepted;
    }
    if (request_id || id == Some(&Value::Null)) && is_error_data(m.get("error")) {
        return Kind::Accepted;
    }
    Kind::Invalid("not a request, notification, response or error")
}

fn is_error_data(v: Option<&Value>) -> bool {
    v.and_then(Value::as_object).is_some_and(|e: &Map<String, Value>| {
        e.get("code").is_some_and(|c| c.is_i64()) && e.get("message").is_some_and(Value::is_string)
    })
}

fn response(status: StatusCode, content_type: Option<&'static str>, body: Body) -> Response {
    let mut res = Response::new(body);
    *res.status_mut() = status;
    if let Some(ct) = content_type {
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    }
    res
}

/// Starlette's bare `Response(text)`: no content type.
fn plain(status: StatusCode, text: &'static str) -> Response {
    response(status, None, Body::from(text))
}

fn accepted() -> Response {
    response(StatusCode::ACCEPTED, Some("application/json"), Body::empty())
}

/// `_create_error_response`: a JSON-RPC error with a null id.
fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    let body = json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}});
    response(status, Some("application/json"), Body::from(body.to_string()))
}
