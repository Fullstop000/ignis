use async_trait::async_trait;
use futures_util::stream::{self, BoxStream, StreamExt};
use ignis::{
    config::CompactionConfig,
    provider::{LlmProvider, LlmResponseDelta},
    storage::{InMemoryStorage, SessionStorage},
    tool::{AgentTool, ToolResult},
    types::{AgentEvent, Message},
    Session,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// ==========================================
// Mock Provider
// ==========================================

struct MockProvider {
    responses: Vec<Vec<LlmResponseDelta>>,
    index: AtomicUsize,
}

impl MockProvider {
    fn new(responses: Vec<Vec<LlmResponseDelta>>) -> Self {
        Self {
            responses,
            index: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn chat_stream(
        &self,
        _system_prompt: &str,
        _messages: &[Message],
        _tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
        let idx = self.index.fetch_add(1, Ordering::SeqCst);
        let deltas = self.responses.get(idx).cloned().unwrap_or_default();
        Ok(stream::iter(deltas.into_iter().map(Ok)).boxed())
    }
}

// ==========================================
// Mock Tool
// ==========================================

struct EchoTool;

#[async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo back the input message."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "Message to echo" }
            },
            "required": ["message"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let msg = args["message"].as_str().unwrap_or("?");
        ToolResult::ok(msg.to_string())
    }
}

struct FailTool;

#[async_trait]
impl AgentTool for FailTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn call(&self, _args: serde_json::Value) -> ToolResult {
        ToolResult::error("intentional failure".to_string())
    }
}

// ==========================================
// Helpers
// ==========================================

async fn collect_events(rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    events
}

fn find_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageUpdate { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

// ==========================================
// Tests
// ==========================================

#[tokio::test]
async fn agent_single_turn_no_tools() {
    let provider = MockProvider::new(vec![vec![LlmResponseDelta::Text(
        "Hello world".to_string(),
    )]]);
    let storage = InMemoryStorage::new();
    let mut session = Session::open(
        "test".to_string(),
        "system".to_string(),
        Box::new(provider),
        Box::new(storage),
        "/tmp".to_string(),
    )
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    session.prompt("Hi", tx).await.unwrap();

    let events = collect_events(&mut rx).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentStart)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentEnd)));
    assert_eq!(find_text(&events), "Hello world");
}

#[tokio::test]
async fn agent_executes_tool_and_continues() {
    let provider = MockProvider::new(vec![
        // First turn: LLM requests a tool call
        vec![LlmResponseDelta::ToolCall {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("echo".to_string()),
            arguments: r#"{"message":"hello"}"#.to_string(),
        }],
        // Second turn: LLM responds with final text
        vec![LlmResponseDelta::Text("Done".to_string())],
    ]);
    let storage = InMemoryStorage::new();
    let mut session = Session::open(
        "test".to_string(),
        "system".to_string(),
        Box::new(provider),
        Box::new(storage),
        "/tmp".to_string(),
    )
    .await
    .unwrap();
    session.register_tool(Arc::new(EchoTool));

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    session.prompt("Call echo", tx).await.unwrap();

    let events = collect_events(&mut rx).await;

    // Should see tool execution lifecycle
    let tool_starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }))
        .collect();
    assert_eq!(tool_starts.len(), 1, "expected exactly one tool start");

    let tool_ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .collect();
    assert_eq!(tool_ends.len(), 1, "expected exactly one tool end");

    // Tool result should contain the echoed message
    let tool_result = tool_ends
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolExecutionEnd { result, .. } => Some(result.content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(tool_result.contains("hello"));

    // Final LLM text
    assert_eq!(find_text(&events), "Done");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentEnd)));
}

#[tokio::test]
async fn agent_handles_tool_error_gracefully() {
    let provider = MockProvider::new(vec![
        vec![LlmResponseDelta::ToolCall {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("fail".to_string()),
            arguments: "{}".to_string(),
        }],
        vec![LlmResponseDelta::Text("Recovered".to_string())],
    ]);
    let storage = InMemoryStorage::new();
    let mut session = Session::open(
        "test".to_string(),
        "system".to_string(),
        Box::new(provider),
        Box::new(storage),
        "/tmp".to_string(),
    )
    .await
    .unwrap();
    session.register_tool(Arc::new(FailTool));

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    session.prompt("Trigger failure", tx).await.unwrap();

    let events = collect_events(&mut rx).await;

    let tool_ends: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .collect();
    assert_eq!(tool_ends.len(), 1);

    let is_error = tool_ends.iter().any(|e| match e {
        AgentEvent::ToolExecutionEnd { result, .. } => result.is_error,
        _ => false,
    });
    assert!(is_error, "tool should report error");

    assert_eq!(find_text(&events), "Recovered");
}

#[tokio::test]
async fn agent_persists_messages_to_storage() {
    let provider = MockProvider::new(vec![vec![LlmResponseDelta::Text("Reply".to_string())]]);
    let storage = InMemoryStorage::new();
    let mut session = Session::open(
        "persist-test".to_string(),
        "system".to_string(),
        Box::new(provider),
        Box::new(storage.clone()),
        "/tmp".to_string(),
    )
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    session.prompt("Hello", tx).await.unwrap();
    let _ = collect_events(&mut rx).await;

    let history = storage.load_session("persist-test").await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, "user");
    assert_eq!(history[0].content.as_deref(), Some("Hello"));
    assert_eq!(history[1].role, "assistant");
    assert_eq!(history[1].content.as_deref(), Some("Reply"));
}

#[tokio::test]
async fn agent_streaming_multiple_deltas() {
    let provider = MockProvider::new(vec![vec![
        LlmResponseDelta::Text("Hello ".to_string()),
        LlmResponseDelta::Text("world".to_string()),
        LlmResponseDelta::Text("!".to_string()),
    ]]);
    let storage = InMemoryStorage::new();
    let mut session = Session::open(
        "stream-test".to_string(),
        "system".to_string(),
        Box::new(provider),
        Box::new(storage),
        "/tmp".to_string(),
    )
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    session.prompt("Stream", tx).await.unwrap();

    let events = collect_events(&mut rx).await;
    let updates: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageUpdate { delta } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(updates, vec!["Hello ", "world", "!"]);
}

#[tokio::test]
async fn agent_reasoning_content_is_preserved() {
    let provider = MockProvider::new(vec![vec![
        LlmResponseDelta::Reasoning("thinking...".to_string()),
        LlmResponseDelta::Text("Answer".to_string()),
    ]]);
    let storage = InMemoryStorage::new();
    let mut session = Session::open(
        "reasoning-test".to_string(),
        "system".to_string(),
        Box::new(provider),
        Box::new(storage.clone()),
        "/tmp".to_string(),
    )
    .await
    .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    session.prompt("Think", tx).await.unwrap();
    let _ = collect_events(&mut rx).await;

    let history = storage.load_session("reasoning-test").await.unwrap();
    let assistant_msg = history.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(
        assistant_msg.reasoning_content.as_deref(),
        Some("thinking...")
    );
    assert_eq!(assistant_msg.content.as_deref(), Some("Answer"));
}

#[tokio::test]
async fn session_compact_summarizes_old_history() {
    // Seed a 10-message conversation with uniform, sizable content so a small
    // keep-budget reliably forces compaction.
    let storage = InMemoryStorage::new();
    let mut seed = Vec::new();
    for _ in 0..5 {
        for role in ["user", "assistant"] {
            seed.push(Message {
                role: role.to_string(),
                content: Some("x".repeat(400)), // ~101 estimated tokens each
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }
    }
    storage
        .save_session("c", &seed, Some("/tmp"))
        .await
        .unwrap();

    // The compaction completion returns the <analysis>/<summary> framing the
    // prompt asks for; only the <summary> body should be kept.
    let provider = MockProvider::new(vec![vec![LlmResponseDelta::Text(
        "<analysis>thinking</analysis><summary>SUMMARY</summary>".to_string(),
    )]]);
    let mut session = Session::open(
        "c".to_string(),
        "system".to_string(),
        Box::new(provider),
        Box::new(storage.clone()),
        "/tmp".to_string(),
    )
    .await
    .unwrap();
    session.set_compaction(CompactionConfig {
        auto: false,
        threshold_tokens: usize::MAX,
        keep_recent_tokens: 350,
    });
    assert_eq!(session.history().len(), 10);

    let removed = session.compact().await.unwrap();
    assert!(removed > 0, "expected the older head to be compacted");

    let h = session.history();
    assert!(h.len() < 10, "history should shrink");
    assert_eq!(h[0].role, "user");
    let summary = h[0].content.as_deref().unwrap();
    assert!(summary.contains("SUMMARY"));
    assert!(!summary.contains("thinking")); // <analysis> scratchpad dropped
                                            // The kept tail must start at a user turn boundary, so no tool result is
                                            // ever orphaned from the assistant message that requested it.
    assert_eq!(h[1].role, "user");

    // Compaction is persisted.
    let persisted = storage.load_session("c").await.unwrap();
    assert_eq!(persisted.len(), h.len());
}
