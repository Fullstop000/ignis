//! `McpServer`: one live connection to an MCP stdio server. Wraps the rmcp
//! peer, the cached tool list captured at connect time, and the optional
//! `instructions` field the server returned in its `initialize` response.
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::{Peer, RoleClient, RunningService};
use tokio::sync::Mutex;

use crate::tools::tool::ToolResult;

/// A connected MCP server. `shutdown` is `&self` so the registry can drive it
/// without consuming `Arc<McpServer>` (wrappers and sub-agents hold clones).
pub struct McpServer {
    /// User-facing name (the config key, e.g. `"github"`).
    name: String,
    /// Cheap-to-clone handle for `call_tool` etc. (rmcp's `Peer` is `Clone`).
    /// Keeping this alongside the `RunningService` means tool calls don't have
    /// to lock the inner mutex on every invocation.
    peer: Peer<RoleClient>,
    /// Owned service; taken on shutdown so we can `cancel().await` (consumes).
    /// `tokio::sync::Mutex` so the take can cross an `.await`.
    inner: Mutex<Option<RunningService<RoleClient, ()>>>,
    /// PID of the immediate child, captured before the transport was given to
    /// rmcp. Used on Unix to `SIGTERM`/`SIGKILL` the whole process group so
    /// shell- or npx-wrapped servers don't leave orphans.
    child_pid: Option<u32>,
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
        child_pid: Option<u32>,
        tools: Vec<Tool>,
        instructions: Option<String>,
        tool_timeout: std::time::Duration,
    ) -> Self {
        let peer = service.peer().clone();
        Self {
            name,
            peer,
            inner: Mutex::new(Some(service)),
            child_pid,
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
    /// them the same as any other tool failure. After shutdown, returns an
    /// error.
    pub async fn call_tool(self: &Arc<Self>, params: CallToolRequestParams) -> ToolResult {
        let tool_name = params.name.to_string();
        let outcome = tokio::time::timeout(self.tool_timeout, self.peer.call_tool(params)).await;
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

    /// Tear down this server's connection. Steps in order:
    ///   1. Cancel the rmcp service (this consumes it; rmcp closes the
    ///      transport, the child sees EOF on stdin, and most servers exit).
    ///   2. On Unix, SIGTERM the child's process group to catch descendants
    ///      that wrapper commands like `npx` leave behind. SIGKILL only if the
    ///      process group is still alive after a short grace period.
    ///
    /// Safe to call multiple times — the second call is a no-op.
    pub async fn shutdown(&self) {
        if let Some(service) = self.inner.lock().await.take() {
            let _ = service.cancel().await;
        }
        #[cfg(unix)]
        if let Some(pid) = self.child_pid {
            // Negative PID = process group. The child was spawned with
            // `process_group(0)` so its PGID equals its PID. Ignoring errors:
            // ESRCH (already-dead) is the common case.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            // Give the process group a short grace period to exit; only escalate
            // to SIGKILL if it is still alive. This avoids firing SIGKILL at an
            // already-reaped child, which would fail harmlessly but log noise.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if process_group_is_alive(pid) {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
}

#[cfg(unix)]
fn process_group_is_alive(pid: u32) -> bool {
    // Signal 0 performs error checking without delivering a signal. A return
    // value of 0 means at least one process in the group exists.
    unsafe { libc::kill(-(pid as i32), 0) == 0 }
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
