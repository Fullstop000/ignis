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

/// The result of a tool's inner `run`: `Ok` carries the output text shown to
/// the model, `Err` carries an error message shown in its place. `call` bridges
/// it to [`ToolResult`] via [`IntoToolResult`] (`Ok` → success, `Err` → error).
pub type ToolOutcome = Result<String, String>;

/// Typed accessors for the JSON argument object a tool receives.
///
/// `call` returns [`ToolResult`], so a tool body can't use `?` directly. The
/// idiom is a private `run(&self, args) -> ToolOutcome` whose body
/// uses these accessors with `?`, wrapped by a one-line `call` that converts
/// through [`IntoToolResult`] — the same path the `#[tool]` macro generates.
pub trait ToolArgs {
    /// A required string argument, or a uniform "missing parameter" error.
    fn require_str(&self, key: &str) -> Result<&str, String>;
}

impl ToolArgs for serde_json::Value {
    fn require_str(&self, key: &str) -> Result<&str, String> {
        self[key]
            .as_str()
            .ok_or_else(|| format!("Missing required parameter: {key}"))
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
