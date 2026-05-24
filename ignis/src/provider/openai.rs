use super::{
    bytes_to_lines, ChatCompletionsRequest, Chunk, LlmProvider, LlmResponseDelta, StreamOptions,
};
use crate::Message;
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
    model: String,
    user_agent: String,
    reasoning_effort: Option<String>,
}

impl OpenAiProvider {
    pub fn new(
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
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_stream(
        &self,
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
            model: &self.model,
            messages: request_messages,
            tools,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            reasoning_effort: self.reasoning_effort.as_deref(),
        };

        let endpoint = if self.api_url.ends_with("/chat/completions") {
            self.api_url.clone()
        } else {
            format!("{}/chat/completions", self.api_url.trim_end_matches('/'))
        };

        let res = self
            .client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("User-Agent", &self.user_agent)
            .json(&req_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let error_text = res
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!("LLM API returned error: {}", error_text));
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
                    let data_part = match line.strip_prefix("data:") {
                        Some(d) => d.trim(),
                        None => return None,
                    };
                    if data_part == "[DONE]" {
                        return None;
                    }
                    match serde_json::from_str::<Chunk>(data_part) {
                        Err(_) => None,
                        Ok(chunk) => {
                            if let Some(choices) = &chunk.choices {
                                if let Some(choice) = choices.first() {
                                    if let Some(content) = &choice.delta.content {
                                        if !content.is_empty() {
                                            return Some(Ok(LlmResponseDelta::Text(
                                                content.clone(),
                                            )));
                                        }
                                    }
                                    if let Some(reasoning) = &choice.delta.reasoning_content {
                                        if !reasoning.is_empty() {
                                            return Some(Ok(LlmResponseDelta::Reasoning(
                                                reasoning.clone(),
                                            )));
                                        }
                                    }
                                    if let Some(tool_calls) = &choice.delta.tool_calls {
                                        if let Some(tc) = tool_calls.first() {
                                            let name =
                                                tc.function.as_ref().and_then(|f| f.name.clone());
                                            let args = tc
                                                .function
                                                .as_ref()
                                                .and_then(|f| f.arguments.clone())
                                                .unwrap_or_default();
                                            return Some(Ok(LlmResponseDelta::ToolCall {
                                                index: tc.index,
                                                id: tc.id.clone(),
                                                name,
                                                arguments: args,
                                            }));
                                        }
                                    }
                                }
                            }
                            if let Some(u) = &chunk.usage {
                                return Some(Ok(LlmResponseDelta::Usage(u.to_usage())));
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
