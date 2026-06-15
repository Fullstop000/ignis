use async_trait::async_trait;
use futures_util::stream::{BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;

mod message;
pub use message::{now_ms, Message, ToolCall, ToolCallFunction, Usage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmResponseDelta {
    Text(String),
    Reasoning(String),
    ToolCall {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Real token usage for the completion (emitted once, near the end).
    Usage(Usage),
}

#[async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error>;

    /// Return the model identifier this provider is configured for. Used for
    /// telemetry attributes and any caller that needs to know the active model
    /// without inspecting config.
    fn model_id(&self) -> &str;

    /// Return the provider name (e.g. "openai", "anthropic", "kimi-code"). Used
    /// for telemetry attributes. Stored on the struct rather than the trait so
    /// OpenAiProvider can distinguish openai vs. kimi vs. moonshot.
    fn provider_name(&self) -> &str;
}

mod anthropic;
mod ollama;
mod openai;

/// Wire protocol — selects the concrete protocol client in [`build`] and gates
/// tool support (only `Ollama` lacks it). Deserialized directly from a config
/// `protocol = "..."` override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[serde(alias = "openai-compatible")]
    OpenAi,
    Anthropic,
    Ollama,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Anthropic => "anthropic",
            Protocol::Ollama => "ollama",
        }
    }
}

/// How the API key is attached. Decoupled from [`Protocol`]: MiniMax's Anthropic
/// endpoint uses `Bearer`, while real Anthropic uses `XApiKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>` (Anthropic)
    XApiKey,
    /// No credential (Ollama)
    None,
}

/// A fully-resolved active selection: provider metadata merged with config
/// overrides, with one endpoint chosen. [`build`] turns it into a concrete
/// [`LlmProvider`].
pub struct Resolved {
    pub provider_id: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub auth: Auth,
    pub api_key: Option<String>,
    pub model: String,
    pub request_headers: Vec<(String, String)>,
    pub reasoning_effort: Option<String>,
}

/// Carries a non-2xx provider response back to callers so retry logic can
/// distinguish transient 5xx failures from fatal 4xx/audit errors.
#[derive(Debug)]
pub(crate) struct LlmHttpError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for LlmHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LLM API returned error {}: {}", self.status, self.body)
    }
}

impl std::error::Error for LlmHttpError {}

/// Time-to-first-byte timeout for LLM chat requests. Covers connection
/// establishment + response headers; the returned stream is not bounded by this.
const LLM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// Send a request and fail fast if the provider doesn't start responding within
/// [`LLM_RESPONSE_TIMEOUT`]. This prevents a hung connection from blocking the
/// agent loop forever without cutting off a slow-but-healthy streaming body.
async fn send_with_timeout(
    req: reqwest::RequestBuilder,
) -> Result<reqwest::Response, anyhow::Error> {
    match tokio::time::timeout(LLM_RESPONSE_TIMEOUT, req.send()).await {
        Ok(res) => res.map_err(Into::into),
        Err(_) => Err(anyhow::anyhow!(
            "LLM request timed out after {}s waiting for response",
            LLM_RESPONSE_TIMEOUT.as_secs()
        )),
    }
}

/// Construct the concrete protocol client for a [`Resolved`] selection. The single
/// `match` on `protocol` lives here (build time) — never inside `chat_stream`.
pub fn build(r: Resolved) -> Box<dyn LlmProvider> {
    match r.protocol {
        Protocol::OpenAi => Box::new(openai::OpenAiCompatible::new(r)),
        Protocol::Anthropic => Box::new(anthropic::AnthropicCompatible::new(r)),
        Protocol::Ollama => Box::new(ollama::Ollama::new(r)),
    }
}

// ==========================================
// OpenAI-compatible request/response types
// Shared by OpenAI, DeepSeek, Kimi, and other compatible providers.
// ==========================================

#[derive(Serialize)]
pub struct ChatCompletionsRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "is_empty_slice")]
    pub tools: &'a [serde_json::Value],
    pub stream: bool,
    /// Ask the API to emit a final usage chunk (OpenAI-compatible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Reasoning effort (`low`/`medium`/`high`); omitted when the model/provider
    /// doesn't support it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'a str>,
}

#[derive(Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

fn is_empty_slice(slice: &[serde_json::Value]) -> bool {
    slice.is_empty()
}

#[derive(Deserialize, Debug)]
pub struct Chunk {
    pub choices: Option<Vec<ChunkChoice>>,
    #[serde(default)]
    pub usage: Option<ChunkUsage>,
}

/// OpenAI-compatible usage object from the final stream chunk. `prompt_tokens`
/// is the full input (cache hits included); cache reads appear either as
/// OpenAI's `prompt_tokens_details.cached_tokens` or DeepSeek's
/// `prompt_cache_hit_tokens`. `completion_tokens_details.reasoning_tokens` is
/// the invisible-thinking subset of `completion_tokens` for o-series models.
#[derive(Deserialize, Debug)]
pub struct ChunkUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
}

#[derive(Deserialize, Debug)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

#[derive(Deserialize, Debug)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl ChunkUsage {
    pub fn to_usage(&self) -> Usage {
        let cache_read = self
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
            .unwrap_or(0);
        let reasoning = self
            .completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);
        Usage {
            // prompt_tokens already includes cached tokens.
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            reasoning_tokens: reasoning,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct ChunkChoice {
    pub delta: ChunkDelta,
}

#[derive(Deserialize, Debug)]
pub struct ChunkDelta {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Deserialize, Debug)]
pub struct ChunkToolCall {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<ChunkFunction>,
}

#[derive(Deserialize, Debug)]
pub struct ChunkFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

pub(crate) fn bytes_to_lines<S, E>(stream: S) -> impl Stream<Item = Result<String, anyhow::Error>>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Send + Unpin + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    futures_util::stream::unfold(
        (stream, Vec::<u8>::new()),
        |(mut stream, mut buffer)| async move {
            loop {
                if let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                    let line_bytes = buffer.drain(..=pos).collect::<Vec<u8>>();
                    if let Ok(line) = String::from_utf8(line_bytes) {
                        return Some((Ok(line), (stream, buffer)));
                    }
                }

                match stream.next().await {
                    Some(Ok(bytes)) => {
                        buffer.extend_from_slice(&bytes);
                    }
                    Some(Err(err)) => {
                        return Some((Err(anyhow::Error::new(err)), (stream, buffer)));
                    }
                    None => {
                        if !buffer.is_empty() {
                            let line_bytes = std::mem::take(&mut buffer);
                            if let Ok(line) = String::from_utf8(line_bytes) {
                                return Some((Ok(line), (stream, buffer)));
                            }
                        }
                        return None;
                    }
                }
            }
        },
    )
}

/// Parse one SSE line's payload into a response delta. Returns `None` for lines
/// that carry no delta: blank lines, comments / non-`data:` lines, the terminal
/// `[DONE]`, unparseable JSON, and empty-content deltas. Shared by every
/// OpenAI-compatible provider so the mapping lives — and is tested — once.
pub(crate) fn parse_sse_line(line: &str) -> Option<LlmResponseDelta> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let data_part = line.strip_prefix("data:")?.trim();
    if data_part == "[DONE]" {
        return None;
    }
    let chunk: Chunk = serde_json::from_str(data_part).ok()?;
    if let Some(choice) = chunk.choices.as_ref().and_then(|c| c.first()) {
        if let Some(content) = &choice.delta.content {
            if !content.is_empty() {
                return Some(LlmResponseDelta::Text(content.clone()));
            }
        }
        if let Some(reasoning) = &choice.delta.reasoning_content {
            if !reasoning.is_empty() {
                return Some(LlmResponseDelta::Reasoning(reasoning.clone()));
            }
        }
        if let Some(tc) = choice.delta.tool_calls.as_ref().and_then(|t| t.first()) {
            let name = tc.function.as_ref().and_then(|f| f.name.clone());
            let arguments = tc
                .function
                .as_ref()
                .and_then(|f| f.arguments.clone())
                .unwrap_or_default();
            return Some(LlmResponseDelta::ToolCall {
                index: tc.index,
                id: tc.id.clone(),
                name,
                arguments,
            });
        }
    }
    if let Some(u) = &chunk.usage {
        return Some(LlmResponseDelta::Usage(u.to_usage()));
    }
    None
}

/// Policy for trimming prior-turn material from a `history` before serializing
/// it out to the model. One lever: drop prior-turn reasoning on assistant
/// messages that did NOT call a tool. Mirrors DeepSeek's documented contract
/// (`reasoning_content` is *ignored* on non-tool turns when sent, mandatory
/// when adjacent to a tool call) and Anthropic's default behavior on Sonnet
/// ≤4.5 / Haiku via `clear_thinking_20251015`. Also strips inline
/// `<think>...</think>` regions from `content` on those text-only turns
/// (MiniMax-M3 emits its reasoning that way, inline in the visible content
/// stream).
#[derive(Clone, Copy, Debug)]
pub struct HistoryPolicy {
    pub strip_think: bool,
}

/// Config-supplied override for the default history policy. Set once at
/// startup by [`set_history_policy`] when the user's `~/.ignis/config.toml`
/// includes a `[settings] strip-think = ...` line.
static HISTORY_POLICY_FROM_CONFIG: std::sync::OnceLock<HistoryPolicy> = std::sync::OnceLock::new();

/// Set the config-derived default [`HistoryPolicy`]. Called once at startup
/// from `load_config()`. First call wins; subsequent calls are no-ops.
pub fn set_history_policy(policy: HistoryPolicy) {
    let _ = HISTORY_POLICY_FROM_CONFIG.set(policy);
}

impl Default for HistoryPolicy {
    /// Resolved in precedence order, highest first:
    ///
    /// 1. `[settings] strip-think = ...` from `~/.ignis/config.toml`, plumbed in
    ///    once at startup via [`set_history_policy`].
    /// 2. Built-in fallback: strip on. Cache-stable; never regressed in the
    ///    validation A/B series that led to this default (PR #123).
    fn default() -> Self {
        HISTORY_POLICY_FROM_CONFIG
            .get()
            .copied()
            .unwrap_or(Self { strip_think: true })
    }
}

/// Strip every `<think>...</think>` region from `s` (case-sensitive, multi-line,
/// non-greedy). MiniMax-M3 emits chain-of-thought as inline `<think>...</think>`
/// in the visible content stream rather than via the `reasoning_content` field,
/// so without this strip every prior turn's CoT is replayed verbatim on every
/// subsequent turn — pushing the actual work past the timeout budget (we
/// observed this on 12/12 TimedOut MM3 trials in TB 2.1 06-03). Idempotent.
fn strip_think_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("<think>") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + "<think>".len()..];
        if let Some(close) = after_open.find("</think>") {
            rest = &after_open[close + "</think>".len()..];
        } else {
            // Unclosed tag — keep the tail intact rather than dropping it
            // silently. A stream-truncated `<think>` will surface as visible
            // text on next replay, which is the safe-degrade behavior.
            out.push_str("<think>");
            out.push_str(after_open);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Prepare a history `&[Message]` slice for outbound serialization, applying
/// the strip configured by [`HistoryPolicy`]. Returns a fresh `Vec` — the
/// caller's history is never mutated. Identity transform when the strip is
/// disabled or on empty input.
///
/// Invariants:
/// - Message ordering and count are unchanged.
/// - `tool_call_id` / `tool_calls` are never touched, so the call→result
///   linkage providers validate (especially Anthropic) stays intact.
/// - `reasoning_content` is preserved on every tool-calling assistant turn.
///   DeepSeek 400s otherwise, and Anthropic requires intra-turn thinking
///   adjacency to its `tool_use` block. Only text-only assistant turns get
///   their inline `<think>...</think>` stripped and `reasoning_content` cleared.
pub(crate) fn prep_outbound_history(messages: &[Message], policy: &HistoryPolicy) -> Vec<Message> {
    let mut out: Vec<Message> = messages.to_vec();
    if !policy.strip_think {
        return out;
    }
    for msg in out.iter_mut() {
        if msg.role != "assistant" {
            continue;
        }
        let is_tool_calling = msg.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty());
        if is_tool_calling {
            continue;
        }
        if let Some(content) = msg.content.as_ref() {
            if content.contains("<think>") {
                msg.content = Some(strip_think_blocks(content));
            }
        }
        msg.reasoning_content = None;
    }
    out
}

/// Stream a chat completion from an OpenAI-compatible endpoint. The only
/// provider-specific knob is `request_headers`: built-in provider declarations
/// can add headers such as Kimi's whitelisted `User-Agent`. All response parsing
/// is shared via `parse_sse_line`, so a streaming-parser change happens in
/// exactly one place.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn openai_compatible_chat_stream(
    client: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    model: &str,
    reasoning_effort: Option<&str>,
    request_headers: &[(String, String)],
    system_prompt: &str,
    messages: &[Message],
    tools: &[serde_json::Value],
) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
    let history = prep_outbound_history(messages, &HistoryPolicy::default());
    let mut request_messages = vec![Message {
        role: "system".to_string(),
        content: Some(system_prompt.to_string()),
        reasoning_content: None,
        name: None,
        tool_call_id: None,
        tool_calls: None,
        created_at_ms: None,
    }];
    request_messages.extend(history);

    let req_body = ChatCompletionsRequest {
        model,
        messages: request_messages,
        tools,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        reasoning_effort,
    };

    let endpoint = if api_url.ends_with("/chat/completions") {
        api_url.to_string()
    } else {
        format!("{}/chat/completions", api_url.trim_end_matches('/'))
    };

    let mut req = client
        .post(&endpoint)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&req_body);
    for (name, value) in request_headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let res = send_with_timeout(req).await?;

    let status = res.status();
    if !status.is_success() {
        let error_text = res
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(LlmHttpError {
            status,
            body: error_text,
        }
        .into());
    }

    let line_stream = bytes_to_lines(res.bytes_stream());
    let delta_stream = line_stream.filter_map(|line_result| async move {
        match line_result {
            Err(err) => Some(Err(err)),
            Ok(line) => parse_sse_line(&line).map(Ok),
        }
    });
    Ok(delta_stream.boxed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_usage_maps_to_usage() {
        let chunk: Chunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":42,"prompt_tokens_details":{"cached_tokens":600}}}"#,
        )
        .unwrap();
        let u = chunk.usage.unwrap().to_usage();
        assert_eq!(u.input_tokens, 1000); // includes cached
        assert_eq!(u.output_tokens, 42);
        assert_eq!(u.cache_read_tokens, 600);
        assert_eq!(u.cache_write_tokens, 0);
    }

    #[test]
    fn chunk_usage_maps_deepseek_cache_field() {
        // DeepSeek reports cache reads as `prompt_cache_hit_tokens`.
        let chunk: Chunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":2157,"completion_tokens":2,"prompt_cache_hit_tokens":1920,"prompt_cache_miss_tokens":237}}"#,
        )
        .unwrap();
        let u = chunk.usage.unwrap().to_usage();
        assert_eq!(u.input_tokens, 2157);
        assert_eq!(u.output_tokens, 2);
        assert_eq!(u.cache_read_tokens, 1920);
    }

    // ---- parse_sse_line: the SSE-line → delta mapping shared by every
    // OpenAI-compatible provider (previously copy-pasted into each). ----

    #[test]
    fn parse_sse_text_delta() {
        let d = parse_sse_line(r#"data: {"choices":[{"delta":{"content":"hello"}}]}"#);
        assert!(matches!(d, Some(LlmResponseDelta::Text(t)) if t == "hello"));
    }

    #[test]
    fn parse_sse_reasoning_delta() {
        let d = parse_sse_line(r#"data: {"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#);
        assert!(matches!(d, Some(LlmResponseDelta::Reasoning(t)) if t == "hmm"));
    }

    #[test]
    fn parse_sse_tool_call_delta() {
        let d = parse_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":"}}]}}]}"#,
        );
        match d {
            Some(LlmResponseDelta::ToolCall {
                index,
                id,
                name,
                arguments,
            }) => {
                assert_eq!(index, 0);
                assert_eq!(id.as_deref(), Some("call_1"));
                assert_eq!(name.as_deref(), Some("bash"));
                assert_eq!(arguments, r#"{"command":"#);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_tool_call_missing_arguments_defaults_empty() {
        let d = parse_sse_line(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"x"}}]}}]}"#,
        );
        assert!(
            matches!(d, Some(LlmResponseDelta::ToolCall { arguments, .. }) if arguments.is_empty())
        );
    }

    #[test]
    fn parse_sse_usage_delta() {
        let d = parse_sse_line(
            r#"data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#,
        );
        assert!(matches!(d, Some(LlmResponseDelta::Usage(u)) if u.input_tokens == 10));
    }

    #[test]
    fn parse_sse_done_and_noise_yield_none() {
        assert!(parse_sse_line("data: [DONE]").is_none());
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line("   ").is_none());
        assert!(parse_sse_line(": keep-alive comment").is_none()); // not a data: line
        assert!(parse_sse_line("data: {not json}").is_none());
        // Empty content delta carries no signal.
        assert!(parse_sse_line(r#"data: {"choices":[{"delta":{"content":""}}]}"#).is_none());
    }

    #[test]
    fn parse_sse_prefers_content_over_reasoning() {
        // When both are present, content wins (matches the prior per-provider order).
        let d = parse_sse_line(
            r#"data: {"choices":[{"delta":{"content":"a","reasoning_content":"b"}}]}"#,
        );
        assert!(matches!(d, Some(LlmResponseDelta::Text(t)) if t == "a"));
    }

    // ---- prep_outbound_history: stale tool-output masking + reasoning strip ----
    //
    // Constructor helpers — kept private to the test module so each assertion
    // reads as plain narrative without struct-literal noise.

    fn user(text: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: Some(text.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }
    }

    fn assistant_text(text: &str, reasoning: Option<&str>) -> Message {
        Message {
            role: "assistant".to_string(),
            content: Some(text.to_string()),
            reasoning_content: reasoning.map(str::to_string),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }
    }

    fn assistant_calling(text: &str, reasoning: Option<&str>, call_id: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: Some(text.to_string()),
            reasoning_content: reasoning.map(str::to_string),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: call_id.to_string(),
                r#type: "function".to_string(),
                function: ToolCallFunction {
                    name: "bash".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                },
            }]),
            created_at_ms: None,
        }
    }

    fn tool_result(call_id: &str, name: &str, body: &str) -> Message {
        Message {
            role: "tool".to_string(),
            content: Some(body.to_string()),
            reasoning_content: None,
            name: Some(name.to_string()),
            tool_call_id: Some(call_id.to_string()),
            tool_calls: None,
            created_at_ms: None,
        }
    }

    #[test]
    fn prep_history_strips_reasoning_on_text_only_assistant_turn() {
        let history = vec![
            user("question"),
            assistant_text("the answer is 42", Some("first I considered ...")),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy { strip_think: true });
        assert!(
            out[1].reasoning_content.is_none(),
            "reasoning should be stripped"
        );
        assert_eq!(
            out[1].content.as_deref(),
            Some("the answer is 42"),
            "content untouched"
        );
    }

    #[test]
    fn prep_history_keeps_reasoning_on_tool_calling_assistant_turn() {
        // DeepSeek 400s if reasoning is dropped on a turn whose assistant message
        // performed a tool call; Anthropic requires intra-turn thinking adjacency.
        let history = vec![
            user("please run ls"),
            assistant_calling("I'll run ls.", Some("plan: list /app"), "call_1"),
            tool_result("call_1", "bash", "app/  README.md"),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy { strip_think: true });
        assert_eq!(
            out[1].reasoning_content.as_deref(),
            Some("plan: list /app"),
            "tool-calling turn must keep its reasoning",
        );
    }

    #[test]
    fn prep_history_strips_inline_think_blocks_from_mm3_text_turn() {
        // MM3 emits chain-of-thought inline as `<think>...</think>` in `content`.
        let history = vec![
            user("hi"),
            assistant_text(
                "<think>let me think about this\nmulti-line</think>here is my answer",
                None,
            ),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy { strip_think: true });
        assert_eq!(out[1].content.as_deref(), Some("here is my answer"));
    }

    #[test]
    fn prep_history_strips_multiple_think_blocks_on_text_turn() {
        let history = vec![
            user("hi"),
            assistant_text("<think>A</think>x<think>B\nC</think>y", None),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy { strip_think: true });
        assert_eq!(out[1].content.as_deref(), Some("xy"));
    }

    #[test]
    fn prep_history_keeps_think_blocks_on_tool_calling_turn() {
        // Strip only applies to text-only assistant turns — tool-calling turns
        // keep their content verbatim so per-provider adjacency invariants hold.
        let history = vec![
            user("ls"),
            assistant_calling("<think>plan</think>running ls", None, "call_1"),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy { strip_think: true });
        assert_eq!(
            out[1].content.as_deref(),
            Some("<think>plan</think>running ls")
        );
    }

    #[test]
    fn prep_history_with_strip_disabled_preserves_reasoning_and_think_tags() {
        let policy = HistoryPolicy { strip_think: false };
        let history = vec![
            user("hi"),
            assistant_text("<think>raw</think>answer", Some("kept reasoning")),
        ];
        let out = prep_outbound_history(&history, &policy);
        assert_eq!(out[1].reasoning_content.as_deref(), Some("kept reasoning"));
        assert_eq!(out[1].content.as_deref(), Some("<think>raw</think>answer"));
    }

    #[test]
    fn prep_history_unclosed_think_tag_preserved_on_text_turn() {
        // Safe-degrade: if a `<think>` was never closed (stream truncation),
        // keep the tail intact rather than dropping the rest of the content.
        let history = vec![
            user("hi"),
            assistant_text("ok<think>truncated and no closer", None),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy { strip_think: true });
        assert_eq!(
            out[1].content.as_deref(),
            Some("ok<think>truncated and no closer")
        );
    }

    #[test]
    fn prep_history_identity_on_empty_input() {
        assert!(prep_outbound_history(&[], &HistoryPolicy { strip_think: true }).is_empty());
    }
}
