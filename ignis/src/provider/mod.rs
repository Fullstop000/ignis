use async_trait::async_trait;
use futures_util::stream::{BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};

mod message;
pub use message::{Message, ToolCall, ToolCallFunction, Usage};

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
mod deepseek;
mod gemini;
mod ollama;
mod openai;

pub use anthropic::AnthropicProvider;
pub use deepseek::DeepSeekProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;

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

/// Stream a chat completion from an OpenAI-compatible endpoint. The only
/// provider-specific knob is `user_agent`: `Some` sets the header (OpenAI/Kimi/
/// Moonshot), `None` omits it (DeepSeek). All response parsing is shared via
/// `parse_sse_line`, so a streaming-parser change happens in exactly one place.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn openai_compatible_chat_stream(
    client: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    model: &str,
    reasoning_effort: Option<&str>,
    user_agent: Option<&str>,
    system_prompt: &str,
    messages: &[Message],
    tools: &[serde_json::Value],
) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
    let mut request_messages = vec![Message {
        role: "system".to_string(),
        content: Some(system_prompt.to_string()),
        reasoning_content: None,
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];
    request_messages.extend_from_slice(messages);

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
    if let Some(ua) = user_agent {
        req = req.header("User-Agent", ua);
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
}
