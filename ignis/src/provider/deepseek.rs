use super::{openai_compatible_chat_stream, LlmProvider, LlmResponseDelta};
use crate::Message;
use async_trait::async_trait;
use futures_util::stream::BoxStream;

pub struct DeepSeekProvider {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
    model: String,
    reasoning_effort: Option<String>,
}

impl DeepSeekProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self::with_url(
            api_key,
            "https://api.deepseek.com/v1".to_string(),
            model,
            None,
        )
    }
    pub fn with_url(
        api_key: String,
        api_url: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            api_url,
            model,
            reasoning_effort,
        }
    }
}

#[async_trait]
impl LlmProvider for DeepSeekProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "deepseek"
    }

    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
        // DeepSeek is OpenAI-compatible; it differs from `OpenAiProvider` only in
        // omitting the `User-Agent` header (hence `None`).
        openai_compatible_chat_stream(
            &self.client,
            &self.api_url,
            &self.api_key,
            &self.model,
            self.reasoning_effort.as_deref(),
            None,
            system_prompt,
            messages,
            tools,
        )
        .await
    }
}
