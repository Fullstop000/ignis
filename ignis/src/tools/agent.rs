use crate::agent::Agent;
use crate::config::Config;
use crate::mcp::McpRegistry;
use crate::{AgentTool, Message, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SUBAGENT_SYSTEM_PROMPT: &str =
    "You are a focused sub-agent spawned to complete one self-contained task. \
     Use the available tools (read_file, grep, glob, list_dir, bash, web_search, web_fetch) \
     to investigate and act, then reply with a single concise, complete answer the calling \
     agent can use directly. Do not ask follow-up questions.";

/// Delegate a self-contained task to a fresh sub-agent that has the file,
/// search, and web tools (but not this one — sub-agents do not nest). Runs to
/// completion and returns the sub-agent's final answer.
pub struct SubagentTool {
    config: Config,
    cwd: PathBuf,
    /// Shared with the parent — subagents see the same connected MCP servers
    /// and their tools (as `mcp__<server>__<tool>`).
    mcp: Option<Arc<McpRegistry>>,
}

impl SubagentTool {
    pub fn new(config: Config, cwd: &Path) -> Self {
        Self {
            config,
            cwd: cwd.to_path_buf(),
            mcp: None,
        }
    }

    pub fn with_mcp(mut self, mcp: Arc<McpRegistry>) -> Self {
        self.mcp = Some(mcp);
        self
    }
}

#[async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "Delegate a focused, self-contained task to a sub-agent that has the file/search/web \
         tools. Returns its final answer. Use for multi-step research or lookups to keep the \
         main thread uncluttered. The sub-agent cannot spawn further sub-agents."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The task for the sub-agent, self-contained with all needed context" },
                "description": { "type": "string", "description": "Optional short label for the task" }
            },
            "required": ["prompt"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let Some(prompt) = args["prompt"].as_str() else {
            return ToolResult::error("Missing required parameter: prompt".to_string());
        };

        let provider = match crate::config::build_provider(&self.config) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Could not build provider: {e}")),
        };
        let mut agent = Agent::new(SUBAGENT_SYSTEM_PROMPT.to_string(), provider);
        // Same toolset as the main agent, minus `agent` itself — no recursion.
        for tool in super::native_tools(&self.cwd, self.config.web_search.clone()) {
            agent.register_tool(tool);
        }
        // Inherit MCP tools and their server-instructions from the parent.
        if let Some(mcp) = &self.mcp {
            for wrapper in mcp.wrappers() {
                agent.register_tool(wrapper as Arc<dyn AgentTool>);
            }
            agent.set_mcp(mcp.clone());
        }

        let mut history = vec![Message {
            role: "user".to_string(),
            content: Some(prompt.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }];

        // Drain the event stream concurrently so `run` never blocks on a full
        // channel; the sub-agent's events aren't surfaced in the UI (yet).
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let run = agent.run(&mut history, tx, None, None, None).await;
        let _ = drain.await;

        if let Err(e) = run {
            return ToolResult::error(format!("Sub-agent failed: {e}"));
        }

        let answer = history
            .iter()
            .rev()
            .find(|m| {
                m.role == "assistant"
                    && m.content
                        .as_deref()
                        .map(|c| !c.trim().is_empty())
                        .unwrap_or(false)
            })
            .and_then(|m| m.content.clone());

        match answer {
            Some(a) => ToolResult::ok(a),
            None => ToolResult::ok("(sub-agent produced no text answer)".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subagent_requires_prompt() {
        let tool = SubagentTool::new(Config::default(), Path::new("."));
        let res = tool.call(json!({})).await;
        assert!(res.is_error);
    }
}
