//! The `todo_write` tool: an agent-maintained task list for multi-step work.
//!
//! The model sends the **complete** list on every call (full replace). The list
//! lives in a shared [`TodoStore`] owned by the [`crate::Session`] (so it can be
//! persisted with the session and reloaded on resume) and is surfaced to the
//! frontend via [`crate::AgentEvent::Todos`] — the tool result itself is just a
//! short ack, to keep the verbose list out of the model's history.

use crate::tools::tool::{ExecutionMode, StaticTool, ToolOutcome, ToolParam};
use crate::AgentEvent;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// One task in the agent's working list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    /// Imperative description of the task ("Add the JSON parser").
    pub content: String,
    pub status: TodoStatus,
    /// Present-continuous label shown while the task is in progress ("Adding the
    /// JSON parser"). Optional; the UI falls back to `content` when absent.
    #[serde(
        rename = "activeForm",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// The session's task list: shared between the owning [`crate::Session`] (which
/// persists it) and the `todo_write` tool (which replaces it). A plain
/// `std::sync::Mutex` — the lock is only ever held for a vector swap/clone, never
/// across an `.await`.
pub type TodoStore = Arc<Mutex<Vec<Todo>>>;

/// Build a fresh, empty store.
pub fn new_store() -> TodoStore {
    Arc::new(Mutex::new(Vec::new()))
}

pub struct TodoWriteTool {
    store: TodoStore,
    /// The session's long-lived event channel (cloned at session build). When
    /// present, a write emits [`AgentEvent::Todos`] so the frontend re-renders.
    /// `None` in headless contexts (one-shot CLI, tests) — the write still
    /// updates the store and returns its ack, it just isn't surfaced.
    events: Option<mpsc::Sender<AgentEvent>>,
}

impl TodoWriteTool {
    pub fn new(store: TodoStore, events: Option<mpsc::Sender<AgentEvent>>) -> Self {
        Self { store, events }
    }
}

#[async_trait]
impl StaticTool for TodoWriteTool {
    const NAME: &'static str = "todo_write";
    const DESCRIPTION: &'static str = "Record or update your task list for the current multi-step task. \
Send the COMPLETE list every time — it replaces the previous list, it is not a delta. \
Each item is an object: `content` (imperative task description), `status` (one of \"pending\", \"in_progress\", \"completed\"), and optional `activeForm` (a present-continuous label shown while the task is in progress, e.g. \"Running the tests\"). \
Keep exactly one task `in_progress` at a time and mark tasks `completed` as soon as they are done. \
Use this for tasks with several distinct steps so the user can see your plan and progress; skip it for trivial single-step work.";
    const PARAMETERS: &'static [ToolParam] = &[ToolParam {
        name: "todos",
        ty: "array",
        description: "The complete task list. Each element is an object with `content` (string), `status` (\"pending\" | \"in_progress\" | \"completed\"), and optional `activeForm` (string).",
    }];
    const REQUIRED: &'static [&'static str] = &["todos"];
    // Writes the shared store; Sequential keeps it ordered relative to any other
    // state-touching tool in the same batch.
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let raw = args
            .get("todos")
            .ok_or_else(|| "Missing required parameter: todos".to_string())?;
        let todos: Vec<Todo> = serde_json::from_value(raw.clone()).map_err(|e| {
            format!(
                "Invalid `todos`: {e}. Each item needs `content` (string) and \
                 `status` (one of \"pending\", \"in_progress\", \"completed\")."
            )
        })?;
        let n = todos.len();

        // Replace the shared list. Lock held only for the swap — no await inside.
        {
            let mut guard = self.store.lock().unwrap();
            *guard = todos.clone();
        }

        // Surface the new list to the frontend (no-op when headless).
        if let Some(tx) = &self.events {
            let _ = tx.send(AgentEvent::Todos { items: todos }).await;
        }

        Ok(format!(
            "Updated todo list ({n} item{}).",
            if n == 1 { "" } else { "s" }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tool::AgentTool;
    use serde_json::json;

    fn item(content: &str, status: &str) -> serde_json::Value {
        json!({ "content": content, "status": status })
    }

    #[tokio::test]
    async fn replaces_the_store_and_acks() {
        let store = new_store();
        let tool = TodoWriteTool::new(store.clone(), None);
        let res = tool
            .call(json!({ "todos": [item("a", "in_progress"), item("b", "pending")] }))
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "Updated todo list (2 items).");
        let got = store.lock().unwrap().clone();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].status, TodoStatus::InProgress);
        assert_eq!(got[1].content, "b");

        // A second call fully replaces (not appends).
        let res = tool
            .call(json!({ "todos": [item("c", "completed")] }))
            .await;
        assert!(!res.is_error);
        assert_eq!(res.content, "Updated todo list (1 item).");
        assert_eq!(store.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_bad_status() {
        let tool = TodoWriteTool::new(new_store(), None);
        let res = tool.call(json!({ "todos": [item("a", "nope")] })).await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid `todos`"));
    }

    #[tokio::test]
    async fn emits_event_when_wired() {
        let (tx, mut rx) = mpsc::channel(4);
        let tool = TodoWriteTool::new(new_store(), Some(tx));
        let _ = tool
            .call(json!({ "todos": [json!({ "content": "x", "status": "pending", "activeForm": "Doing x" })] }))
            .await;
        match rx.recv().await {
            Some(AgentEvent::Todos { items }) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].active_form.as_deref(), Some("Doing x"));
            }
            other => panic!("expected Todos event, got {other:?}"),
        }
    }

    #[test]
    fn activeform_serializes_camelcase() {
        let t = Todo {
            content: "x".into(),
            status: TodoStatus::Pending,
            active_form: Some("Doing x".into()),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["activeForm"], "Doing x");
        assert_eq!(v["status"], "pending");
    }
}
