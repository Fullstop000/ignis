use crate::Message;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};

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
    Usage(crate::types::Usage),
}

#[async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error>;
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
/// `prompt_cache_hit_tokens`.
#[derive(Deserialize, Debug)]
pub struct ChunkUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
}

#[derive(Deserialize, Debug)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

impl ChunkUsage {
    pub fn to_usage(&self) -> crate::types::Usage {
        let cache_read = self
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
            .unwrap_or(0);
        crate::types::Usage {
            // prompt_tokens already includes cached tokens.
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
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
}
