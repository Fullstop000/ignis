use super::{bytes_to_lines, LlmProvider, LlmResponseDelta};
use crate::Message;
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};

pub struct OllamaProvider {
    client: reqwest::Client,
    model: String,
    api_url: String,
}

impl OllamaProvider {
    pub fn new(api_url: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_url,
            model,
        }
    }
}

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
}

#[derive(Deserialize, Debug)]
struct OllamaResponse {
    message: Option<OllamaMessage>,
}

#[derive(Deserialize, Debug)]
struct OllamaMessage {
    content: String,
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "ollama"
    }

    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        _tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
        let mut ollama_messages = vec![Message {
            role: "system".to_string(),
            content: Some(system_prompt.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        ollama_messages.extend_from_slice(messages);

        let req_body = OllamaRequest {
            model: self.model.clone(),
            messages: ollama_messages,
            stream: true,
        };

        let endpoint = format!("{}/api/chat", self.api_url.trim_end_matches('/'));

        let res = self.client.post(&endpoint).json(&req_body).send().await?;

        if !res.status().is_success() {
            let error_text = res
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!("Ollama API returned error: {}", error_text));
        }

        let byte_stream = res.bytes_stream();
        let line_stream = bytes_to_lines(byte_stream);

        let delta_stream = line_stream.filter_map(|line_result| async move {
            match line_result {
                Err(err) => Some(Err(err)),
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        return None;
                    }
                    match serde_json::from_str::<OllamaResponse>(line) {
                        Err(_) => None,
                        Ok(resp) => {
                            if let Some(msg) = resp.message {
                                if !msg.content.is_empty() {
                                    return Some(Ok(LlmResponseDelta::Text(msg.content)));
                                }
                            }
                            None
                        }
                    }
                }
            }
        });

        Ok(delta_stream.boxed())
    }
}
