use crate::agent::Agent;
use crate::config::Config;
use crate::mcp::McpRegistry;
use crate::tools::cwd::SessionCwd;
use crate::{AgentTool, Message, StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
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
    /// Default model tier when the `tier` arg is omitted, so delegation is
    /// model-efficient by default: cheap models map/search, strong models
    /// review. `None` = inherit the session model (the `general` catch-all).
    default_tier: Option<&'static str>,
}

/// The built-in sub-agent types. `general` reproduces today's behavior and is
/// the default when `agent_type` is absent.
const AGENT_TYPES: &[AgentTypeSpec] = &[
    AgentTypeSpec {
        name: "general",
        system_prompt: SUBAGENT_SYSTEM_PROMPT,
        read_only: false,
        // Catch-all: could be anything, so don't presume a tier.
        default_tier: None,
    },
    AgentTypeSpec {
        name: "explore",
        system_prompt: EXPLORE_SYSTEM_PROMPT,
        read_only: true,
        // Bulk reading/mapping — the high-volume work where a cheaper model
        // pays off; `medium` keeps enough comprehension.
        default_tier: Some("medium"),
    },
    AgentTypeSpec {
        name: "review",
        system_prompt: REVIEW_SYSTEM_PROMPT,
        read_only: true,
        // Bug/risk hunting rewards the strongest reasoning available.
        default_tier: Some("high"),
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

/// The tier to run a sub-agent at: an explicit `tier` argument wins; otherwise
/// the agent type's default (so `explore`/`review` are model-efficient without
/// the caller choosing). `None` → inherit the session model.
fn chosen_tier<'a>(arg: Option<&'a str>, spec: &'a AgentTypeSpec) -> Option<&'a str> {
    arg.or(spec.default_tier)
}

/// A one-line activity + cost summary appended to a sub-agent's answer, so the
/// delegation isn't a black box — e.g.
/// `— agent[explore·medium] · deepseek/deepseek-v4-flash@max · 5 tool calls (grep×2, read_file×3) · 4210+830 tok`.
fn subagent_footer(
    agent_type: &str,
    tier: Option<&str>,
    config: &Config,
    history: &[Message],
    usage: &crate::Usage,
) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    let mut total = 0usize;
    for m in history {
        for call in m.tool_calls.iter().flatten() {
            total += 1;
            *counts.entry(call.function.name.as_str()).or_default() += 1;
        }
    }
    let acts = if total == 0 {
        "no tool calls".to_string()
    } else {
        let tally = counts
            .iter()
            .map(|(n, c)| {
                if *c > 1 {
                    format!("{n}×{c}")
                } else {
                    (*n).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{total} tool calls ({tally})")
    };
    let provider = config.active_provider().unwrap_or_default();
    let model = config.active_model().unwrap_or_default();
    let effort = config
        .active_effort()
        .map(|e| format!("@{e}"))
        .unwrap_or_default();
    let tier_lbl = tier.map(|t| format!("·{t}")).unwrap_or_default();
    format!(
        "\n\n— agent[{agent_type}{tier_lbl}] · {provider}/{model}{effort} · {acts} · {}+{} tok",
        usage.input_tokens, usage.output_tokens
    )
}

/// Delegate a self-contained task to a fresh sub-agent that has the file,
/// search, and web tools (but not this one — sub-agents do not nest). Runs to
/// completion and returns the sub-agent's final answer.
pub struct SubagentTool {
    config: Config,
    /// Shared with the parent, so a sub-agent spawned after `enter_worktree`
    /// operates inside the active worktree (it can't switch cwd itself — the
    /// worktree tools are top-level only).
    cwd: SessionCwd,
    /// Shared with the parent — subagents see the same connected MCP servers
    /// and their tools (as `mcp__<server>__<tool>`).
    mcp: Option<Arc<McpRegistry>>,
}

impl SubagentTool {
    pub fn new(config: Config, cwd: impl Into<SessionCwd>) -> Self {
        Self {
            config,
            cwd: cwd.into(),
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
         Optionally set `tier` to match the model to the subtask's difficulty (`low` for \
         mechanical/lookup work, `medium` for normal coding, `high` for hard reasoning) — \
         cheaper models for easy work, stronger ones only when the task needs it. \
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
        ToolParam {
            name: "tier",
            ty: "string",
            description: "Optional model tier matched to task difficulty: \"low\" \
                          (mechanical/lookup), \"medium\" (normal coding/reasoning), or \"high\" \
                          (hard reasoning — architecture, tricky debugging, math). Omit to use \
                          the agent type's default (`explore`→medium, `review`→high; `general` \
                          keeps the current model).",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["prompt"];

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let prompt = args.require_str("prompt")?;
        let spec = resolve_agent_type(args.get("agent_type").and_then(|v| v.as_str()))?;

        // Tier routing: an explicit `tier` arg wins, else the agent type's
        // default (so `explore`/`review` are model-efficient by default). A
        // resolved tier picks the cheapest adequate model (+ scaled effort) of
        // the active provider; unknown/unmatched/None → inherit the session model.
        let tier = chosen_tier(args.get("tier").and_then(|v| v.as_str()), spec);
        let (config, routed) = match tier {
            Some(t) => self.config.with_tier(t),
            None => (self.config.clone(), false),
        };
        // Only label the footer with the tier when it actually routed; an
        // unresolved tier runs on the session model, so claiming the tier would
        // mislabel which model ran.
        let footer_tier = if routed { tier } else { None };
        let provider = crate::config::build_provider(&config)
            .map_err(|e| format!("Could not build provider: {e}"))?;
        let mut agent = Agent::new(spec.system_prompt.to_string(), provider);
        // The type's toolset — read-only types get reads + search only; `general`
        // gets the full native set. Minus `agent` itself either way (no nesting).
        // No background context and no bash sandbox: sub-agents get plain
        // blocking bash only (the top-level loop is the gate).
        let tools = if spec.read_only {
            super::read_only_tools(self.cwd.clone())
        } else {
            super::native_tools(self.cwd.clone(), self.config.web_search.clone(), None, None)
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
        let usage = outcome.map_err(|e| format!("Sub-agent failed: {e}"))?;

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
            .and_then(|m| m.content.clone())
            .unwrap_or_else(|| "(sub-agent produced no text answer)".to_string());

        // Surface the delegation so it isn't a black box: which model/tier ran,
        // how much work it did, and what it cost.
        let footer = subagent_footer(spec.name, footer_tier, &config, &history, &usage);
        Ok(format!("{answer}{footer}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn subagent_requires_prompt() {
        let tool = SubagentTool::new(Config::default(), std::path::Path::new("."));
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

    /// `tier` is advertised but optional — omitting it must keep today's
    /// inherit-the-session-model behavior.
    #[test]
    fn tier_param_is_advertised_and_optional() {
        assert!(
            <SubagentTool as StaticTool>::PARAMETERS
                .iter()
                .any(|p| p.name == "tier"),
            "tier is advertised as a parameter"
        );
        assert!(
            !<SubagentTool as StaticTool>::REQUIRED.contains(&"tier"),
            "tier is optional"
        );
    }

    /// The read-only toolset excludes execution/write/network tools; `general`
    /// includes them. Pins the toolset-by-type contract.
    #[test]
    fn read_only_toolset_excludes_bash_but_general_includes_it() {
        let cwd = std::path::Path::new(".");
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

    #[test]
    fn agent_types_carry_default_tiers() {
        assert_eq!(resolve_agent_type(None).unwrap().default_tier, None);
        assert_eq!(
            resolve_agent_type(Some("explore")).unwrap().default_tier,
            Some("medium")
        );
        assert_eq!(
            resolve_agent_type(Some("review")).unwrap().default_tier,
            Some("high")
        );
    }

    #[test]
    fn explicit_tier_overrides_agent_type_default() {
        let review = resolve_agent_type(Some("review")).unwrap();
        assert_eq!(chosen_tier(Some("low"), review), Some("low")); // explicit wins
        assert_eq!(chosen_tier(None, review), Some("high")); // falls to default
        let general = resolve_agent_type(None).unwrap();
        assert_eq!(chosen_tier(None, general), None); // general inherits
    }

    #[test]
    fn subagent_footer_reports_model_tier_activity_and_cost() {
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
reasoning_effort = "max"
[providers.deepseek]
api_key = "x"
"#,
        )
        .unwrap();
        let tool_call = |name: &str, id: &str| crate::ToolCall {
            id: id.to_string(),
            r#type: "function".to_string(),
            function: crate::ToolCallFunction {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        };
        let history = vec![Message {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![
                tool_call("grep", "1"),
                tool_call("grep", "2"),
                tool_call("read_file", "3"),
            ]),
            created_at_ms: None,
        }];
        let usage = crate::Usage {
            input_tokens: 4210,
            output_tokens: 830,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let footer = subagent_footer("explore", Some("medium"), &cfg, &history, &usage);
        assert!(footer.contains("agent[explore·medium]"), "{footer}");
        assert!(
            footer.contains("deepseek/deepseek-v4-flash@max"),
            "{footer}"
        );
        assert!(footer.contains("3 tool calls"), "{footer}");
        assert!(footer.contains("grep×2"), "{footer}");
        assert!(footer.contains("read_file"), "{footer}");
        assert!(footer.contains("4210+830 tok"), "{footer}");
    }
}
