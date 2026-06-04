use async_trait::async_trait;
use futures_util::stream::{BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};

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
/// it out to the model. Two independent levers, both grounded in published
/// agent-efficiency work:
///
/// - `keep_live_tool_outputs` — JetBrains "complexity-trap" observation-masking
///   (arxiv 2508.21433). Halves agent-context cost on SWE-bench Verified with
///   no solve-rate loss vs LLM summarization, by blanking the *bodies* of stale
///   tool-result messages while keeping their existence in the transcript. We
///   keep the N most recent tool results intact and replace older bodies with
///   a tiny stub. `0` disables masking.
///
/// - `strip_text_turn_reasoning` — drop prior-turn reasoning on assistant
///   messages that did NOT call a tool. Mirrors DeepSeek's documented contract
///   (`reasoning_content` is *ignored* on non-tool turns when sent, mandatory
///   when adjacent to a tool call) and Anthropic's default behavior on Sonnet
///   ≤4.5 / Haiku via `clear_thinking_20251015`. Also strips inline
///   `<think>...</think>` regions from `content` (MiniMax-M3 emits its
///   reasoning that way, inline in the visible content stream — without this
///   strip every prior turn's chain-of-thought is replayed verbatim and the
///   input prompt grows monotonically).
#[derive(Clone, Copy, Debug)]
pub struct HistoryPolicy {
    pub keep_live_tool_outputs: usize,
    pub strip_text_turn_reasoning: bool,
}

impl HistoryPolicy {
    /// The literature-recommended defaults: `keep_live_tool_outputs = 5`,
    /// `strip_text_turn_reasoning = true`. Used by [`HistoryPolicy::default`]
    /// when the env override is absent or anything other than `off`.
    pub fn enabled_defaults() -> Self {
        Self {
            keep_live_tool_outputs: 5,
            strip_text_turn_reasoning: true,
        }
    }

    /// An identity policy — both levers off; [`prep_outbound_history`] becomes
    /// a `to_vec()` of the input. Used by [`HistoryPolicy::default`] when
    /// `IGNIS_HISTORY_TRIM=off` is set in the process environment, for A/B
    /// benchmarking the trim's impact without rebuilding the binary.
    pub fn disabled() -> Self {
        Self {
            keep_live_tool_outputs: 0,
            strip_text_turn_reasoning: false,
        }
    }
}

impl Default for HistoryPolicy {
    /// `enabled_defaults()` by default; flipped to `disabled()` when the
    /// process env carries `IGNIS_HISTORY_TRIM=off`. Read once per call so a
    /// single shell can run masked-vs-unmasked trials by toggling the env in
    /// between, without restarting any long-lived process.
    fn default() -> Self {
        match std::env::var("IGNIS_HISTORY_TRIM").as_deref() {
            Ok("off") => Self::disabled(),
            _ => Self::enabled_defaults(),
        }
    }
}

/// Stub body that replaces a masked tool result. Kept short so the savings
/// dominate; includes the tool name when known so the model still has cheap
/// context about *what* was elided (a `bash` vs a `read_file` matters even
/// when the body is gone).
fn masked_tool_stub(tool_name: Option<&str>) -> String {
    match tool_name {
        Some(n) if !n.is_empty() => format!("[{n} output elided to save context]"),
        _ => "[older tool output elided to save context]".to_string(),
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
/// the two cost-reduction levers in [`HistoryPolicy`]. Returns a fresh `Vec` —
/// the caller's history is never mutated. Identity transform on an empty
/// policy / empty history.
///
/// Invariants we preserve:
/// - Message ordering and count are unchanged. Only individual `content` /
///   `reasoning_content` fields are blanked or stripped.
/// - `tool_call_id` and `tool_calls` are never touched, so the
///   call→result linkage that providers (especially Anthropic) validate
///   remains intact.
/// - An assistant turn that *did* call a tool keeps its reasoning verbatim —
///   DeepSeek 400s otherwise, and Anthropic requires intra-turn thinking
///   adjacency to its `tool_use` block.
pub(crate) fn prep_outbound_history(messages: &[Message], policy: &HistoryPolicy) -> Vec<Message> {
    let mut out: Vec<Message> = messages.to_vec();

    if policy.strip_text_turn_reasoning {
        for msg in out.iter_mut() {
            if msg.role != "assistant" {
                continue;
            }
            // Tool-calling assistant turns: keep reasoning intact.
            if msg.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty()) {
                continue;
            }
            // Text-only assistant turn: drop the dedicated reasoning slot AND
            // any inline `<think>...</think>` the content carried (MM3).
            msg.reasoning_content = None;
            if let Some(content) = msg.content.as_ref() {
                if content.contains("<think>") {
                    msg.content = Some(strip_think_blocks(content));
                }
            }
        }
    }

    if policy.keep_live_tool_outputs > 0 {
        let tool_idxs: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "tool")
            .map(|(i, _)| i)
            .collect();
        if tool_idxs.len() > policy.keep_live_tool_outputs {
            let cutoff = tool_idxs.len() - policy.keep_live_tool_outputs;
            for &idx in &tool_idxs[..cutoff] {
                let name = out[idx].name.clone();
                out[idx].content = Some(masked_tool_stub(name.as_deref()));
            }
        }
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
    let res = req.send().await?;

    if !res.status().is_success() {
        let error_text = res
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(anyhow::anyhow!("LLM API returned error: {}", error_text));
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
    fn prep_history_masks_stale_tool_outputs_keeps_latest_n() {
        // Eight tool results, keep_live = 3 → first five are masked, last three intact.
        let mut history = Vec::new();
        history.push(user("do many things"));
        for i in 0..8 {
            history.push(assistant_calling("", None, &format!("call_{i}")));
            history.push(tool_result(
                &format!("call_{i}"),
                "bash",
                &format!("result body number {i}"),
            ));
        }
        let policy = HistoryPolicy {
            keep_live_tool_outputs: 3,
            strip_text_turn_reasoning: false,
        };
        let out = prep_outbound_history(&history, &policy);
        let tool_msgs: Vec<&Message> = out.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(tool_msgs.len(), 8);
        // First five tool results are masked (stub includes the tool name).
        for (i, m) in tool_msgs.iter().take(5).enumerate() {
            let body = m.content.as_deref().unwrap_or("");
            assert!(
                body.contains("bash") && body.contains("elided"),
                "tool result #{i} should be masked, got: {body:?}"
            );
            // tool_call_id linkage stays intact — that's the hard invariant.
            assert_eq!(
                m.tool_call_id.as_deref(),
                Some(format!("call_{i}").as_str())
            );
        }
        // Last three tool results keep their real bodies.
        for (offset, m) in tool_msgs.iter().skip(5).enumerate() {
            let i = 5 + offset;
            let expected = format!("result body number {i}");
            assert_eq!(m.content.as_deref(), Some(expected.as_str()));
        }
    }

    #[test]
    fn prep_history_strips_reasoning_on_text_only_assistant_turn() {
        let history = vec![
            user("question"),
            assistant_text("the answer is 42", Some("first I considered ...")),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy::default());
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
        let out = prep_outbound_history(&history, &HistoryPolicy::default());
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
        let out = prep_outbound_history(&history, &HistoryPolicy::default());
        assert_eq!(out[1].content.as_deref(), Some("here is my answer"));
    }

    #[test]
    fn prep_history_strips_multiple_think_blocks_on_text_turn() {
        let history = vec![
            user("hi"),
            assistant_text("<think>A</think>x<think>B\nC</think>y", None),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy::default());
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
        let out = prep_outbound_history(&history, &HistoryPolicy::default());
        assert_eq!(
            out[1].content.as_deref(),
            Some("<think>plan</think>running ls")
        );
    }

    #[test]
    fn prep_history_with_strip_disabled_preserves_reasoning_and_think_tags() {
        let policy = HistoryPolicy {
            keep_live_tool_outputs: 0,
            strip_text_turn_reasoning: false,
        };
        let history = vec![
            user("hi"),
            assistant_text("<think>raw</think>answer", Some("kept reasoning")),
        ];
        let out = prep_outbound_history(&history, &policy);
        assert_eq!(out[1].reasoning_content.as_deref(), Some("kept reasoning"));
        assert_eq!(out[1].content.as_deref(), Some("<think>raw</think>answer"));
    }

    #[test]
    fn prep_history_masking_disabled_keeps_all_tool_bodies() {
        let policy = HistoryPolicy {
            keep_live_tool_outputs: 0,
            strip_text_turn_reasoning: false,
        };
        let mut history = vec![user("go")];
        for i in 0..5 {
            history.push(assistant_calling("", None, &format!("c{i}")));
            history.push(tool_result(&format!("c{i}"), "bash", &format!("body {i}")));
        }
        let out = prep_outbound_history(&history, &policy);
        for (i, m) in out.iter().filter(|m| m.role == "tool").enumerate() {
            let expected = format!("body {i}");
            assert_eq!(m.content.as_deref(), Some(expected.as_str()));
        }
    }

    #[test]
    fn prep_history_unclosed_think_tag_preserved_on_text_turn() {
        // Safe-degrade: if a `<think>` was never closed (stream truncation),
        // keep the tail intact rather than dropping the rest of the content.
        let history = vec![
            user("hi"),
            assistant_text("ok<think>truncated and no closer", None),
        ];
        let out = prep_outbound_history(&history, &HistoryPolicy::default());
        assert_eq!(
            out[1].content.as_deref(),
            Some("ok<think>truncated and no closer")
        );
    }

    #[test]
    fn prep_history_identity_on_empty_input() {
        assert!(prep_outbound_history(&[], &HistoryPolicy::default()).is_empty());
    }

    /// Env-var override read by `HistoryPolicy::default`. Uses a `Mutex` to
    /// serialize the test (process env is global) and a guard to restore the
    /// prior value so an aborted test doesn't poison sibling tests in the same
    /// binary.
    #[test]
    fn history_policy_default_honors_env_off_switch() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let prior = std::env::var("IGNIS_HISTORY_TRIM").ok();

        // SAFETY: env mutation; the static Mutex above serializes every test
        // in this module that touches IGNIS_HISTORY_TRIM, so no concurrent
        // get/set in this process can race.
        unsafe { std::env::set_var("IGNIS_HISTORY_TRIM", "off") };
        let p = HistoryPolicy::default();
        assert_eq!(p.keep_live_tool_outputs, 0);
        assert!(!p.strip_text_turn_reasoning);

        unsafe { std::env::set_var("IGNIS_HISTORY_TRIM", "on") };
        let p = HistoryPolicy::default();
        assert_eq!(p.keep_live_tool_outputs, 5);
        assert!(p.strip_text_turn_reasoning);

        unsafe { std::env::remove_var("IGNIS_HISTORY_TRIM") };
        let p = HistoryPolicy::default();
        assert_eq!(p.keep_live_tool_outputs, 5);
        assert!(p.strip_text_turn_reasoning);

        // Restore the prior value, if any.
        match prior {
            Some(v) => unsafe { std::env::set_var("IGNIS_HISTORY_TRIM", v) },
            None => unsafe { std::env::remove_var("IGNIS_HISTORY_TRIM") },
        }
    }
}
