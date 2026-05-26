//! Minimal MCP server used by `tests/mcp_integration.rs`. Hand-rolled
//! newline-delimited JSON-RPC so the production `ignis` binary doesn't have
//! to depend on rmcp's `server` feature (rmcp's feature flags unify across
//! [dependencies] and [dev-dependencies], so opting into `server` for tests
//! would also pull schemars/server-side code into release builds).
//!
//! Supported behavior via env vars:
//! - `MOCK_INSTRUCTIONS` (optional): included in the initialize response
//! - `MOCK_SLEEP_MS`     (optional): block this many ms before responding to
//!   initialize — exercises the startup-timeout path
//! - `MOCK_ERROR_ON`    (optional): a tool name that always returns is_error=true
//!
//! Tools advertised:
//! - `echo` — returns its `text` argument verbatim
//! - `add`  — returns `a+b` as text
//!
//! Exit on stdin EOF (i.e. when ignis closes the transport).
use std::env;
use std::io::{BufRead, BufReader, Write};

use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() {
    if let Ok(ms) = env::var("MOCK_SLEEP_MS").map(|s| s.parse::<u64>().unwrap_or(0)) {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
    let instructions = env::var("MOCK_INSTRUCTIONS").ok();
    let error_on = env::var("MOCK_ERROR_ON").ok();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut out = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return, // EOF — transport closed
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg): Result<Value, _> = serde_json::from_str(trimmed) else {
            continue; // ignore garbage; some clients send whitespace
        };
        // Notifications carry no `id` and want no response.
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply: Option<Value> = match (method, id.as_ref()) {
            ("initialize", Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": initialize_result(instructions.as_deref()),
            })),
            ("notifications/initialized", _) | ("notifications/cancelled", _) => None,
            ("tools/list", Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "tools": tools_list() },
            })),
            ("tools/call", Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": call_tool(&params, error_on.as_deref()),
            })),
            (_, Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            })),
            _ => None,
        };
        if let Some(reply) = reply {
            writeln!(out, "{reply}").ok();
            out.flush().ok();
        }
    }
}

fn initialize_result(instructions: Option<&str>) -> Value {
    let mut result = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "mock", "version": "0.1.0" },
    });
    if let Some(s) = instructions {
        result["instructions"] = Value::String(s.to_string());
    }
    result
}

fn tools_list() -> Vec<Value> {
    vec![
        json!({
            "name": "echo",
            "description": "Return the `text` argument verbatim.",
            "inputSchema": {
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            },
        }),
        json!({
            "name": "add",
            "description": "Return a + b as text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": { "type": "number" },
                    "b": { "type": "number" },
                },
                "required": ["a", "b"],
            },
        }),
    ]
}

fn call_tool(params: &Value, error_on: Option<&str>) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    if error_on == Some(name) {
        return json!({
            "content": [ { "type": "text", "text": format!("simulated failure in {name}") } ],
            "isError": true,
        });
    }
    let text = match name {
        "echo" => args
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "add" => {
            let a = args.get("a").and_then(Value::as_f64).unwrap_or(0.0);
            let b = args.get("b").and_then(Value::as_f64).unwrap_or(0.0);
            format!("{}", a + b)
        }
        other => {
            return json!({
                "content": [ { "type": "text", "text": format!("unknown tool: {other}") } ],
                "isError": true,
            });
        }
    };
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": false,
    })
}
