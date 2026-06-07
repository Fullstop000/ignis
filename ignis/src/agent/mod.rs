use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use regex::Regex;

use crate::mcp::McpRegistry;
use crate::skills::SkillRegistry;

use serde::Serialize;

use crate::llm::{
    now_ms, LlmProvider, LlmResponseDelta, Message, ToolCall, ToolCallFunction, Usage,
};
use crate::tools::tool::{AgentTool, ExecutionMode, ToolHooks, ToolResult};

pub mod agents_md;

/// Events emitted by the agent loop as it streams a turn.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum AgentEvent {
    #[serde(rename = "turn_start")]
    TurnStart,
    #[serde(rename = "run_start")]
    RunStart,
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
    #[serde(rename = "run_end")]
    RunEnd,
    #[serde(rename = "usage")]
    Usage(Usage),
    #[serde(rename = "turn_end")]
    TurnEnd,
    #[serde(rename = "user_injected")]
    UserInjected { text: String },
    /// Streaming HTTP request to the model dropped mid-flight (e.g. provider
    /// closed the connection); the agent is about to re-issue the same request.
    /// Fires only when no partial content has been emitted yet to the user.
    /// `attempt` is 1-indexed; `max` is the total retry budget (so the visible
    /// sequence is `1/3`, `2/3`, `3/3`). `reason` is the sanitized provider error.
    #[serde(rename = "reconnecting")]
    Reconnecting {
        attempt: u32,
        max: u32,
        reason: String,
    },
    /// The user's submitted prompt, after any `UserPromptSubmit` hook chain
    /// has run. Carries the final string that gets pushed into history —
    /// this is what the console renders to scrollback so the visible block
    /// matches what the model actually saw. Emitted on every direct submit
    /// (`UserInjected` covers the queued/inject path).
    #[serde(rename = "user_prompt_committed")]
    UserPromptCommitted { text: String },
    /// Non-fatal advisory from a subsystem (e.g. hook chain). Rendered as a
    /// dim `[warn] {source}: {message}` line in scrollback. Never produced
    /// by the model loop itself — only by ignis-side machinery that wants
    /// to surface a soft failure without breaking the turn.
    #[serde(rename = "warning")]
    Warning { source: String, message: String },
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
 - Before finishing, check your work against the task's exact requirements: confirm any required output exists at the precise path and format requested (use the absolute path the task gives), and remove temporary or build artifacts you created from the deliverable location.
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

/// The `before_llm_call` lifecycle moment: drain any pending steering / inject
/// messages, run them through the `UserPromptSubmit` hook chain when a registry
/// is installed, push the effective text to `history`, and emit a `UserInjected`
/// event for each. Returns how many were folded in.
///
/// Block semantics match `Session::prompt`: a `Blocked` outcome drops the
/// inject (no history push, no `UserInjected` event) — the chain already
/// emitted a Warning. Without this, a steer message reaches the model
/// untranslated / unfiltered, which is the bilingual hook's primary use case.
async fn before_llm_call(
    inject_rx: Option<&mut tokio::sync::mpsc::Receiver<String>>,
    history: &mut Vec<Message>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_hooks: Option<&crate::hooks::HookRegistry>,
    hook_ctx: Option<crate::hooks::HookContext<'_>>,
) -> usize {
    let mut n = 0;
    if let Some(rx) = inject_rx {
        while let Ok(text) = rx.try_recv() {
            let effective = match (prompt_hooks, hook_ctx) {
                (Some(reg), Some(ctx)) => {
                    match reg.run_user_prompt_submit(&text, ctx, tx).await {
                        crate::hooks::PromptHookResult::Continue(t) => Some(t),
                        // Block: warning already emitted on `tx`; drop
                        // the inject entirely — same posture as
                        // Session::prompt's short-circuit.
                        crate::hooks::PromptHookResult::Blocked { .. } => None,
                    }
                }
                _ => Some(text),
            };
            if let Some(effective) = effective {
                history.push(Message {
                    role: "user".to_string(),
                    content: Some(effective.clone()),
                    reasoning_content: None,
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    created_at_ms: Some(now_ms()),
                });
                let _ = tx.send(AgentEvent::UserInjected { text: effective }).await;
                n += 1;
            }
        }
    }
    n
}

/// Append a tool-result message to `history`, running the `after_tool_call` hook
/// first if one is set (the hook may transform the result content/error). The
/// stored content is re-serialized as the `{result, is_error}` JSON envelope.
///
/// `args` is the JSON the tool actually ran with — `PostToolUse` subprocess
/// hooks pull `tool_input` from it. Orphan / unmatched result paths pass
/// `Value::Null` (no way to recover original args after the fact); hooks that
/// inspect `tool_input` should treat null as "unavailable".
async fn push_with_hook(
    history: &mut Vec<Message>,
    hooks: Option<&dyn ToolHooks>,
    args: &serde_json::Value,
    msg: Message,
) {
    if let Some(h) = hooks {
        let content_str = msg.content.clone().unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(&content_str).unwrap_or_default();
        let original_result = ToolResult {
            content: parsed["result"]
                .as_str()
                .unwrap_or(&content_str)
                .to_string(),
            is_error: parsed["is_error"].as_bool().unwrap_or(false),
        };
        let transformed = h
            .after_tool_call(msg.name.as_deref().unwrap_or(""), args, original_result)
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

/// Run one tool call end-to-end: emit `ToolExecutionStart`, parse arguments,
/// dispatch to the tool (or surface a not-found / bad-JSON error), record the
/// `ignis.tool.execution` span + telemetry, emit `ToolExecutionEnd`, and return
/// the `role:"tool"` result message. Captures nothing from the agent, so it runs
/// safely in parallel across calls.
async fn execute_single_tool(
    tc: ToolCall,
    tools_map: HashMap<String, Arc<dyn AgentTool>>,
    tx_inner: tokio::sync::mpsc::Sender<AgentEvent>,
) -> Message {
    let tc_id = tc.id;
    let tool_name = tc.function.name;
    let arguments_str = tc.function.arguments;
    let maybe_tool = tools_map.get(&tool_name).cloned();

    // ignis.tool.execution span. Captures wall-clock via construct→drop;
    // tool.arguments included only when IGNIS_LOG_TOOL_DETAILS=1.
    let tool_span = tracing::info_span!(
        "ignis.tool.execution",
        tool.name = %tool_name,
        tool.call_id = %tc_id,
        success = tracing::field::Empty,
        is_error = tracing::field::Empty,
        tool.arguments = tracing::field::Empty,
    );
    if crate::telemetry::log_tool_details() {
        tool_span.record("tool.arguments", arguments_str.as_str());
    }
    let tool_start = std::time::Instant::now();

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
            let err_msg = Agent::sanitize_and_truncate_error(&e.to_string());
            ToolResult::error(format!("Invalid JSON arguments: {}", err_msg))
        }
        Ok(args) => match maybe_tool {
            None => ToolResult::error(format!("Tool '{}' not found", tool_name)),
            Some(tool) => tool.call(args).await,
        },
    };

    tool_span.record("success", !result.is_error);
    tool_span.record("is_error", result.is_error);
    crate::telemetry::record_tool_call(&tool_name, tool_start.elapsed(), !result.is_error);
    drop(tool_span);

    let _ = tx_inner
        .send(AgentEvent::ToolExecutionEnd {
            tool_call_id: tc_id.clone(),
            result: result.clone(),
        })
        .await;

    tool_result_message(&tool_name, &tc_id, &result)
}

/// Build the `role:"tool"` history message for a finished tool call, wrapping
/// the result as the `{result, is_error}` JSON envelope the rest of the loop
/// expects. Shared by the normal execution path and the hook-blocked path.
fn tool_result_message(name: &str, call_id: &str, result: &ToolResult) -> Message {
    let result_json = serde_json::json!({
        "result": result.content,
        "is_error": result.is_error,
    });
    Message {
        role: "tool".to_string(),
        content: Some(result_json.to_string()),
        reasoning_content: None,
        name: Some(name.to_string()),
        tool_call_id: Some(call_id.to_string()),
        tool_calls: None,
        created_at_ms: Some(now_ms()),
    }
}

/// Outcome of the `before_tool_call` gate for a single tool call.
enum HookGateOutcome {
    /// Hook chain (or no hooks) returned `Ok(None)`. Run the tool with
    /// the original arguments.
    Proceed,
    /// A hook returned `Ok(Some(rewritten))` — the tool runs with
    /// `rewritten` substituted for the original `tool_input` (the
    /// `PreToolUse` `updatedInput` path). The caller must update the
    /// `ToolCall.function.arguments` string before dispatch so the
    /// tool, the `ToolExecutionStart` event, and any `PostToolUse`
    /// envelope all see the same args.
    Rewrite(serde_json::Value),
    /// A hook returned `Err(reason)`. The blocked `role:"tool"`
    /// message is ready to push to history; the
    /// `ToolExecutionStart`/`End` event pair has been emitted so the
    /// blocked call renders like any other.
    Block(Message),
}

/// Run the `before_tool_call` hook for `tc`. Emits the
/// `ToolExecutionStart`/`End` event pair on block so a blocked call
/// renders like any other.
async fn before_tool_call_block(
    tc: &ToolCall,
    hooks: Option<&dyn ToolHooks>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> HookGateOutcome {
    let Some(h) = hooks else {
        return HookGateOutcome::Proceed;
    };
    let args: serde_json::Value = serde_json::from_str(&tc.function.arguments).unwrap_or_default();
    match h.before_tool_call(&tc.function.name, &args).await {
        Ok(None) => HookGateOutcome::Proceed,
        Ok(Some(rewritten)) => HookGateOutcome::Rewrite(rewritten),
        Err(reason) => {
            let blocked = ToolResult::error(format!("Blocked by hook: {}", reason));
            let _ = tx
                .send(AgentEvent::ToolExecutionStart {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                })
                .await;
            let _ = tx
                .send(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tc.id.clone(),
                    result: blocked.clone(),
                })
                .await;
            HookGateOutcome::Block(tool_result_message(&tc.function.name, &tc.id, &blocked))
        }
    }
}

/// Apply a `PreToolUse` rewrite to `tc.function.arguments` in place. The
/// rewritten JSON re-serialises canonically so the `ToolExecutionStart`
/// event, the tool dispatch, and any downstream `PostToolUse` hook all
/// agree on what ran.
fn apply_rewrite(tc: &mut ToolCall, rewritten: serde_json::Value) {
    if let Ok(s) = serde_json::to_string(&rewritten) {
        tc.function.arguments = s;
    }
}

/// The accumulated result of consuming one streamed LLM run.
struct RunStream {
    assistant_content: String,
    reasoning_content: String,
    tool_calls: Vec<ToolCall>,
    message_started: bool,
    reasoning_streaming: bool,
    run_usage: Usage,
    /// Sanitized provider error if the stream terminated abnormally mid-flight
    /// (the `Err` branch in the consume loop). `None` on clean completion.
    /// The caller decides whether to retry or surface the error to the user;
    /// `consume_run_stream` itself does not emit `[Error in stream: …]`.
    stream_error: Option<String>,
}

impl RunStream {
    /// `true` when no UI-visible state was produced. Used by the retry loop
    /// to decide whether re-issuing the request is safe: emitting any
    /// assistant/reasoning text or accumulating tool-call fragments means
    /// the user has already seen partial output, so retrying would
    /// duplicate or contradict it.
    fn is_empty(&self) -> bool {
        self.assistant_content.is_empty()
            && self.reasoning_content.is_empty()
            && self.tool_calls.is_empty()
            && !self.message_started
            && !self.reasoning_streaming
    }
}

/// What one LLM run produced, handed back to the turn loop by
/// [`Agent::after_llm_call`]: the run's token usage (to accumulate) and any
/// tool calls it requested (empty when the run ended the turn).
struct RunOutcome {
    usage: Usage,
    tool_calls: Vec<ToolCall>,
}

/// Consume one streamed run: accumulate text / reasoning / tool-call / usage
/// deltas, emitting `MessageStart`/`MessageUpdate`/`MessageEnd` for streamed
/// text and reasoning blocks. Text and reasoning open as separate blocks; when
/// the active kind flips (text-while-streaming-text → reasoning, or the
/// reverse) the current block is closed and a fresh one opened, so
/// `interleaved-thinking`-style streams render in order rather than getting
/// glued together. Returns the accumulated content, the index-sorted tool
/// calls, the streaming flags, and the run's token usage.
async fn consume_run_stream(
    mut stream: futures_util::stream::BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> RunStream {
    use futures_util::stream::StreamExt;
    let mut assistant_content = String::new();
    let mut reasoning_content = String::new();
    let mut pending_tool_calls: HashMap<usize, AccumulatingToolCall> = HashMap::new();
    let mut message_started = false;
    let mut reasoning_streaming = false;
    let mut run_usage = Usage::default();
    let mut stream_error: Option<String> = None;

    while let Some(delta_result) = stream.next().await {
        match delta_result {
            Err(err) => {
                // Capture the error; do NOT emit it here. The caller's retry
                // loop checks `stream_error` and decides between re-issuing
                // the request (safe iff no partial UI state was produced) and
                // surfacing the error to the user as a normal message.
                //
                // The `break` below preserves the PR-#55 invariant: a stream
                // that re-yields the same error infinitely won't spin the loop
                // — we stop after the first error regardless of retry policy.
                stream_error = Some(Agent::sanitize_and_truncate_error(&err.to_string()));
                break;
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
                                    created_at_ms: None,
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
                                    created_at_ms: None,
                                },
                            })
                            .await;
                        message_started = true;
                    }
                    assistant_content.push_str(&content);
                    let _ = tx.send(AgentEvent::MessageUpdate { delta: content }).await;
                }
                LlmResponseDelta::Reasoning(reasoning) => {
                    // Reasoning arrived. If text was mid-stream, close it so
                    // the user sees the reasoning as its own block in order
                    // (symmetric to the text-after-reasoning path above).
                    // The renderer attaches a "✻ Thinking" header — no
                    // in-band prefix delta.
                    if message_started {
                        let _ = tx
                            .send(AgentEvent::MessageEnd {
                                message: Message {
                                    role: "assistant".to_string(),
                                    content: Some(assistant_content.clone()),
                                    reasoning_content: None,
                                    name: None,
                                    tool_call_id: None,
                                    tool_calls: None,
                                    created_at_ms: None,
                                },
                            })
                            .await;
                        message_started = false;
                    }
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
                                    created_at_ms: None,
                                },
                            })
                            .await;
                        reasoning_streaming = true;
                    }
                    let _ = tx
                        .send(AgentEvent::MessageUpdate {
                            delta: reasoning.clone(),
                        })
                        .await;
                    reasoning_content.push_str(&reasoning);
                }
                LlmResponseDelta::ToolCall {
                    index,
                    id,
                    name,
                    arguments,
                } => {
                    let entry =
                        pending_tool_calls
                            .entry(index)
                            .or_insert_with(|| AccumulatingToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
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
                    run_usage.add(&u);
                }
            },
        }
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

    RunStream {
        assistant_content,
        reasoning_content,
        tool_calls,
        message_started,
        reasoning_streaming,
        run_usage,
        stream_error,
    }
}

/// Maximum number of automatic retries when the model stream drops mid-flight
/// or `chat_stream()` returns a transport error. Total attempts = 1 initial +
/// `MAX_STREAM_RETRIES`. Three is the typical production sweet spot: enough
/// to absorb both a single blip and a brief multi-second outage, not so many
/// that a truly broken endpoint burns minutes before surfacing the error.
const MAX_STREAM_RETRIES: u32 = 3;

/// Initial backoff before retry #1. Subsequent retries double this until
/// `MAX_BACKOFF_MS` is hit. A non-zero start avoids hammering an endpoint
/// that just dropped us — the typical failure mode is upstream load, not
/// pure jitter, so an immediate retry tends to hit the same overloaded
/// state.
const BASE_BACKOFF_MS: u64 = 500;

/// Ceiling for any single retry sleep. With the current budget the schedule
/// is 500 ms → 1 s → 2 s and never reaches the cap; it exists so a future
/// `MAX_STREAM_RETRIES` bump cannot accidentally introduce minutes-long
/// sleeps if someone forgets to revisit this function.
const MAX_BACKOFF_MS: u64 = 10_000;

/// Exponential backoff before retry attempt `n` (1-indexed). Doubles from
/// `BASE_BACKOFF_MS` and saturates at `MAX_BACKOFF_MS`. Sequence with the
/// current constants: 500 ms, 1 s, 2 s, 4 s, 8 s, 10 s, 10 s, …
fn retry_backoff_ms(attempt: u32) -> u64 {
    // saturating_sub guards attempt=0 (defensive — callers pass attempt>=1);
    // .min(30) keeps the left shift in the safe range for u64 even if the
    // retry budget is ever raised dramatically. checked_shl returns None on
    // overflow → fall back to u64::MAX and then clamp to MAX_BACKOFF_MS.
    let shift = attempt.saturating_sub(1).min(30);
    BASE_BACKOFF_MS
        .checked_shl(shift)
        .unwrap_or(u64::MAX)
        .min(MAX_BACKOFF_MS)
}

/// Emit `Reconnecting { attempt, max }` and back off before the next stream
/// attempt. Shared by both retry triggers (pre-stream error, empty mid-stream
/// drop) so the reconnect-and-sleep sequence lives in exactly one place.
async fn reconnect(tx: &tokio::sync::mpsc::Sender<AgentEvent>, attempt: u32, reason: String) {
    let _ = tx
        .send(AgentEvent::Reconnecting {
            attempt,
            max: MAX_STREAM_RETRIES,
            reason,
        })
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(retry_backoff_ms(attempt))).await;
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
    /// Project instructions (from `AGENTS.md`) prepended to each request as a
    /// synthetic first user turn. Not stored in `history`.
    project_instructions: Option<String>,
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
            project_instructions: None,
        }
    }

    /// Set the `AGENTS.md` project instructions prepended to each model request.
    pub fn set_project_instructions(&mut self, instructions: Option<String>) {
        self.project_instructions = instructions;
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

    /// Open one model stream and consume a single run, retrying transient
    /// failures up to `MAX_STREAM_RETRIES`. A failure is retryable iff no
    /// UI-visible state was produced yet (see [`RunStream::is_empty`]):
    ///
    ///   * `chat_stream` returns `Err` before any stream item — nothing was
    ///     emitted, always safe to retry.
    ///   * the stream opens but the body closes before any delta arrives —
    ///     `consume_run_stream` reports `stream_error: Some` with `is_empty()`.
    ///
    /// A run that already streamed partial content returns `Ok(run)` with
    /// `stream_error: Some`, leaving the caller to surface it inline. Each
    /// retry emits `AgentEvent::Reconnecting { attempt, max, reason }` and
    /// backs off via [`reconnect`].
    async fn stream_run_with_retry(
        &self,
        effective_prompt: &str,
        messages: &[Message],
        tool_schemas: &[serde_json::Value],
        tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<RunStream, anyhow::Error> {
        let mut attempt: u32 = 0;
        loop {
            match self
                .provider
                .chat_stream(effective_prompt, messages, tool_schemas)
                .await
            {
                Err(err) if attempt < MAX_STREAM_RETRIES => {
                    attempt += 1;
                    reconnect(
                        tx,
                        attempt,
                        Self::sanitize_and_truncate_error(&err.to_string()),
                    )
                    .await;
                }
                Err(err) => return Err(err),
                Ok(stream) => {
                    let run = consume_run_stream(stream, tx).await;
                    match run.stream_error.as_deref() {
                        Some(reason) if attempt < MAX_STREAM_RETRIES && run.is_empty() => {
                            attempt += 1;
                            reconnect(tx, attempt, reason.to_string()).await;
                        }
                        _ => return Ok(run),
                    }
                }
            }
        }
    }

    /// Build the JSON function-call schemas advertised to the provider, one
    /// per registered tool.
    fn tool_schemas(&self) -> Vec<serde_json::Value> {
        self.tools
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
            .collect()
    }

    /// One LLM round: prepend project instructions, open the model stream (with
    /// retry), and record the `ignis.llm_request` span + telemetry. Returns the
    /// consumed run on success; on exhausted retries with no stream ever opened,
    /// records the failed request and returns the error for the caller to
    /// surface via [`Self::emit_fatal`].
    async fn call_llm(
        &self,
        effective_prompt: &str,
        history: &[Message],
        tool_schemas: &[serde_json::Value],
        tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<RunStream, anyhow::Error> {
        // LLM request span. Captures wall-clock via construct→drop; final attrs
        // recorded below once the stream is consumed.
        let llm_span = tracing::info_span!(
            "ignis.llm_request",
            provider = %self.provider.provider_name(),
            model = %self.provider.model_id(),
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            reasoning_tokens = tracing::field::Empty,
            success = tracing::field::Empty,
        );
        let llm_start = std::time::Instant::now();

        // Prepend the AGENTS.md project instructions as a synthetic first user
        // turn when present. Kept out of `history` so it never persists or
        // renders, and always reflects the current file (zero-copy when absent).
        let messages = agents_md::prepend(self.project_instructions.as_deref(), history);
        let result = self
            .stream_run_with_retry(effective_prompt, &messages, tool_schemas, tx)
            .await;

        match &result {
            Ok(run) => {
                // A surviving mid-stream error (`stream_error: Some`) means the
                // run produced partial-or-no content under an unhealthy stream —
                // count it as a failed request, mirroring the pre-stream path.
                let success = run.stream_error.is_none();
                llm_span.record("success", success);
                llm_span.record("input_tokens", run.run_usage.input_tokens);
                llm_span.record("output_tokens", run.run_usage.output_tokens);
                llm_span.record("reasoning_tokens", run.run_usage.reasoning_tokens);
                crate::telemetry::record_llm_request(
                    self.provider.provider_name(),
                    self.provider.model_id(),
                    llm_start.elapsed(),
                    success,
                );
            }
            Err(_) => {
                llm_span.record("success", false);
                crate::telemetry::record_llm_request(
                    self.provider.provider_name(),
                    self.provider.model_id(),
                    llm_start.elapsed(),
                    false,
                );
            }
        }
        result
    }

    /// After a run completes: surface any mid-stream error inline, report token
    /// usage, assemble the assistant message into `history`, and emit its
    /// message events. Returns the run's usage plus any tool calls it requested
    /// (empty when the run ended the turn).
    async fn after_llm_call(
        &self,
        history: &mut Vec<Message>,
        run: RunStream,
        tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> RunOutcome {
        let RunStream {
            assistant_content,
            reasoning_content,
            tool_calls,
            message_started,
            reasoning_streaming,
            run_usage,
            stream_error,
        } = run;

        // Mid-stream error AFTER partial UI state was produced (so retry was
        // not safe). Surface the error inline as a MessageUpdate so the user
        // sees the response was truncated.
        if let Some(reason) = &stream_error {
            let _ = tx
                .send(AgentEvent::MessageUpdate {
                    delta: format!("\n[Error in stream: {}]", reason),
                })
                .await;
        }

        // Report this run's real token usage (if the provider supplied it).
        if !run_usage.is_zero() {
            crate::telemetry::record_tokens(
                &run_usage,
                self.provider.provider_name(),
                self.provider.model_id(),
            );
            let _ = tx.send(AgentEvent::Usage(run_usage)).await;
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
                created_at_ms: Some(now_ms()),
            };

            if message_started || reasoning_streaming {
                let _ = tx
                    .send(AgentEvent::MessageEnd {
                        message: msg.clone(),
                    })
                    .await;
            } else if has_tools {
                // No streamed content but the run produced tool calls —
                // synthesize a Start/End pair so the UI bookkeeping matches the
                // persisted message. (`has_reasoning` can't reach here: any
                // reasoning chunk would have flipped `reasoning_streaming`.)
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

            history.push(msg);
        }

        RunOutcome {
            usage: run_usage,
            tool_calls,
        }
    }

    /// Emit the assistant `Error: …` message-pair shown when all stream retries
    /// were exhausted before any stream opened. The failed request itself is
    /// already recorded by [`Self::call_llm`]; this only surfaces the UI.
    async fn emit_fatal(&self, err: anyhow::Error, tx: &tokio::sync::mpsc::Sender<AgentEvent>) {
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
                    created_at_ms: None,
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
                    created_at_ms: None,
                },
            })
            .await;
    }

    /// Run the model + tool loop over `history` until the turn completes — the
    /// model stops requesting tools and no steering remains — appending
    /// assistant and tool messages in place and emitting events. This is the
    /// stable turn skeleton; each lifecycle moment lives in its own method:
    /// draining steering ([`before_llm_call`]), the LLM round
    /// ([`Self::call_llm`]), assembling the reply ([`Self::after_llm_call`]),
    /// and tool execution ([`Self::execute_tool_calls`]). Does not load or
    /// persist; the caller ([`crate::Session`]) owns history and storage.
    pub async fn run(
        &self,
        history: &mut Vec<Message>,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
        mut inject_rx: Option<&mut tokio::sync::mpsc::Receiver<String>>,
        prompt_hooks: Option<&crate::hooks::HookRegistry>,
        hook_ctx: Option<crate::hooks::HookContext<'_>>,
    ) -> Result<Usage, anyhow::Error> {
        let _ = tx.send(AgentEvent::TurnStart).await;
        let effective_prompt = with_catalog(
            &self.system_prompt,
            self.skills.as_deref(),
            self.mcp.as_deref(),
        );
        let tool_schemas = self.tool_schemas();
        let mut total_usage = Usage::default();

        loop {
            before_llm_call(
                inject_rx.as_deref_mut(),
                history,
                &tx,
                prompt_hooks,
                hook_ctx,
            )
            .await;
            let _ = tx.send(AgentEvent::RunStart).await;

            // SystemPromptCompose: hooks may rewrite the system prompt
            // per LLM call (e.g. trim git diff for token-efficiency
            // research). Hooks may also inject `additionalContext`,
            // queued for the same flush path PostToolUse uses. No
            // hook chain → zero overhead.
            let call_prompt: std::borrow::Cow<'_, str> = match (prompt_hooks, hook_ctx) {
                (Some(reg), Some(ctx)) => std::borrow::Cow::Owned(
                    reg.run_system_prompt_compose(
                        &effective_prompt,
                        self.provider.model_id(),
                        ctx,
                        &tx,
                    )
                    .await,
                ),
                _ => std::borrow::Cow::Borrowed(effective_prompt.as_str()),
            };

            let run = match self
                .call_llm(&call_prompt, history, &tool_schemas, &tx)
                .await
            {
                Ok(run) => run,
                Err(err) => {
                    // All retries exhausted with no stream ever opened.
                    self.emit_fatal(err, &tx).await;
                    break;
                }
            };

            let outcome = self.after_llm_call(history, run, &tx).await;
            total_usage.add(&outcome.usage);

            if !outcome.tool_calls.is_empty() {
                self.execute_tool_calls(&outcome.tool_calls, history, &tx)
                    .await;
                let _ = tx.send(AgentEvent::RunEnd).await;
                // Continue the turn loop: tool results go back to the model.
            } else {
                // No tools: this run would end the turn. But if a steering
                // message arrived, fold it in and run one more round so the
                // model responds to it (keep-the-turn-alive).
                let injected = before_llm_call(
                    inject_rx.as_deref_mut(),
                    history,
                    &tx,
                    prompt_hooks,
                    hook_ctx,
                )
                .await;
                let _ = tx.send(AgentEvent::RunEnd).await;
                if injected == 0 {
                    break;
                }
            }
        }

        let _ = tx.send(AgentEvent::TurnEnd).await;
        Ok(total_usage)
    }

    /// Run all tool calls for a run and append their results to `history`.
    /// Honors `before_tool_call`/`after_tool_call` hooks, runs in parallel
    /// (bounded) unless any tool demands sequential execution, and re-aligns
    /// results to the original call order so history stays deterministic (with
    /// any orphan results appended rather than dropped).
    async fn execute_tool_calls(
        &self,
        tool_calls: &[ToolCall],
        history: &mut Vec<Message>,
        tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    ) {
        use futures_util::stream::StreamExt;
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
        let tool_calls_owned = tool_calls.to_vec();

        let results = if force_sequential {
            // Sequential execution
            let mut results = Vec::new();
            for mut tc in tool_calls_owned {
                match before_tool_call_block(&tc, hooks.as_deref(), &tx_clone).await {
                    HookGateOutcome::Block(blocked) => results.push(blocked),
                    HookGateOutcome::Rewrite(args) => {
                        apply_rewrite(&mut tc, args);
                        results.push(
                            execute_single_tool(tc, tools_map.clone(), tx_clone.clone()).await,
                        );
                    }
                    HookGateOutcome::Proceed => {
                        results.push(
                            execute_single_tool(tc, tools_map.clone(), tx_clone.clone()).await,
                        );
                    }
                }
            }
            results
        } else {
            // Parallel execution — run before_tool_call hooks first
            // sequentially, then fan out the allowed (post-rewrite) calls.
            let mut allowed_calls = Vec::new();
            let mut blocked_results: Vec<Message> = Vec::new();

            for tc in &tool_calls_owned {
                let mut tc = tc.clone();
                match before_tool_call_block(&tc, hooks.as_deref(), &tx_clone).await {
                    HookGateOutcome::Block(blocked) => blocked_results.push(blocked),
                    HookGateOutcome::Rewrite(args) => {
                        apply_rewrite(&mut tc, args);
                        allowed_calls.push(tc);
                    }
                    HookGateOutcome::Proceed => allowed_calls.push(tc),
                }
            }

            let tool_futures = allowed_calls
                .into_iter()
                .map(|tc| execute_single_tool(tc, tools_map.clone(), tx_clone.clone()));

            let mut parallel_results: Vec<Message> = futures_util::stream::iter(tool_futures)
                .buffer_unordered(5)
                .collect::<Vec<Message>>()
                .await;

            parallel_results.append(&mut blocked_results);
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

        for tc in tool_calls {
            if let Some(msg) = results_by_id.remove(&tc.id) {
                // Parse args from the original tool call. Fallback to Null on
                // malformed JSON — the tool's already run, we don't fail the
                // history push over a bad PostToolUse envelope.
                let args =
                    serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
                push_with_hook(history, hooks.as_deref(), &args, msg).await;
            }
        }

        // Drain any remaining orphan results (unmatched IDs or missing IDs) so we
        // never silently lose a tool result. Sort by tool_call_id so the appended
        // order is deterministic across runs (HashMap drain order is not).
        // Orphans have no recoverable args — PostToolUse hooks get Value::Null.
        let null_args = serde_json::Value::Null;
        let mut leftover: Vec<(String, Message)> = results_by_id.drain().collect();
        leftover.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, msg) in leftover {
            push_with_hook(history, hooks.as_deref(), &null_args, msg).await;
        }
        for msg in orphans.drain(..) {
            push_with_hook(history, hooks.as_deref(), &null_args, msg).await;
        }

        // Flush any `additionalContext` queued by PostToolUse hooks
        // (and, eventually, the other inject-context events). Each
        // pending injection becomes a synthetic `role:"user"` message
        // wrapping a `<system-reminder>` block — the next LLM call
        // reads it like any other reminder. Rendered via the single
        // helper in `hooks::render_injection_as_system_reminder` so
        // wire format stays consistent with tests and dashboards.
        if let Some(h) = hooks.as_deref() {
            for inj in h.drain_pending_context().await {
                history.push(Message {
                    role: "user".to_string(),
                    content: Some(crate::hooks::render_injection_as_system_reminder(&inj)),
                    reasoning_content: None,
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    created_at_ms: Some(now_ms()),
                });
            }
        }
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

    #[tokio::test]
    async fn stream_error_captured_and_stops_consuming() {
        use futures_util::stream::StreamExt;
        // PR #55 invariant: a stream that re-yields the same transport error
        // doesn't spin — we stop after the first error. The retry refactor
        // changed *where* the error surfaces: `consume_run_stream` now
        // captures it in `stream_error` and emits nothing itself; the caller
        // (the retry loop in `Agent::run`) decides between re-issuing the
        // request and surfacing the error to the user. Both properties are
        // checked here.
        let errs: Vec<Result<LlmResponseDelta, anyhow::Error>> = (0..50)
            .map(|_| {
                Err(anyhow::anyhow!(
                    "error reading a body from connection: connection reset"
                ))
            })
            .collect();
        let stream = futures_util::stream::iter(errs).boxed();
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        let result = consume_run_stream(stream, &tx).await;
        drop(tx);

        let mut error_messages = 0;
        let mut any_events = 0;
        while let Some(ev) = rx.recv().await {
            any_events += 1;
            if let AgentEvent::MessageUpdate { delta } = ev {
                if delta.contains("Error in stream") {
                    error_messages += 1;
                }
            }
        }
        assert_eq!(
            any_events, 0,
            "consume_run_stream should emit no events for a pure-error stream; \
             error surfacing is the caller's job"
        );
        assert_eq!(error_messages, 0);
        assert!(result.assistant_content.is_empty());
        assert!(result.is_empty());
        assert!(
            result
                .stream_error
                .as_deref()
                .is_some_and(|s| s.contains("connection reset")),
            "stream_error should carry the sanitized failure reason, got {:?}",
            result.stream_error
        );
    }

    #[tokio::test]
    async fn clean_stream_leaves_stream_error_none() {
        use crate::llm::Usage;
        use futures_util::stream::StreamExt;
        let deltas: Vec<Result<LlmResponseDelta, anyhow::Error>> = vec![
            Ok(LlmResponseDelta::Text("hello ".into())),
            Ok(LlmResponseDelta::Text("world".into())),
            Ok(LlmResponseDelta::Usage(Usage::default())),
        ];
        let stream = futures_util::stream::iter(deltas).boxed();
        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        let result = consume_run_stream(stream, &tx).await;
        drop(tx);
        // Drain so the channel doesn't leak warnings in test output.
        while rx.recv().await.is_some() {}
        assert_eq!(result.assistant_content, "hello world");
        assert!(result.stream_error.is_none());
        assert!(!result.is_empty(), "non-empty content => is_empty()==false");
    }

    #[test]
    fn turn_stream_is_empty_is_strict() {
        let mut t = RunStream {
            assistant_content: String::new(),
            reasoning_content: String::new(),
            tool_calls: Vec::new(),
            message_started: false,
            reasoning_streaming: false,
            run_usage: crate::llm::Usage::default(),
            stream_error: None,
        };
        assert!(t.is_empty());
        // Any one of these means the user has already seen partial state, so
        // retry would corrupt the UI; `is_empty()` must reject all of them.
        t.assistant_content.push('x');
        assert!(!t.is_empty());
        t.assistant_content.clear();
        t.reasoning_content.push('y');
        assert!(!t.is_empty());
        t.reasoning_content.clear();
        t.tool_calls.push(ToolCall {
            id: "1".into(),
            r#type: "function".into(),
            function: ToolCallFunction {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        });
        assert!(!t.is_empty());
        t.tool_calls.clear();
        t.message_started = true;
        assert!(!t.is_empty());
        t.message_started = false;
        t.reasoning_streaming = true;
        assert!(!t.is_empty());
    }

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        // Schedule for the current budget of 3 retries: 500 ms → 1 s → 2 s.
        assert_eq!(retry_backoff_ms(1), BASE_BACKOFF_MS);
        assert_eq!(retry_backoff_ms(2), BASE_BACKOFF_MS * 2);
        assert_eq!(retry_backoff_ms(3), BASE_BACKOFF_MS * 4);
        // Doubling continues until MAX_BACKOFF_MS clamps it.
        assert_eq!(retry_backoff_ms(4), BASE_BACKOFF_MS * 8);
        assert!(
            retry_backoff_ms(5) >= BASE_BACKOFF_MS * 16 || retry_backoff_ms(5) == MAX_BACKOFF_MS
        );
        // The ceiling holds for any number of attempts, including pathological
        // values that would otherwise overflow the u64 shift.
        assert_eq!(retry_backoff_ms(30), MAX_BACKOFF_MS);
        assert_eq!(retry_backoff_ms(u32::MAX), MAX_BACKOFF_MS);
        // Defensive: attempt=0 (shouldn't happen, but if it does) still
        // returns a sane positive value, not zero or a panic.
        assert_eq!(retry_backoff_ms(0), BASE_BACKOFF_MS);
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
