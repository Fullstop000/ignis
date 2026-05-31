//! End-to-end tests against the in-tree mock MCP server
//! (`tests/fixtures/mock_mcp_server/main.rs`).
//!
//! All tests use the real subprocess + the real rmcp client; only the *peer*
//! is stubbed. The mock binary is located via the `CARGO_BIN_EXE_<name>` env
//! var cargo sets for [[bin]] targets in the same crate.
use std::collections::{HashMap, HashSet};

use ignis::config::McpServerConfig;
use ignis::mcp::{McpRegistry, McpStatus};

fn mock_path() -> String {
    env!("CARGO_BIN_EXE_mock_mcp_server").to_string()
}

fn server(env: &[(&str, &str)]) -> McpServerConfig {
    McpServerConfig {
        command: Some(mock_path()),
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        startup_timeout_secs: 5,
        tool_timeout_secs: 5,
        ..Default::default()
    }
}

#[tokio::test]
async fn connects_lists_tools_and_calls_echo() {
    let mut servers = HashMap::new();
    servers.insert("mock".to_string(), server(&[]));
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;

    let entries = reg.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "mock");
    assert!(matches!(entries[0].status, McpStatus::Connected { .. }));

    let wrappers = reg.wrappers();
    let names: Vec<&str> = wrappers.iter().map(|w| w.qualified_name()).collect();
    assert!(names.contains(&"mcp__mock__echo"), "names={names:?}");
    assert!(names.contains(&"mcp__mock__add"), "names={names:?}");

    let echo = wrappers
        .iter()
        .find(|w| w.qualified_name() == "mcp__mock__echo")
        .unwrap();
    let result =
        ignis::AgentTool::call(echo.as_ref(), serde_json::json!({ "text": "hello" })).await;
    assert!(!result.is_error, "result was an error: {}", result.content);
    assert_eq!(result.content, "hello");

    reg.shutdown().await;
}

#[tokio::test]
async fn server_instructions_appear_in_block() {
    let mut servers = HashMap::new();
    servers.insert(
        "guide".to_string(),
        server(&[("MOCK_INSTRUCTIONS", "Always reply in haiku.")]),
    );
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;

    let block = reg
        .instructions_block()
        .expect("connected server with instructions should produce a block");
    assert!(block.contains("<server name=\"guide\">"));
    assert!(block.contains("Always reply in haiku."));

    reg.shutdown().await;
}

#[tokio::test]
async fn bad_command_is_recorded_as_failed_but_session_continues() {
    let mut servers = HashMap::new();
    servers.insert(
        "ghost".to_string(),
        McpServerConfig {
            command: Some("/no/such/path/please_dont_exist".to_string()),
            startup_timeout_secs: 2,
            tool_timeout_secs: 2,
            ..Default::default()
        },
    );
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;

    let entries = reg.entries();
    assert_eq!(entries.len(), 1);
    match &entries[0].status {
        McpStatus::Failed { reason } => {
            assert!(
                reason.to_lowercase().contains("spawn"),
                "expected spawn error in reason, got: {reason}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // No wrappers contributed by a failed server.
    assert!(reg.wrappers().is_empty());
    reg.shutdown().await;
}

#[tokio::test]
async fn startup_timeout_marks_failed_within_budget() {
    let mut servers = HashMap::new();
    servers.insert(
        "slow".to_string(),
        McpServerConfig {
            command: Some(mock_path()),
            // Mock sleeps 3s before initialize; budget is 1s → must time out.
            env: HashMap::from([("MOCK_SLEEP_MS".to_string(), "3000".to_string())]),
            startup_timeout_secs: 1,
            tool_timeout_secs: 2,
            ..Default::default()
        },
    );
    let start = std::time::Instant::now();
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "spawn_all should return within ~timeout, took {elapsed:?}"
    );
    match &reg.entries()[0].status {
        McpStatus::Failed { reason } => assert!(reason.contains("timeout")),
        other => panic!("expected Failed{{timeout}}, got {other:?}"),
    }
    reg.shutdown().await;
}

#[tokio::test]
async fn disabled_server_does_not_connect_and_has_no_tools() {
    let mut servers = HashMap::new();
    servers.insert("mock".to_string(), server(&[]));
    let mut disabled = HashSet::new();
    disabled.insert("mock".to_string());
    let reg = McpRegistry::spawn_all(&servers, disabled).await;

    let entries = reg.entries();
    assert!(matches!(entries[0].status, McpStatus::Disabled));
    assert!(reg.wrappers().is_empty());
    assert!(reg.instructions_block().is_none());
    reg.shutdown().await;
}

#[tokio::test]
async fn tool_returning_is_error_surfaces_as_tool_result_error() {
    let mut servers = HashMap::new();
    servers.insert("boom".to_string(), server(&[("MOCK_ERROR_ON", "echo")]));
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;

    let echo = reg
        .wrappers()
        .into_iter()
        .find(|w| w.qualified_name() == "mcp__boom__echo")
        .unwrap();
    let result = ignis::AgentTool::call(echo.as_ref(), serde_json::json!({ "text": "x" })).await;
    assert!(result.is_error);
    assert!(result.content.contains("simulated failure"));
    reg.shutdown().await;
}

#[tokio::test]
async fn oversize_qualified_tool_name_is_skipped() {
    // 60 chars + `mcp__skip__` (11) = 71 → over the 64-char OpenAI cap.
    let huge_name = "x".repeat(60);
    let mut servers = HashMap::new();
    servers.insert(
        "skip".to_string(),
        server(&[("MOCK_EXTRA_TOOLS", huge_name.as_str())]),
    );
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    let names: Vec<String> = reg
        .wrappers()
        .iter()
        .map(|w| w.qualified_name().to_string())
        .collect();
    assert!(names.contains(&"mcp__skip__echo".to_string()));
    assert!(names.contains(&"mcp__skip__add".to_string()));
    assert!(
        !names.iter().any(|n| n.contains("xxxxxxxx")),
        "oversize tool should be skipped, got {names:?}"
    );
    reg.shutdown().await;
}

#[tokio::test]
async fn sanitize_collision_is_skipped_not_silently_shadowed() {
    // Both names sanitize to `read_file`. The second occurrence must be
    // skipped, leaving only the first.
    let mut servers = HashMap::new();
    servers.insert(
        "dup".to_string(),
        server(&[("MOCK_EXTRA_TOOLS", "read.file;read/file")]),
    );
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    let names: Vec<String> = reg
        .wrappers()
        .iter()
        .map(|w| w.qualified_name().to_string())
        .collect();
    let read_file_count = names.iter().filter(|n| *n == "mcp__dup__read_file").count();
    assert_eq!(
        read_file_count, 1,
        "exactly one read_file should win after sanitization, got {names:?}"
    );
    reg.shutdown().await;
}

#[tokio::test]
async fn zero_servers_means_zero_overhead() {
    let servers: HashMap<String, McpServerConfig> = HashMap::new();
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    assert!(reg.is_empty());
    assert!(reg.wrappers().is_empty());
    assert!(reg.instructions_block().is_none());
    reg.shutdown().await;
}

/// Wire-format smoke test against a real published MCP server
/// (`@modelcontextprotocol/server-filesystem`). Requires `npx` on PATH and a
/// working npm registry, so it's `#[ignore]` by default — run explicitly with
/// `cargo test --test mcp_integration -- --ignored`.
#[tokio::test]
#[ignore = "requires npx + network; run with --ignored"]
async fn real_filesystem_server_list_and_call() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "hi from ignis\n").unwrap();
    let mut servers = HashMap::new();
    servers.insert(
        "fs".to_string(),
        McpServerConfig {
            command: Some("npx".to_string()),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
                tmp.path().to_string_lossy().to_string(),
            ],
            startup_timeout_secs: 60, // npx download can be slow
            tool_timeout_secs: 30,
            ..Default::default()
        },
    );
    let reg = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    assert!(matches!(
        reg.entries()[0].status,
        McpStatus::Connected { .. }
    ));
    let read = reg
        .wrappers()
        .into_iter()
        .find(|w| w.qualified_name() == "mcp__fs__read_text_file")
        .expect("real fs server should expose read_text_file");
    let path = tmp.path().join("hello.txt").to_string_lossy().to_string();
    let res = ignis::AgentTool::call(read.as_ref(), serde_json::json!({ "path": path })).await;
    assert!(
        !res.is_error,
        "real read_text_file returned error: {}",
        res.content
    );
    assert!(
        res.content.contains("hi from ignis"),
        "got: {}",
        res.content
    );
    reg.shutdown().await;
}
