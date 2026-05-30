use super::{openai_compatible_chat_stream, LlmProvider, LlmResponseDelta, Resolved};
use crate::Message;
use async_trait::async_trait;
use futures_util::stream::BoxStream;

/// Any OpenAI-compatible endpoint — OpenAI, DeepSeek, Kimi, Moonshot, MiniMax's
/// `/v1`, or a user's `custom` endpoint. The only per-brand knobs are the base
/// URL, optional request headers (some plans whitelist a `User-Agent`), and
/// reasoning effort; all response parsing is shared via
/// [`openai_compatible_chat_stream`].
pub struct OpenAiCompatible {
    client: reqwest::Client,
    provider_id: String,
    api_key: String,
    base_url: String,
    model: String,
    request_headers: Vec<(String, String)>,
    reasoning_effort: Option<String>,
}

impl OpenAiCompatible {
    pub fn new(r: Resolved) -> Self {
        Self {
            client: reqwest::Client::new(),
            provider_id: r.provider_id,
            api_key: r.api_key.unwrap_or_default(),
            base_url: r.base_url,
            model: r.model,
            request_headers: r.request_headers,
            reasoning_effort: r.reasoning_effort,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatible {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        &self.provider_id
    }

    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
        openai_compatible_chat_stream(
            &self.client,
            &self.base_url,
            &self.api_key,
            &self.model,
            self.reasoning_effort.as_deref(),
            &self.request_headers,
            system_prompt,
            messages,
            tools,
        )
        .await
    }
}
