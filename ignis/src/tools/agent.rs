use crate::agent::Agent;
use crate::config::Config;
use crate::mcp::McpRegistry;
use crate::{AgentTool, Message, StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SUBAGENT_SYSTEM_PROMPT: &str =
    "You are a focused sub-agent spawned to complete one self-contained task. \
     Use the available tools (read_file, grep, glob, list_dir, bash, web_search, web_fetch) \
     to investigate and act, then reply with a single concise, complete answer the calling \
     agent can use directly. Do not ask follow-up questions.";

const EXPLORE_SYSTEM_PROMPT: &str =
    "You are an exploration sub-agent. Locate and map the code or information the calling agent \
     needs. Report findings concretely with `file:line` references and brief explanations. You \
     are READ-ONLY — do not edit files or run commands. End with a concise, structured summary \
     the calling agent can act on. Do not ask follow-up questions.";

const REVIEW_SYSTEM_PROMPT: &str =
    "You are a code-review sub-agent. Critically review the given scope for bugs, regressions, \
     and risks. Report each finding as `file:line` + severity + a one-line failure scenario. You \
     are READ-ONLY — do not edit files or run commands. Be concise and skip style nits. Do not \
     ask follow-up questions.";

/// A built-in sub-agent type: a fixed `{system prompt, toolset}` pairing the
/// `agent` tool selects via its `agent_type` parameter.
#[derive(Debug)]
struct AgentTypeSpec {
    name: &'static str,
    system_prompt: &'static str,
    /// `true` → the read-only toolset (file reads + search); `false` → the full
    /// native toolset (today's default behavior).
    read_only: bool,
}

/// The built-in sub-agent types. `general` reproduces today's behavior and is
/// the default when `agent_type` is absent.
const AGENT_TYPES: &[AgentTypeSpec] = &[
    AgentTypeSpec {
        name: "general",
        system_prompt: SUBAGENT_SYSTEM_PROMPT,
        read_only: false,
    },
    AgentTypeSpec {
        name: "explore",
        system_prompt: EXPLORE_SYSTEM_PROMPT,
        read_only: true,
    },
    AgentTypeSpec {
        name: "review",
        system_prompt: REVIEW_SYSTEM_PROMPT,
        read_only: true,
    },
];

/// Resolve an `agent_type` value (or `None` → `general`) to its spec, or an
/// error listing the valid types.
fn resolve_agent_type(name: Option<&str>) -> Result<&'static AgentTypeSpec, String> {
    let name = name.unwrap_or("general");
    AGENT_TYPES.iter().find(|t| t.name == name).ok_or_else(|| {
        let valid = AGENT_TYPES
            .iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
            .join(", ");
        format!("Unknown agent_type '{name}'. Valid types: {valid}.")
    })
}

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
impl StaticTool for SubagentTool {
    const NAME: &'static str = "agent";
    const DESCRIPTION: &'static str =
        "Delegate a focused, self-contained task to a sub-agent. Returns its final answer. Use \
         for multi-step research or lookups to keep the main thread uncluttered. Pick an \
         `agent_type`: `general` (default, full toolset), `explore` (read-only; locate/map code, \
         report file:line findings), or `review` (read-only; critique a scope for bugs/risks). \
         For independent subtasks, issue SEVERAL `agent` calls in one turn — they run in \
         parallel. The sub-agent cannot spawn further sub-agents.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "prompt",
            ty: "string",
            description: "The task for the sub-agent, self-contained with all needed context",
        },
        ToolParam {
            name: "agent_type",
            ty: "string",
            description: "One of \"general\" (default), \"explore\" (read-only research), or \
                          \"review\" (read-only critical review).",
        },
        ToolParam {
            name: "description",
            ty: "string",
            description: "Optional short label for the task",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["prompt"];

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let prompt = args.require_str("prompt")?;
        let spec = resolve_agent_type(args.get("agent_type").and_then(|v| v.as_str()))?;

        let provider = crate::config::build_provider(&self.config)
            .map_err(|e| format!("Could not build provider: {e}"))?;
        let mut agent = Agent::new(spec.system_prompt.to_string(), provider);
        // The type's toolset — read-only types get reads + search only; `general`
        // gets the full native set. Minus `agent` itself either way (no nesting).
        // No background context and no bash sandbox: sub-agents get plain
        // blocking bash only (the top-level loop is the gate).
        let tools = if spec.read_only {
            super::read_only_tools(&self.cwd)
        } else {
            super::native_tools(&self.cwd, self.config.web_search.clone(), None, None)
        };
        for tool in tools {
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
        let outcome = agent.run(&mut history, tx, None, None, None).await;
        let _ = drain.await;
        outcome.map_err(|e| format!("Sub-agent failed: {e}"))?;

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

        Ok(answer.unwrap_or_else(|| "(sub-agent produced no text answer)".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn subagent_requires_prompt() {
        let tool = SubagentTool::new(Config::default(), Path::new("."));
        let res = tool.call(json!({})).await;
        assert!(res.is_error);
    }

    #[test]
    fn agent_type_defaults_to_general() {
        let spec = resolve_agent_type(None).unwrap();
        assert_eq!(spec.name, "general");
        assert!(!spec.read_only, "general is the full toolset");
    }

    #[test]
    fn explore_and_review_are_read_only() {
        assert!(resolve_agent_type(Some("explore")).unwrap().read_only);
        assert!(resolve_agent_type(Some("review")).unwrap().read_only);
    }

    #[test]
    fn unknown_agent_type_errors_with_valid_list() {
        let err = resolve_agent_type(Some("wizard")).unwrap_err();
        assert!(err.contains("Unknown agent_type 'wizard'"));
        assert!(err.contains("general"));
        assert!(err.contains("explore"));
        assert!(err.contains("review"));
    }

    /// The read-only toolset excludes execution/write/network tools; `general`
    /// includes them. Pins the toolset-by-type contract.
    #[test]
    fn read_only_toolset_excludes_bash_but_general_includes_it() {
        let cwd = Path::new(".");
        let names = |tools: Vec<Arc<dyn AgentTool>>| -> Vec<String> {
            tools.iter().map(|t| t.name().to_string()).collect()
        };
        let read_only = names(crate::tools::read_only_tools(cwd));
        assert!(read_only.iter().any(|n| n == "read_file"));
        assert!(read_only.iter().any(|n| n == "grep"));
        assert!(
            !read_only.iter().any(|n| n == "bash"),
            "read-only excludes bash"
        );
        assert!(
            !read_only.iter().any(|n| n == "edit_file"),
            "read-only excludes writes"
        );

        let general = names(crate::tools::native_tools(
            cwd,
            Default::default(),
            None,
            None,
        ));
        assert!(general.iter().any(|n| n == "bash"), "general includes bash");
    }
}
