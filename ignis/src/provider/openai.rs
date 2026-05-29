use super::{openai_compatible_chat_stream, LlmProvider, LlmResponseDelta};
use crate::Message;
use async_trait::async_trait;
use futures_util::stream::BoxStream;

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
    model: String,
    user_agent: String,
    reasoning_effort: Option<String>,
    /// Provider label for telemetry attribution. The same struct is used for
    /// "openai", "kimi-code", and "Moonshot Platform CN" — each construction
    /// site passes its canonical name.
    provider_name: String,
}

impl OpenAiProvider {
    pub fn new(
        provider_name: impl Into<String>,
        api_key: String,
        api_url: String,
        model: String,
        user_agent: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            api_url,
            model,
            user_agent: user_agent.unwrap_or_else(|| "ignis/0.1.0".to_string()),
            reasoning_effort,
            provider_name: provider_name.into(),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
        openai_compatible_chat_stream(
            &self.client,
            &self.api_url,
            &self.api_key,
            &self.model,
            self.reasoning_effort.as_deref(),
            Some(&self.user_agent),
            system_prompt,
            messages,
            tools,
        )
        .await
    }
}
