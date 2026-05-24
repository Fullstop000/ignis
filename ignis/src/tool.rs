use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Per-tool execution mode. If ANY tool in a batch is Sequential,
/// the entire batch runs sequentially.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExecutionMode {
    Parallel,
    Sequential,
}

/// Structured tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: String) -> Self {
        Self {
            content,
            is_error: false,
        }
    }
    pub fn error(content: String) -> Self {
        Self {
            content,
            is_error: true,
        }
    }
}

/// Converts Result<T, E> into ToolResult for the #[tool] macro.
pub trait IntoToolResult {
    fn into_tool_result(self) -> ToolResult;
}

impl<T, E> IntoToolResult for Result<T, E>
where
    T: std::fmt::Display,
    E: std::fmt::Display,
{
    fn into_tool_result(self) -> ToolResult {
        match self {
            Ok(val) => ToolResult::ok(val.to_string()),
            Err(err) => ToolResult::error(err.to_string()),
        }
    }
}

#[async_trait]
pub trait AgentTool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    async fn call(&self, args: serde_json::Value) -> ToolResult;
}

/// Optional hooks for tool call lifecycle.
#[async_trait]
pub trait ToolHooks: Send + Sync + 'static {
    /// Called before tool execution. Return Err(reason) to block the call.
    async fn before_tool_call(
        &self,
        _tool_name: &str,
        _args: &serde_json::Value,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Called after tool execution. Can transform the result.
    async fn after_tool_call(&self, _tool_name: &str, result: ToolResult) -> ToolResult {
        result
    }
}
