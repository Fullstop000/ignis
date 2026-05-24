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
}

fn is_empty_slice(slice: &[serde_json::Value]) -> bool {
    slice.is_empty()
}

#[derive(Deserialize, Debug)]
pub struct Chunk {
    pub choices: Option<Vec<ChunkChoice>>,
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
