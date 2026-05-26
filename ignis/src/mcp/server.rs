//! `McpServer`: one live connection to an MCP stdio server. Wraps the rmcp
//! peer, the cached tool list captured at connect time, and the optional
//! `instructions` field the server returned in its `initialize` response.
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::{RoleClient, RunningService};

use crate::tools::tool::ToolResult;

/// A connected MCP server. Dropping this struct also drops the underlying
/// `RunningService`, which terminates the child process (rmcp's
/// `TokioChildProcess` has `kill_on_drop` semantics).
pub struct McpServer {
    /// User-facing name (the config key, e.g. `"github"`).
    name: String,
    /// Live rmcp service. Deref'd to `Peer<RoleClient>` for `call_tool` etc.
    service: RunningService<RoleClient, ()>,
    /// Tools advertised at connect time. We don't refresh on
    /// `notifications/tools/list_changed` in v1.
    tools: Vec<Tool>,
    /// `instructions` field from the server's `InitializeResult`; may be empty.
    instructions: Option<String>,
    /// Per-call ceiling for `tools/call`.
    tool_timeout: std::time::Duration,
}

impl McpServer {
    pub fn new(
        name: String,
        service: RunningService<RoleClient, ()>,
        tools: Vec<Tool>,
        instructions: Option<String>,
        tool_timeout: std::time::Duration,
    ) -> Self {
        Self {
            name,
            service,
            tools,
            instructions,
            tool_timeout,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// Invoke an MCP tool. Maps timeouts, transport errors, and server-side
    /// `is_error: true` results into `ToolResult::error` so the agent treats
    /// them the same as any other tool failure.
    pub async fn call_tool(self: &Arc<Self>, params: CallToolRequestParams) -> ToolResult {
        let tool_name = params.name.to_string();
        let fut = self.service.call_tool(params);
        let outcome = tokio::time::timeout(self.tool_timeout, fut).await;
        match outcome {
            Err(_) => ToolResult::error(format!(
                "mcp__{}__{} timed out after {}s",
                self.name,
                tool_name,
                self.tool_timeout.as_secs()
            )),
            Ok(Err(err)) => ToolResult::error(format!(
                "MCP server `{}` failed to run `{}`: {err}",
                self.name, tool_name
            )),
            Ok(Ok(result)) => {
                let text = flatten_content(&result.content);
                if result.is_error == Some(true) {
                    ToolResult::error(text)
                } else {
                    ToolResult::ok(text)
                }
            }
        }
    }

    /// Cancel the underlying rmcp service. Consumes `self`. Drop alone would
    /// also kill the child, but cancelling first lets rmcp send a clean
    /// `notifications/cancelled` and wait briefly for the child to exit.
    pub async fn shutdown(self) {
        let _ = self.service.cancel().await;
    }
}

/// Concatenate every `text` content block (newline-separated) into one string.
/// Non-text content (images, audio, embedded resources, resource links) is
/// summarised as a placeholder line so we don't silently drop it.
fn flatten_content(content: &[rmcp::model::Content]) -> String {
    use rmcp::model::RawContent;
    let mut parts: Vec<String> = Vec::with_capacity(content.len());
    for block in content {
        match &block.raw {
            RawContent::Text(t) => parts.push(t.text.clone()),
            RawContent::Image(img) => {
                parts.push(format!(
                    "[image: {} bytes, {}]",
                    img.data.len(),
                    img.mime_type
                ));
            }
            RawContent::Audio(a) => {
                parts.push(format!("[audio: {} bytes, {}]", a.data.len(), a.mime_type));
            }
            RawContent::Resource(r) => {
                parts.push(format!("[embedded resource: {:?}]", r.resource));
            }
            RawContent::ResourceLink(r) => {
                parts.push(format!("[resource link: {}]", r.uri));
            }
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Annotated, RawContent, RawTextContent};

    fn text(s: &str) -> rmcp::model::Content {
        Annotated::new(
            RawContent::Text(RawTextContent {
                text: s.to_string(),
                meta: None,
            }),
            None,
        )
    }

    #[test]
    fn flatten_concatenates_text_blocks_newline_separated() {
        let out = flatten_content(&[text("alpha"), text("beta"), text("gamma")]);
        assert_eq!(out, "alpha\nbeta\ngamma");
    }

    #[test]
    fn flatten_empty_returns_empty_string() {
        assert_eq!(flatten_content(&[]), "");
    }
}
