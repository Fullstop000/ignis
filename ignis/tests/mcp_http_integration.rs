//! End-to-end tests for the v2 HTTP MCP transport. Spins a minimal hand-rolled
//! Streamable-HTTP MCP server in-process (no extra deps; hyper + hyper-util are
//! already in the workspace) and exercises ignis's `connect_streamable_http`
//! path through the public `McpRegistry::spawn_all` API.
//!
//! Coverage:
//!   1. happy path: connect → list_tools → call_tool over real HTTP
//!   2. bearer-token header reaches the server
//!   3. server-returned 401 surfaces as a clean `Failed` status
//!   4. an unreachable URL fails within the startup timeout
//!
//! Other failure modes (DNS, TLS, mid-session disconnect, reserved-header
//! conflict) are intentionally not covered yet — they're rmcp's surface and
//! cheap to add later if a regression appears.
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use ignis::config::McpServerConfig;
use ignis::mcp::{McpRegistry, McpStatus};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Behavior knobs for the mock server. Default = happy path. Tests opt into
/// quirks (require bearer, return 401, capture last Authorization header)
/// through these fields.
#[derive(Clone, Default)]
struct MockOpts {
    /// If set, every POST without exactly `Authorization: Bearer <this>` returns 401.
    require_bearer: Option<String>,
    /// Records the most recent `Authorization` header value the server saw.
    captured_auth: Arc<Mutex<Option<String>>>,
}

struct MockServer {
    url: String,
    _task: JoinHandle<()>,
}

impl MockServer {
    /// Spawn the server bound to 127.0.0.1 on an OS-assigned port. The task
    /// is left detached; dropping the handle stops it on the next request,
    /// which is fine for short tests.
    async fn spawn(opts: MockOpts) -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("http://{addr}/mcp");
        let opts_for_task = opts.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let opts = opts_for_task.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service_fn(move |req| handle(req, opts.clone())))
                        .await;
                });
            }
        });
        Self { url, _task: task }
    }
}

async fn handle(
    req: Request<Incoming>,
    opts: MockOpts,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Capture Authorization for the assertion tests.
    if let Some(h) = req.headers().get("authorization") {
        if let Ok(s) = h.to_str() {
            *opts.captured_auth.lock().unwrap() = Some(s.to_string());
        }
    }
    if req.method() != Method::POST || req.uri().path() != "/mcp" {
        return Ok(reply_status(StatusCode::NOT_FOUND, "no"));
    }
    if let Some(expected) = &opts.require_bearer {
        let ok = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v == format!("Bearer {expected}"))
            .unwrap_or(false);
        if !ok {
            return Ok(reply_status(StatusCode::UNAUTHORIZED, "no token"));
        }
    }
    // Read the JSON-RPC request body.
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

    match (method, id) {
        ("initialize", Some(id)) => {
            // Mcp-Session-Id is REQUIRED on the initialize response when
            // `allow_stateless = false` (rmcp client default).
            Ok(json_reply_with(
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock", "version": "0.0.1" },
                        "instructions": "mock http server"
                    }
                }),
                Some(("Mcp-Session-Id", "ignis-test-session")),
            ))
        }
        ("notifications/initialized", _) | ("notifications/cancelled", _) => {
            Ok(reply_status(StatusCode::ACCEPTED, ""))
        }
        ("tools/list", Some(id)) => Ok(json_reply(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "echo back the `text` argument",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }]
            }
        }))),
        ("tools/call", Some(id)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let text = params
                .get("arguments")
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(json_reply(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }],
                    "isError": false
                }
            })))
        }
        _ => Ok(reply_status(StatusCode::METHOD_NOT_ALLOWED, "unhandled")),
    }
}

fn json_reply(body: Value) -> Response<Full<Bytes>> {
    json_reply_with(body, None)
}

fn json_reply_with(body: Value, extra_header: Option<(&str, &str)>) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(&body).unwrap();
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json");
    if let Some((k, v)) = extra_header {
        builder = builder.header(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    builder.body(Full::new(Bytes::from(bytes))).unwrap()
}

fn reply_status(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(msg.to_string())))
        .unwrap()
}

fn http_server(url: &str) -> McpServerConfig {
    McpServerConfig {
        url: Some(url.to_string()),
        startup_timeout_secs: 5,
        tool_timeout_secs: 5,
        ..Default::default()
    }
}

#[tokio::test]
async fn connects_lists_tools_and_calls_echo_over_http() {
    let mock = MockServer::spawn(MockOpts::default()).await;
    let mut servers = HashMap::new();
    servers.insert("mock".to_string(), http_server(&mock.url));
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;

    let entries = reg.entries();
    assert_eq!(entries.len(), 1);
    match &entries[0].status {
        McpStatus::Connected { tool_count } => assert_eq!(*tool_count, 1),
        other => panic!("expected Connected, got {other:?}"),
    }

    let wrappers = reg.wrappers();
    let echo = wrappers
        .iter()
        .find(|w| w.qualified_name() == "mcp__mock__echo")
        .expect("mcp__mock__echo wrapper present");

    let result = ignis::AgentTool::call(echo.as_ref(), json!({ "text": "hi over http" })).await;
    assert!(!result.is_error, "got error: {:?}", result.content);
    assert!(
        result.content.contains("hi over http"),
        "content={:?}",
        result.content
    );
    reg.shutdown().await;
}

#[tokio::test]
async fn bearer_token_header_is_sent() {
    // Tell the mock to require a bearer; set the matching env var; verify the
    // server saw Authorization: Bearer <value>. ENV_TEST_LOCK is internal-only;
    // we use a unique env-var name per test instead to keep parallel runs safe.
    std::env::set_var("IGNIS_TEST_HTTP_BEARER_OK", "secret-abc");
    let opts = MockOpts {
        require_bearer: Some("secret-abc".to_string()),
        ..Default::default()
    };
    let captured = opts.captured_auth.clone();
    let mock = MockServer::spawn(opts).await;

    let mut servers = HashMap::new();
    servers.insert(
        "mock".to_string(),
        McpServerConfig {
            url: Some(mock.url.clone()),
            bearer_token_env_var: Some("IGNIS_TEST_HTTP_BEARER_OK".to_string()),
            startup_timeout_secs: 5,
            tool_timeout_secs: 5,
            ..Default::default()
        },
    );
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    std::env::remove_var("IGNIS_TEST_HTTP_BEARER_OK");

    assert!(
        matches!(reg.entries()[0].status, McpStatus::Connected { .. }),
        "status={:?}",
        reg.entries()[0].status
    );
    let auth = captured.lock().unwrap().clone();
    assert_eq!(auth.as_deref(), Some("Bearer secret-abc"));
    reg.shutdown().await;
}

#[tokio::test]
async fn auth_failure_marks_failed_with_401_message() {
    let opts = MockOpts {
        require_bearer: Some("right".to_string()),
        ..Default::default()
    };
    let mock = MockServer::spawn(opts).await;
    let mut servers = HashMap::new();
    // No bearer set → server returns 401 on the very first POST.
    servers.insert("mock".to_string(), http_server(&mock.url));
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;

    match &reg.entries()[0].status {
        McpStatus::Failed { reason } => assert!(
            reason.contains("401"),
            "expected 401 in failure reason, got: {reason}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(reg.wrappers().is_empty());
    reg.shutdown().await;
}

#[tokio::test]
async fn unreachable_url_marks_failed_within_startup_timeout() {
    // Loopback port 1 → no listener; connect refused almost instantly. Budget
    // is 2s; failure must arrive well inside that window.
    let mut servers = HashMap::new();
    servers.insert(
        "dead".to_string(),
        McpServerConfig {
            url: Some("http://127.0.0.1:1/mcp".to_string()),
            startup_timeout_secs: 2,
            tool_timeout_secs: 2,
            ..Default::default()
        },
    );
    let start = std::time::Instant::now();
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "spawn_all took {elapsed:?} — budget was 2s"
    );
    assert!(
        matches!(reg.entries()[0].status, McpStatus::Failed { .. }),
        "status={:?}",
        reg.entries()[0].status
    );
    reg.shutdown().await;
}
