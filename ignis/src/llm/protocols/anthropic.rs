use super::{
    bytes_to_lines, prep_outbound_history, Auth, HistoryPolicy, LlmProvider, LlmResponseDelta,
    Resolved,
};
use crate::Message;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Any Anthropic-Messages-compatible endpoint — real Anthropic (`x-api-key`) or
/// MiniMax's `/anthropic` endpoint (`Bearer`). The base URL is the API root; we
/// append `/v1/messages`, and the auth header is chosen from [`Auth`].
pub struct AnthropicCompatible {
    client: reqwest::Client,
    provider_id: String,
    api_key: String,
    base_url: String,
    auth: Auth,
    model: String,
}

impl AnthropicCompatible {
    pub fn new(r: Resolved) -> Self {
        Self {
            client: reqwest::Client::new(),
            provider_id: r.provider_id,
            api_key: r.api_key.unwrap_or_default(),
            base_url: r.base_url,
            auth: r.auth,
            model: r.model,
        }
    }
}

/// Output-token cap sent on every request. Anthropic's Messages API requires
/// `max_tokens`; the value is a ceiling (the model stops earlier when it's
/// done), so we pick a value safely under every supported model's max output
/// (Sonnet 4.6 / Haiku 4.5 / Opus 4.6+ all allow ≥ 64k; MiniMax M2.7 accepts
/// the same range).
const DEFAULT_MAX_TOKENS: u64 = 32_768;

#[derive(Serialize)]
struct AnthropicMessagesRequest {
    model: String,
    system: String,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
    max_tokens: u64,
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
impl LlmProvider for AnthropicCompatible {
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

        // Apply the same context-trim policy the OpenAI-compat path uses
        // (observation masking on stale tool results; reasoning-strip is a
        // no-op here because the Anthropic mapping below doesn't carry
        // `reasoning_content` into the outbound payload).
        let trimmed = prep_outbound_history(messages, &HistoryPolicy::default());

        // Map messages
        let mut anthropic_messages = Vec::new();
        for msg in &trimmed {
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
            max_tokens: DEFAULT_MAX_TOKENS,
        };

        let endpoint = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut req = self
            .client
            .post(&endpoint)
            .header("anthropic-version", "2023-06-01");
        req = match self.auth {
            Auth::XApiKey => req.header("x-api-key", self.api_key.clone()),
            _ => req.header("Authorization", format!("Bearer {}", self.api_key)),
        };
        let res = super::send_with_timeout(req.json(&req_body)).await?;

        let status = res.status();
        if !status.is_success() {
            let error_text = res
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(super::LlmHttpError {
                status,
                body: error_text,
            }
            .into());
        }

        let byte_stream = res.bytes_stream();
        let line_stream = bytes_to_lines(byte_stream);

        let state = std::sync::Arc::new(tokio::sync::Mutex::new(ParserState::default()));

        let state_clone = state.clone();
        let delta_stream = line_stream.filter_map(move |line_result| {
            let state = state_clone.clone();
            async move {
                match line_result {
                    Err(err) => Some(Err(err)),
                    Ok(line) => parse_line(&mut *state.lock().await, line.trim()),
                }
            }
        });

        Ok(delta_stream.boxed())
    }
}

#[derive(Default)]
struct ParserState {
    /// Per-content-block: the tool-call id + name announced by `content_block_start`,
    /// so we can drop them from subsequent `input_json_delta` events (the agent
    /// accumulator `push_str`s `Some(name)` per delta — emitting them on every
    /// chunk produced `bashbashbash`-style tripled names).
    active_tool_calls: HashMap<usize, (String, String)>,
}

/// Translates one SSE line into at most one `LlmResponseDelta`.
///
/// Anthropic streams an `event: <type>` line followed by a `data: <json>` line.
/// We ignore the `event:` line — `data` already carries `"type"` for serde's tag.
fn parse_line(
    state: &mut ParserState,
    line: &str,
) -> Option<Result<LlmResponseDelta, anyhow::Error>> {
    if line.is_empty() || line.starts_with("event:") {
        return None;
    }
    let data_part = line.strip_prefix("data:")?.trim();
    let event: AnthropicEvent = serde_json::from_str(data_part).ok()?;
    match event {
        AnthropicEvent::ContentBlockStart {
            index,
            content_block: AnthropicContentBlock::ToolUse { id, name },
        } => {
            state
                .active_tool_calls
                .insert(index, (id.clone(), name.clone()));
            Some(Ok(LlmResponseDelta::ToolCall {
                index,
                id: Some(id),
                name: Some(name),
                arguments: String::new(),
            }))
        }
        AnthropicEvent::ContentBlockDelta {
            delta: AnthropicDelta::TextDelta { text },
            ..
        } => Some(Ok(LlmResponseDelta::Text(text))),
        AnthropicEvent::ContentBlockDelta {
            index,
            delta: AnthropicDelta::InputJsonDelta { partial_json },
        } => Some(Ok(LlmResponseDelta::ToolCall {
            index,
            // id + name were already emitted on ContentBlockStart and are
            // accumulated by the agent via push_str — re-sending them here
            // would triple the name (see active_tool_calls comment).
            id: None,
            name: None,
            arguments: partial_json,
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirror of the agent loop's tool-call accumulator (`agent/mod.rs` keeps
    /// `id`/`name`/`arguments` strings and `push_str`s every delta that arrives
    /// with `Some(...)`). Used to assert that the parser doesn't re-send `name`
    /// per chunk — if it does, the accumulated name comes out duplicated.
    fn accumulate(deltas: Vec<LlmResponseDelta>) -> (String, String, String) {
        let (mut id, mut name, mut args) = (String::new(), String::new(), String::new());
        for d in deltas {
            if let LlmResponseDelta::ToolCall {
                id: did,
                name: dname,
                arguments,
                ..
            } = d
            {
                if let Some(v) = did {
                    id.push_str(&v);
                }
                if let Some(v) = dname {
                    name.push_str(&v);
                }
                args.push_str(&arguments);
            }
        }
        (id, name, args)
    }

    #[test]
    fn tool_name_emitted_only_once_across_input_json_chunks() {
        // Regression: small bash calls produce a `content_block_start` followed
        // by N `input_json_delta` chunks. Before the fix, every chunk re-sent
        // `Some("bash")`, so the agent accumulator built `bashbashbash` — which
        // then showed in the tool-block header and the permission picker.
        let mut state = ParserState::default();
        let sse = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"bash"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":" -la /home/zht/ignis\"}"}}"#,
        ];
        let deltas: Vec<_> = sse
            .iter()
            .filter_map(|l| parse_line(&mut state, l))
            .map(|r| r.unwrap())
            .collect();
        let (id, name, args) = accumulate(deltas);
        assert_eq!(
            name, "bash",
            "tool name must not be duplicated across chunks"
        );
        assert_eq!(
            id, "toolu_01",
            "tool id must not be duplicated across chunks"
        );
        assert_eq!(args, r#"{"command":"ls -la /home/zht/ignis"}"#);
    }

    #[test]
    fn request_body_includes_max_tokens() {
        // Anthropic's POST /v1/messages requires `max_tokens`; without it the
        // API rejects the request before streaming. Guards against the field
        // being dropped from `AnthropicMessagesRequest` in future refactors.
        let body = AnthropicMessagesRequest {
            model: "claude-sonnet-4-6".into(),
            system: String::new(),
            messages: vec![],
            tools: vec![],
            stream: true,
            max_tokens: DEFAULT_MAX_TOKENS,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["max_tokens"], serde_json::json!(DEFAULT_MAX_TOKENS));
    }
}
