use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use regex::Regex;

use crate::mcp::McpRegistry;
use crate::skills::SkillRegistry;

use serde::Serialize;

use crate::provider::{LlmProvider, LlmResponseDelta, Message, ToolCall, ToolCallFunction, Usage};
use crate::tools::tool::{AgentTool, ExecutionMode, ToolHooks, ToolResult};

/// Events emitted by the agent loop as it streams a turn.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum AgentEvent {
    #[serde(rename = "agent_start")]
    AgentStart,
    #[serde(rename = "turn_start")]
    TurnStart,
    #[serde(rename = "message_start")]
    MessageStart { message: Message },
    #[serde(rename = "message_update")]
    MessageUpdate { delta: String },
    #[serde(rename = "message_end")]
    MessageEnd { message: Message },
    #[serde(rename = "tool_execution_start")]
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        arguments: String,
    },
    #[serde(rename = "tool_execution_end")]
    ToolExecutionEnd {
        tool_call_id: String,
        result: ToolResult,
    },
    #[serde(rename = "turn_end")]
    TurnEnd,
    #[serde(rename = "usage")]
    Usage(Usage),
    #[serde(rename = "agent_end")]
    AgentEnd,
    #[serde(rename = "user_injected")]
    UserInjected { text: String },
}

/// Build the system prompt for an interactive/one-shot run: the static agent
/// instructions plus live environment and git context for `cwd`.
pub fn build_system_prompt(cwd: &Path) -> String {
    let git_status = get_git_status();
    let git_diff = get_git_diff();
    let current_date = get_current_date();
    let os_name = std::env::consts::OS;

    format!(
        "You are Ignis, an interactive agent that helps users with software engineering tasks. \
        Use the instructions below and the tools available to you to assist the user.

# Guidelines
 - All text you output outside of tool use is displayed to the user.
 - Tools are executed in a user-selected permission mode.
 - Read relevant code before changing it and keep changes tightly scoped to the request.
 - Do not add speculative abstractions, compatibility shims, or unrelated cleanup.
 - Do not create files unless they are required to complete the task.
 - If an approach fails, diagnose the failure before switching tactics.
 - Be careful not to introduce security vulnerabilities such as command injection, XSS, or SQL injection.
 - Report outcomes faithfully: if verification fails or was not run, say so explicitly.
 - Carefully consider reversibility and blast radius. Local, reversible actions like editing files or running tests are usually fine. Actions that affect shared systems, publish state, delete data, or otherwise have high blast radius should be explicitly authorized by the user.

# Tone & Style
 - Be concise. Start work immediately. No conversational fillers or preambles.
 - Answer directly without flattery or flippancy.
 - Don't summarize what you did unless asked. Don't explain your code unless asked.

# Environment Context
 - Operating System: {}
 - Working Directory: {}
 - Current Date/Time: {}

# Git Context
Git Status:
```
{}
```

Git Diff:
```
{}
```",
        os_name,
        cwd.display(),
        current_date,
        git_status,
        git_diff
    )
}

fn get_git_status() -> String {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "Not a git repository or git not installed".to_string())
}

fn get_git_diff() -> String {
    let output = std::process::Command::new("git")
        .args(["diff"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    if output.trim().is_empty() {
        "No changes".to_string()
    } else if output.len() > 2000 {
        format!("{}... (truncated)", &output[..2000])
    } else {
        output
    }
}

fn get_current_date() -> String {
    std::process::Command::new("date")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "Unknown Date".to_string())
}

/// Drain any pending inject messages into `history` as user turns, emitting a
/// `UserInjected` event for each. Returns how many were drained.
async fn drain_injected(
    inject_rx: Option<&mut tokio::sync::mpsc::Receiver<String>>,
    history: &mut Vec<Message>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> usize {
    let mut n = 0;
    if let Some(rx) = inject_rx {
        while let Ok(text) = rx.try_recv() {
            history.push(Message {
                role: "user".to_string(),
                content: Some(text.clone()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
            let _ = tx.send(AgentEvent::UserInjected { text }).await;
            n += 1;
        }
    }
    n
}

/// Compose the effective system prompt: base + the enabled-skills catalog +
/// the connected-MCP-servers' `instructions` block.
fn with_catalog(base: &str, skills: Option<&SkillRegistry>, mcp: Option<&McpRegistry>) -> String {
    let mut out = base.to_string();
    if let Some(catalog) = skills.and_then(|r| r.catalog_prompt()) {
        out.push_str("\n\n");
        out.push_str(&catalog);
    }
    if let Some(block) = mcp.and_then(|r| r.instructions_block()) {
        out.push_str("\n\n");
        out.push_str(&block);
    }
    out
}

/// Execution engine: given a conversation `history`, runs the model + tool
/// loop and emits events. State and persistence live in [`crate::Session`].
pub struct Agent {
    system_prompt: String,
    provider: Box<dyn LlmProvider>,
    tools: Vec<Arc<dyn AgentTool>>,
    hooks: Option<Box<dyn ToolHooks>>,
    skills: Option<Arc<SkillRegistry>>,
    mcp: Option<Arc<McpRegistry>>,
}

struct AccumulatingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl Agent {
    pub fn new(system_prompt: String, provider: Box<dyn LlmProvider>) -> Self {
        Self {
            system_prompt,
            provider,
            tools: Vec::new(),
            hooks: None,
            skills: None,
            mcp: None,
        }
    }

    pub fn register_tool(&mut self, tool: Arc<dyn AgentTool>) {
        self.tools.push(tool);
    }

    pub fn set_hooks(&mut self, hooks: Box<dyn ToolHooks>) {
        self.hooks = Some(hooks);
    }

    pub fn set_skills(&mut self, skills: Arc<SkillRegistry>) {
        self.skills = Some(skills);
    }

    pub fn set_mcp(&mut self, mcp: Arc<McpRegistry>) {
        self.mcp = Some(mcp);
    }

    /// One-shot, tool-less completion: stream a response for `messages` and
    /// return the concatenated text. Used by [`crate::Session::compact`] to
    /// summarize history.
    pub async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
    ) -> Result<String, anyhow::Error> {
        use futures_util::stream::StreamExt;
        let mut stream = self
            .provider
            .chat_stream(system_prompt, messages, &[])
            .await?;
        let mut out = String::new();
        while let Some(delta) = stream.next().await {
            if let Ok(LlmResponseDelta::Text(text)) = delta {
                out.push_str(&text);
            }
        }
        Ok(out)
    }

    fn sanitize_and_truncate_error(err: &str) -> String {
        static API_KEY_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"sk-[A-Za-z0-9_-]{20,}").unwrap());
        let redacted = API_KEY_RE.replace_all(err, "[REDACTED_API_KEY]");
        let mut end = 500.min(redacted.len());
        while end < redacted.len() && !redacted.is_char_boundary(end) {
            end -= 1;
        }
        if end < redacted.len() {
            format!("{}... [truncated]", &redacted[..end])
        } else {
            redacted.into_owned()
        }
    }

    /// Run the model + tool loop over `history`, appending assistant and tool
    /// messages in place and emitting events. Does not load or persist; the
    /// caller ([`crate::Session`]) owns history and storage.
    pub async fn run(
        &self,
        history: &mut Vec<Message>,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
        mut inject_rx: Option<&mut tokio::sync::mpsc::Receiver<String>>,
    ) -> Result<Usage, anyhow::Error> {
        let _ = tx.send(AgentEvent::AgentStart).await;
        let mut total_usage = Usage::default();
        let effective_prompt = with_catalog(
            &self.system_prompt,
            self.skills.as_deref(),
            self.mcp.as_deref(),
        );

        let tool_schemas: Vec<serde_json::Value> = self
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters(),
                    }
                })
            })
            .collect();

        loop {
            drain_injected(inject_rx.as_deref_mut(), history, &tx).await;
            let _ = tx.send(AgentEvent::TurnStart).await;

            let mut stream = match self
                .provider
                .chat_stream(&effective_prompt, &*history, &tool_schemas)
                .await
            {
                Ok(s) => s,
                Err(err) => {
                    let err_msg = Self::sanitize_and_truncate_error(&err.to_string());
                    let error_content = format!("Error: {}", err_msg);
                    let _ = tx
                        .send(AgentEvent::MessageStart {
                            message: Message {
                                role: "assistant".to_string(),
                                content: Some(String::new()),
                                reasoning_content: None,
                                name: None,
                                tool_call_id: None,
                                tool_calls: None,
                            },
                        })
                        .await;
                    let _ = tx
                        .send(AgentEvent::MessageUpdate {
                            delta: error_content.clone(),
                        })
                        .await;
                    let _ = tx
                        .send(AgentEvent::MessageEnd {
                            message: Message {
                                role: "assistant".to_string(),
                                content: Some(error_content),
                                reasoning_content: None,
                                name: None,
                                tool_call_id: None,
                                tool_calls: None,
                            },
                        })
                        .await;
                    break;
                }
            };

            let mut assistant_content = String::new();
            let mut reasoning_content = String::new();
            let mut pending_tool_calls: HashMap<usize, AccumulatingToolCall> = HashMap::new();
            let mut message_started = false;
            let mut reasoning_streaming = false;
            let mut turn_usage = Usage::default();

            use futures_util::stream::StreamExt;
            while let Some(delta_result) = stream.next().await {
                match delta_result {
                    Err(err) => {
                        let err_msg = Self::sanitize_and_truncate_error(&err.to_string());
                        let _ = tx
                            .send(AgentEvent::MessageUpdate {
                                delta: format!("\n[Error in stream: {}]", err_msg),
                            })
                            .await;
                    }
                    Ok(delta) => match delta {
                        LlmResponseDelta::Text(content) => {
                            if reasoning_streaming {
                                // Close the reasoning block before opening the text block.
                                let _ = tx
                                    .send(AgentEvent::MessageEnd {
                                        message: Message {
                                            role: "assistant".to_string(),
                                            content: None,
                                            reasoning_content: Some(reasoning_content.clone()),
                                            name: None,
                                            tool_call_id: None,
                                            tool_calls: None,
                                        },
                                    })
                                    .await;
                                reasoning_streaming = false;
                            }
                            if !message_started {
                                let _ = tx
                                    .send(AgentEvent::MessageStart {
                                        message: Message {
                                            role: "assistant".to_string(),
                                            content: Some(String::new()),
                                            reasoning_content: None,
                                            name: None,
                                            tool_call_id: None,
                                            tool_calls: None,
                                        },
                                    })
                                    .await;
                                message_started = true;
                            }
                            assistant_content.push_str(&content);
                            let _ = tx.send(AgentEvent::MessageUpdate { delta: content }).await;
                        }
                        LlmResponseDelta::Reasoning(reasoning) => {
                            // Stream reasoning as a 💭-prefixed assistant block so users
                            // see the model's thinking incrementally. If text streaming
                            // has already started, fall back to silent accumulation —
                            // the final Message still carries reasoning_content.
                            if !message_started {
                                if !reasoning_streaming {
                                    let _ = tx
                                        .send(AgentEvent::MessageStart {
                                            message: Message {
                                                role: "assistant".to_string(),
                                                content: None,
                                                reasoning_content: Some(String::new()),
                                                name: None,
                                                tool_call_id: None,
                                                tool_calls: None,
                                            },
                                        })
                                        .await;
                                    let _ = tx
                                        .send(AgentEvent::MessageUpdate {
                                            delta: "💭 ".to_string(),
                                        })
                                        .await;
                                    reasoning_streaming = true;
                                }
                                let _ = tx
                                    .send(AgentEvent::MessageUpdate {
                                        delta: reasoning.clone(),
                                    })
                                    .await;
                            }
                            reasoning_content.push_str(&reasoning);
                        }
                        LlmResponseDelta::ToolCall {
                            index,
                            id,
                            name,
                            arguments,
                        } => {
                            let entry = pending_tool_calls.entry(index).or_insert_with(|| {
                                AccumulatingToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: String::new(),
                                }
                            });
                            if let Some(id_val) = id {
                                entry.id.push_str(&id_val);
                            }
                            if let Some(name_val) = name {
                                entry.name.push_str(&name_val);
                            }
                            entry.arguments.push_str(&arguments);
                        }
                        LlmResponseDelta::Usage(u) => {
                            turn_usage.add(&u);
                        }
                    },
                }
            }

            // Report this turn's real token usage (if the provider supplied it).
            if !turn_usage.is_zero() {
                total_usage.add(&turn_usage);
                let _ = tx.send(AgentEvent::Usage(turn_usage)).await;
            }

            // Sort by index to maintain deterministic order
            let mut tool_calls = Vec::new();
            if !pending_tool_calls.is_empty() {
                let mut sorted_keys: Vec<&usize> = pending_tool_calls.keys().collect();
                sorted_keys.sort();
                for key in sorted_keys {
                    if let Some(pending) = pending_tool_calls.get(key) {
                        tool_calls.push(ToolCall {
                            id: pending.id.clone(),
                            r#type: "function".to_string(),
                            function: ToolCallFunction {
                                name: pending.name.clone(),
                                arguments: pending.arguments.clone(),
                            },
                        });
                    }
                }
            }

            let has_content = !assistant_content.is_empty();
            let has_reasoning = !reasoning_content.is_empty();
            let has_tools = !tool_calls.is_empty();

            if has_content || has_reasoning || has_tools {
                let msg = Message {
                    role: "assistant".to_string(),
                    content: if has_content {
                        Some(assistant_content.clone())
                    } else {
                        None
                    },
                    reasoning_content: if has_reasoning {
                        Some(reasoning_content.clone())
                    } else {
                        None
                    },
                    name: None,
                    tool_call_id: None,
                    tool_calls: if has_tools {
                        Some(tool_calls.clone())
                    } else {
                        None
                    },
                };

                if message_started || reasoning_streaming {
                    let _ = tx
                        .send(AgentEvent::MessageEnd {
                            message: msg.clone(),
                        })
                        .await;
                } else if has_tools || has_reasoning {
                    let _ = tx
                        .send(AgentEvent::MessageStart {
                            message: msg.clone(),
                        })
                        .await;
                    let _ = tx
                        .send(AgentEvent::MessageEnd {
                            message: msg.clone(),
                        })
                        .await;
                }

                history.push(msg.clone());
            }

            if has_tools {
                let tx_clone = tx.clone();
                let tools_map: HashMap<String, Arc<dyn AgentTool>> = self
                    .tools
                    .iter()
                    .map(|t| (t.name().to_string(), t.clone()))
                    .collect();

                // Check if any tool requires sequential execution
                let force_sequential = tool_calls.iter().any(|tc| {
                    tools_map
                        .get(&tc.function.name)
                        .map(|t| t.execution_mode() == ExecutionMode::Sequential)
                        .unwrap_or(false)
                });

                let hooks = &self.hooks;
                let tool_calls_owned = tool_calls.clone();

                let execute_single_tool =
                    |tc: ToolCall,
                     tools_map: HashMap<String, Arc<dyn AgentTool>>,
                     tx_inner: tokio::sync::mpsc::Sender<AgentEvent>| {
                        async move {
                            let tc_id = tc.id;
                            let tool_name = tc.function.name;
                            let arguments_str = tc.function.arguments;
                            let maybe_tool = tools_map.get(&tool_name).cloned();

                            let _ = tx_inner
                                .send(AgentEvent::ToolExecutionStart {
                                    tool_call_id: tc_id.clone(),
                                    tool_name: tool_name.clone(),
                                    arguments: arguments_str.clone(),
                                })
                                .await;

                            let parsed_args_res: Result<serde_json::Value, serde_json::Error> =
                                serde_json::from_str(&arguments_str);

                            let result = match parsed_args_res {
                                Err(e) => {
                                    let err_msg =
                                        Agent::sanitize_and_truncate_error(&e.to_string());
                                    ToolResult::error(format!(
                                        "Invalid JSON arguments: {}",
                                        err_msg
                                    ))
                                }
                                Ok(args) => match maybe_tool {
                                    None => {
                                        ToolResult::error(format!("Tool '{}' not found", tool_name))
                                    }
                                    Some(tool) => tool.call(args).await,
                                },
                            };

                            let _ = tx_inner
                                .send(AgentEvent::ToolExecutionEnd {
                                    tool_call_id: tc_id.clone(),
                                    result: result.clone(),
                                })
                                .await;

                            // Build tool result message — send content as JSON for consistency
                            let result_json = serde_json::json!({
                                "result": result.content,
                                "is_error": result.is_error
                            });

                            Message {
                                role: "tool".to_string(),
                                content: Some(result_json.to_string()),
                                reasoning_content: None,
                                name: Some(tool_name),
                                tool_call_id: Some(tc_id),
                                tool_calls: None,
                            }
                        }
                    };

                let results = if force_sequential {
                    // Sequential execution
                    let mut results = Vec::new();
                    for tc in tool_calls_owned {
                        // Run beforeToolCall hook
                        let args_val: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        if let Some(h) = hooks {
                            if let Err(reason) =
                                h.before_tool_call(&tc.function.name, &args_val).await
                            {
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionStart {
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.function.name.clone(),
                                        arguments: tc.function.arguments.clone(),
                                    })
                                    .await;
                                let blocked_result =
                                    ToolResult::error(format!("Blocked by hook: {}", reason));
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionEnd {
                                        tool_call_id: tc.id.clone(),
                                        result: blocked_result.clone(),
                                    })
                                    .await;
                                let result_json = serde_json::json!({
                                    "result": blocked_result.content,
                                    "is_error": blocked_result.is_error
                                });
                                results.push(Message {
                                    role: "tool".to_string(),
                                    content: Some(result_json.to_string()),
                                    reasoning_content: None,
                                    name: Some(tc.function.name),
                                    tool_call_id: Some(tc.id),
                                    tool_calls: None,
                                });
                                continue;
                            }
                        }
                        let msg =
                            execute_single_tool(tc, tools_map.clone(), tx_clone.clone()).await;
                        results.push(msg);
                    }
                    results
                } else {
                    // Parallel execution — run beforeToolCall hooks first sequentially
                    let mut allowed_calls = Vec::new();
                    let mut blocked_results = Vec::new();

                    for tc in &tool_calls_owned {
                        let args_val: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        if let Some(h) = hooks {
                            if let Err(reason) =
                                h.before_tool_call(&tc.function.name, &args_val).await
                            {
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionStart {
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.function.name.clone(),
                                        arguments: tc.function.arguments.clone(),
                                    })
                                    .await;
                                let blocked_result =
                                    ToolResult::error(format!("Blocked by hook: {}", reason));
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionEnd {
                                        tool_call_id: tc.id.clone(),
                                        result: blocked_result.clone(),
                                    })
                                    .await;
                                let result_json = serde_json::json!({
                                    "result": blocked_result.content,
                                    "is_error": blocked_result.is_error
                                });
                                blocked_results.push((
                                    tc.id.clone(),
                                    Message {
                                        role: "tool".to_string(),
                                        content: Some(result_json.to_string()),
                                        reasoning_content: None,
                                        name: Some(tc.function.name.clone()),
                                        tool_call_id: Some(tc.id.clone()),
                                        tool_calls: None,
                                    },
                                ));
                                continue;
                            }
                        }
                        allowed_calls.push(tc.clone());
                    }

                    let tool_futures = allowed_calls
                        .into_iter()
                        .map(|tc| execute_single_tool(tc, tools_map.clone(), tx_clone.clone()));

                    let mut parallel_results: Vec<Message> =
                        futures_util::stream::iter(tool_futures)
                            .buffer_unordered(5)
                            .collect::<Vec<Message>>()
                            .await;

                    // Merge blocked results
                    for (_id, msg) in blocked_results {
                        parallel_results.push(msg);
                    }
                    parallel_results
                };

                // Re-align results with original order to maintain history determinism.
                // Results lacking a tool_call_id, or whose id is not in `tool_calls`,
                // are appended afterward as orphans so they are never silently dropped.
                let (mut results_by_id, mut orphans): (HashMap<String, Message>, Vec<Message>) = {
                    let mut by_id = HashMap::new();
                    let mut no_id = Vec::new();
                    for msg in results {
                        match msg.tool_call_id.clone() {
                            Some(id) => {
                                by_id.insert(id, msg);
                            }
                            None => no_id.push(msg),
                        }
                    }
                    (by_id, no_id)
                };

                async fn push_with_hook(
                    history: &mut Vec<Message>,
                    hooks: Option<&dyn ToolHooks>,
                    msg: Message,
                ) {
                    if let Some(h) = hooks {
                        let content_str = msg.content.clone().unwrap_or_default();
                        let parsed: serde_json::Value =
                            serde_json::from_str(&content_str).unwrap_or_default();
                        let original_result = ToolResult {
                            content: parsed["result"]
                                .as_str()
                                .unwrap_or(&content_str)
                                .to_string(),
                            is_error: parsed["is_error"].as_bool().unwrap_or(false),
                        };
                        let transformed = h
                            .after_tool_call(msg.name.as_deref().unwrap_or(""), original_result)
                            .await;
                        let result_json = serde_json::json!({
                            "result": transformed.content,
                            "is_error": transformed.is_error
                        });
                        history.push(Message {
                            content: Some(result_json.to_string()),
                            ..msg
                        });
                    } else {
                        history.push(msg);
                    }
                }

                for tc in &tool_calls {
                    if let Some(msg) = results_by_id.remove(&tc.id) {
                        push_with_hook(history, hooks.as_deref(), msg).await;
                    }
                }

                // Drain any remaining orphan results (unmatched IDs or missing IDs)
                // so we never silently lose a tool result.
                for (_, msg) in results_by_id.drain() {
                    push_with_hook(history, hooks.as_deref(), msg).await;
                }
                for msg in orphans.drain(..) {
                    push_with_hook(history, hooks.as_deref(), msg).await;
                }

                let _ = tx.send(AgentEvent::TurnEnd).await;
                // Continue Turn loop since we provided tool results back to LLM
            } else {
                // No tools: this round would end the turn. But if a steering
                // message arrived, fold it in and run one more round so the model
                // actually responds to it (keep-the-turn-alive).
                let injected = drain_injected(inject_rx.as_deref_mut(), history, &tx).await;
                let _ = tx.send(AgentEvent::TurnEnd).await;
                if injected == 0 {
                    break;
                }
            }
        }

        let _ = tx.send(AgentEvent::AgentEnd).await;
        Ok(total_usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use std::collections::HashSet;

    #[test]
    fn with_catalog_appends_when_skills_present() {
        let tmp = crate::util::unique_temp_dir("ignis-agent-catalog");
        let cwd = tmp.join("proj");
        let dir = cwd.join(".ignis/skills/react");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: react\n---\nbody").unwrap();
        let reg = Arc::new(SkillRegistry::load(None, &cwd, HashSet::new()));

        let out = with_catalog("BASE", Some(reg.as_ref()), None);
        assert!(out.starts_with("BASE\n\n"));
        assert!(out.contains("<available_skills>"));

        assert_eq!(with_catalog("BASE", None, None), "BASE");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sanitize_redacts_api_keys() {
        // Fake keys: underscores keep the secret-scan grep happy while still
        // matching our redaction regex (which accepts [A-Za-z0-9_-]).
        let key = "sk-FAKE_NOT_REAL_xxxxxxxxxxxxxxxx";
        let err = format!("auth failed with key {key}");
        let out = Agent::sanitize_and_truncate_error(&err);
        assert!(!out.contains(key), "key leaked: {out}");
        assert!(out.contains("[REDACTED_API_KEY]"));

        let ant_key = "sk-ant-api03_FAKE_dEf_xyz_long_suffix";
        let out2 = Agent::sanitize_and_truncate_error(ant_key);
        assert!(!out2.contains(ant_key));
        assert_eq!(out2, "[REDACTED_API_KEY]");
    }

    #[test]
    fn sanitize_truncates_long_errors_on_char_boundary() {
        let s = "中".repeat(400);
        let out = Agent::sanitize_and_truncate_error(&s);
        assert!(out.ends_with("... [truncated]"));
        assert!(out.is_char_boundary(0));
    }

    #[test]
    fn sanitize_short_errors_pass_through() {
        let out = Agent::sanitize_and_truncate_error("plain error");
        assert_eq!(out, "plain error");
    }
}
