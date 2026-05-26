//! `McpToolWrapper`: makes an MCP server's tool look like any other `AgentTool`.
//!
//! The wrapper owns an `Arc<McpServer>` and just forwards `call(args)` to the
//! shared rmcp client. The agent loop never has to know MCP exists.
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use serde_json::Value;

use crate::tools::tool::{AgentTool, ExecutionMode, ToolResult};

use super::server::McpServer;

/// One MCP tool, surfaced to the model as `mcp__<server>__<tool>`.
pub struct McpToolWrapper {
    /// The shared connection to the server hosting this tool.
    server: Arc<McpServer>,
    /// The name presented to the model: `mcp__<server>__<sanitized_tool>`.
    qualified_name: String,
    /// The unmodified tool name, sent over JSON-RPC to the server.
    real_name: String,
    /// Optional, may be empty.
    description: String,
    /// JSON Schema for the arguments object (passed to the provider as-is).
    schema: Value,
}

impl McpToolWrapper {
    /// Build a wrapper around one tool from a connected server. `server_name`
    /// is the user's config key (already validated); `tool_name` is whatever the
    /// server returned — non-`[a-zA-Z0-9_-]` characters are mapped to `_` so the
    /// qualified name stays inside the OpenAI tool-name regex.
    pub fn new(
        server: Arc<McpServer>,
        server_name: &str,
        tool_name: String,
        description: String,
        schema: Value,
    ) -> Self {
        let sanitized = sanitize_tool_name(&tool_name);
        let qualified_name = format!("mcp__{server_name}__{sanitized}");
        Self {
            server,
            qualified_name,
            real_name: tool_name,
            description,
            schema,
        }
    }

    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }
}

/// Replace any character outside `[a-zA-Z0-9_-]` with `_`. Matches the
/// convention used by OpenCode and Codex so users see the same names regardless
/// of which client they came from.
pub fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[async_trait]
impl AgentTool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        // We have no per-tool concurrency contract from MCP; default to Parallel
        // (the registry's per-server connection is single-stream JSON-RPC, but
        // rmcp serialises requests on the connection internally, so the agent
        // can fire several in parallel without ordering issues).
        ExecutionMode::Parallel
    }

    async fn call(&self, args: Value) -> ToolResult {
        let mut params = CallToolRequestParams::new(self.real_name.clone());
        match args {
            Value::Null => {}
            Value::Object(map) => {
                params = params.with_arguments(map);
            }
            other => {
                return ToolResult::error(format!(
                    "MCP tool `{}` expected an object for arguments, got: {}",
                    self.qualified_name, other
                ));
            }
        }
        self.server.call_tool(params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_invalid_chars() {
        assert_eq!(sanitize_tool_name("read_file"), "read_file");
        assert_eq!(sanitize_tool_name("read-file"), "read-file");
        assert_eq!(sanitize_tool_name("read.file"), "read_file");
        assert_eq!(sanitize_tool_name("read file"), "read_file");
        assert_eq!(sanitize_tool_name("tools/list"), "tools_list");
        assert_eq!(sanitize_tool_name("a:b@c"), "a_b_c");
    }

    #[test]
    fn sanitize_preserves_valid_names() {
        assert_eq!(sanitize_tool_name(""), "");
        assert_eq!(sanitize_tool_name("ABC_123-xyz"), "ABC_123-xyz");
    }
}
