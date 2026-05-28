use super::{bytes_to_lines, LlmProvider, LlmResponseDelta};
use crate::Message;
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
        }
    }
}

#[derive(Serialize)]
struct AnthropicMessagesRequest {
    model: String,
    system: String,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum AnthropicEvent {
    #[serde(rename = "message_start")]
    MessageStart,
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: AnthropicDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
#[allow(dead_code)]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum AnthropicDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "anthropic"
    }

    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
        // Map OpenAI format tools to Anthropic format:
        // OpenAI: { "type": "function", "function": { "name": "...", "description": "...", "parameters": { ... } } }
        // Anthropic: { "name": "...", "description": "...", "input_schema": { ... } }
        let anthropic_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|ot| {
                let func = &ot["function"];
                serde_json::json!({
                    "name": func["name"].as_str().unwrap_or_default(),
                    "description": func["description"].as_str().unwrap_or_default(),
                    "input_schema": func["parameters"].clone()
                })
            })
            .collect();

        // Map messages
        let mut anthropic_messages = Vec::new();
        for msg in messages {
            match msg.role.as_str() {
                "user" => {
                    anthropic_messages.push(serde_json::json!({
                        "role": "user",
                        "content": msg.content.clone().unwrap_or_default()
                    }));
                }
                "assistant" => {
                    let mut content_blocks = Vec::new();
                    if let Some(text) = &msg.content {
                        content_blocks.push(serde_json::json!({
                            "type": "text",
                            "text": text
                        }));
                    }
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tc in tool_calls {
                            let args: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::Value::Null);
                            content_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id.clone(),
                                "name": tc.function.name.clone(),
                                "input": args
                            }));
                        }
                    }
                    anthropic_messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": content_blocks
                    }));
                }
                "tool" => {
                    let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                    let content_str = msg.content.clone().unwrap_or_default();
                    anthropic_messages.push(serde_json::json!({
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": tool_use_id,
                                "content": content_str
                            }
                        ]
                    }));
                }
                _ => {}
            }
        }

        let req_body = AnthropicMessagesRequest {
            model: self.model.clone(),
            system: system_prompt.to_string(),
            messages: anthropic_messages,
            tools: anthropic_tools,
            stream: true,
        };

        let res = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&req_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let error_text = res
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!("Anthropic API returned error: {}", error_text));
        }

        let byte_stream = res.bytes_stream();
        let line_stream = bytes_to_lines(byte_stream);

        struct ParserState {
            active_tool_calls: HashMap<usize, (String, String)>,
            current_event_type: String,
        }

        let state = std::sync::Arc::new(tokio::sync::Mutex::new(ParserState {
            active_tool_calls: HashMap::new(),
            current_event_type: String::new(),
        }));

        let state_clone = state.clone();
        let delta_stream = line_stream.filter_map(move |line_result| {
            let state = state_clone.clone();
            async move {
                match line_result {
                    Err(err) => Some(Err(err)),
                    Ok(line) => {
                        let line = line.trim();
                        if line.is_empty() {
                            return None;
                        }

                        let mut state_lock = state.lock().await;

                        if let Some(stripped) = line.strip_prefix("event:") {
                            state_lock.current_event_type = stripped.trim().to_string();
                            return None;
                        }
                        let data_part = match line.strip_prefix("data:") {
                            Some(d) => d.trim(),
                            None => return None,
                        };

                        match serde_json::from_str::<AnthropicEvent>(data_part) {
                            Err(_) => None,
                            Ok(event) => match event {
                                AnthropicEvent::ContentBlockStart {
                                    index,
                                    content_block,
                                } => {
                                    if let AnthropicContentBlock::ToolUse { id, name } =
                                        content_block
                                    {
                                        state_lock
                                            .active_tool_calls
                                            .insert(index, (id.clone(), name.clone()));
                                        return Some(Ok(LlmResponseDelta::ToolCall {
                                            index,
                                            id: Some(id),
                                            name: Some(name),
                                            arguments: String::new(),
                                        }));
                                    }
                                    None
                                }
                                AnthropicEvent::ContentBlockDelta { index, delta } => match delta {
                                    AnthropicDelta::TextDelta { text } => {
                                        Some(Ok(LlmResponseDelta::Text(text)))
                                    }
                                    AnthropicDelta::InputJsonDelta { partial_json } => {
                                        let (id, name) = state_lock
                                            .active_tool_calls
                                            .get(&index)
                                            .cloned()
                                            .unwrap_or_else(|| (String::new(), String::new()));
                                        Some(Ok(LlmResponseDelta::ToolCall {
                                            index,
                                            id: if id.is_empty() { None } else { Some(id) },
                                            name: if name.is_empty() { None } else { Some(name) },
                                            arguments: partial_json,
                                        }))
                                    }
                                },
                                _ => None,
                            },
                        }
                    }
                }
            }
        });

        Ok(delta_stream.boxed())
    }
}
