use super::{
    bytes_to_lines, prep_outbound_history, Auth, HistoryPolicy, LlmProvider, LlmResponseDelta,
    Resolved, Usage,
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
    MessageStart { message: AnthropicMessageStart },
    #[serde(rename = "message_delta")]
    MessageDelta { usage: AnthropicUsage },
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
struct AnthropicMessageStart {
    #[serde(default)]
    usage: AnthropicUsage,
}

/// Anthropic streams the input-side counts on `message_start` and the final
/// cumulative output count on `message_delta`. `input_tokens` here excludes
/// cached tokens (reported separately), so we fold the cache fields back in to
/// match the OpenAI-compatible convention where `input_tokens` is the full
/// prompt size and `total()` = input + output.
#[derive(Deserialize, Debug, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
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

        let anthropic_messages = map_messages_to_anthropic(&trimmed);

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

/// Translate a session history (OpenAI-shaped `Message`s) into the
/// Anthropic-Messages wire shape (`{role, content: [...blocks]}`).
///
/// Extracted from `chat_stream` so the mapping — and the two edge cases
/// Anthropic 400s on — can be unit-tested without standing up an HTTP server.
///
/// The two 400s this guards against:
///   * `tool_use.input` must be a JSON object. Empty / unparseable /
///     non-object `arguments` strings fall back to `{}`.
///   * Assistant messages must have a non-empty `content` array AND must
///     alternate with user messages. Turns whose reasoning was stripped by
///     `prep_outbound_history` (leaving `content = None`, `tool_calls = None`)
///     get a single-space text block as a placeholder so the message stays
///     present and alternation is preserved — dropping the message entirely
///     would produce adjacent `user` messages and trigger a different
///     "messages must alternate" 400.
fn map_messages_to_anthropic(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": msg.content.clone().unwrap_or_default()
                }));
            }
            "assistant" => {
                let mut content_blocks = Vec::new();
                if let Some(text) = &msg.content {
                    if !text.is_empty() {
                        content_blocks.push(serde_json::json!({
                            "type": "text",
                            "text": text
                        }));
                    }
                }
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        // Anthropic's `tool_use.input` must be a JSON object —
                        // `null`, arrays, or primitives are all rejected with
                        // `Input should be a valid dictionary`. The accumulator
                        // can leave `arguments` empty (no `input_json_delta`
                        // chunks) or unparseable; in either case fall back to
                        // `{}` so the request stays valid.
                        let parsed = serde_json::from_str(&tc.function.arguments)
                            .ok()
                            .filter(|v: &serde_json::Value| v.is_object());
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id.clone(),
                            "name": tc.function.name.clone(),
                            "input": parsed.unwrap_or_else(|| serde_json::json!({}))
                        }));
                    }
                }
                if content_blocks.is_empty() {
                    // Reasoning-only turn whose reasoning was stripped.
                    // Anthropic rejects `{"content":[]}` AND requires
                    // user/assistant alternation, so we can't drop the
                    // message — emit a single-space text block as the
                    // minimal valid payload. The model still sees the
                    // turn boundary, and the placeholder text is
                    // whitespace so it doesn't poison the context.
                    content_blocks.push(serde_json::json!({
                        "type": "text",
                        "text": " "
                    }));
                }
                out.push(serde_json::json!({
                    "role": "assistant",
                    "content": content_blocks
                }));
            }
            "tool" => {
                let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                let content_str = msg.content.clone().unwrap_or_default();
                out.push(serde_json::json!({
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
    out
}

#[derive(Default)]
struct ParserState {
    /// Per-content-block: the tool-call id + name announced by `content_block_start`,
    /// so we can drop them from subsequent `input_json_delta` events (the agent
    /// accumulator `push_str`s `Some(name)` per delta — emitting them on every
    /// chunk produced `bashbashbash`-style tripled names).
    active_tool_calls: HashMap<usize, (String, String)>,
    /// Input-side token counts captured from `message_start`, merged with the
    /// `message_delta` output count into a single `Usage` delta (#175). Emitting
    /// once avoids the agent's `Usage::add` double-counting the input side.
    input_usage: Option<Usage>,
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
        AnthropicEvent::MessageStart { message } => {
            // Stash the input side; the output count arrives on message_delta.
            // `input_tokens` excludes cache, so fold cache read+creation back in
            // (the OpenAI path's `input_tokens` already includes cached tokens).
            let u = &message.usage;
            state.input_usage = Some(Usage {
                input_tokens: u.input_tokens
                    + u.cache_read_input_tokens
                    + u.cache_creation_input_tokens,
                output_tokens: 0,
                reasoning_tokens: 0,
                cache_read_tokens: u.cache_read_input_tokens,
                cache_write_tokens: u.cache_creation_input_tokens,
            });
            None
        }
        AnthropicEvent::MessageDelta { usage } => {
            // One Usage delta for the whole turn: input from message_start +
            // the final cumulative output count here.
            let mut combined = state.input_usage.take().unwrap_or_default();
            combined.output_tokens = usage.output_tokens;
            Some(Ok(LlmResponseDelta::Usage(combined)))
        }
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
    use crate::{ToolCall, ToolCallFunction};

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
    fn usage_is_parsed_from_message_start_and_delta() {
        // Regression for #175: the Anthropic protocol dropped usage entirely
        // (`_ => None`), so the context meter + telemetry read zero on the
        // default provider. Input arrives on message_start (cache split out),
        // the final output on message_delta — emit exactly one Usage delta.
        let mut state = ParserState::default();
        let sse = [
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","usage":{"input_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":1}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
        ];
        let deltas: Vec<_> = sse
            .iter()
            .filter_map(|l| parse_line(&mut state, l))
            .map(|r| r.unwrap())
            .collect();

        let usages: Vec<&Usage> = deltas
            .iter()
            .filter_map(|d| match d {
                LlmResponseDelta::Usage(u) => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 1, "exactly one Usage delta per turn");
        let u = usages[0];
        assert_eq!(u.input_tokens, 125, "input folds in cache read + creation");
        assert_eq!(u.output_tokens, 42);
        assert_eq!(u.cache_read_tokens, 20);
        assert_eq!(u.cache_write_tokens, 5);
        assert_eq!(u.total(), 167);
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

    // ---- map_messages_to_anthropic: outbound history shape ----
    //
    // Two regressions these cover, both reproducing 400s the user hit in
    // production:
    //   * `tool_use.input` must be a JSON object — empty / unparseable /
    //     non-object `arguments` strings used to fall through as `null`
    //     (`Input should be a valid dictionary (2013)` at the offending
    //     `messages.<i>.content.<j>.tool_use.input` path).
    //   * Assistant messages with no visible content blocks (e.g. a turn that
    //     produced only reasoning, which `prep_outbound_history` then strips)
    //     used to serialize as `{"role":"assistant","content":[]}` and the
    //     API returned `missing messages.content parameter`.

    fn user_msg(text: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: Some(text.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }
    }

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            r#type: "function".to_string(),
            function: ToolCallFunction {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    fn tool_result_msg(call_id: &str, body: &str) -> Message {
        Message {
            role: "tool".to_string(),
            content: Some(body.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: Some(call_id.to_string()),
            tool_calls: None,
            created_at_ms: None,
        }
    }

    #[test]
    fn tool_use_input_falls_back_to_empty_object_for_empty_arguments() {
        // The accumulator leaves `arguments` empty when the model streams a
        // `content_block_start` for a tool_use but no `input_json_delta`
        // chunks. Anthropic rejects `input: null`; we must send `{}`.
        let history = vec![
            user_msg("hi"),
            Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![tool_call("toolu_1", "bash", "")]),
                created_at_ms: None,
            },
        ];
        let out = map_messages_to_anthropic(&history);
        let assistant = &out[1];
        let input = &assistant["content"][0]["input"];
        assert_eq!(
            input,
            &serde_json::json!({}),
            "empty arguments must default to empty object, not null"
        );
    }

    #[test]
    fn tool_use_input_falls_back_to_empty_object_for_invalid_json() {
        // Defensive: a partial / corrupt `arguments` string would also have
        // produced `input: null` under the old `unwrap_or(Value::Null)`.
        let history = vec![Message {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![tool_call("toolu_1", "bash", "{not json")]),
            created_at_ms: None,
        }];
        let out = map_messages_to_anthropic(&history);
        let input = &out[0]["content"][0]["input"];
        assert_eq!(input, &serde_json::json!({}));
    }

    #[test]
    fn tool_use_input_rejects_non_object_values() {
        // Anthropic's contract is stricter than "any JSON value" — `input`
        // must be an object, not an array/string/number. A non-object payload
        // (e.g. a model that emitted a bare array) would also be rejected;
        // normalize to `{}` so the request goes through.
        for raw in [r#""just a string""#, r#"[1,2,3]"#, r#"42"#, r#"true"#] {
            let history = vec![Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![tool_call("toolu_1", "bash", raw)]),
                created_at_ms: None,
            }];
            let out = map_messages_to_anthropic(&history);
            assert_eq!(
                out[0]["content"][0]["input"],
                serde_json::json!({}),
                "non-object {raw} must normalize to empty object"
            );
        }
    }

    #[test]
    fn tool_use_input_preserves_valid_object_arguments() {
        // Sanity: a well-formed object payload passes through unchanged.
        let history = vec![Message {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![tool_call("toolu_1", "bash", r#"{"command":"ls"}"#)]),
            created_at_ms: None,
        }];
        let out = map_messages_to_anthropic(&history);
        assert_eq!(
            out[0]["content"][0]["input"],
            serde_json::json!({"command": "ls"})
        );
    }

    #[test]
    fn assistant_message_with_no_content_blocks_emits_placeholder_text() {
        // Reproduces the `missing messages.content parameter` 400: a turn that
        // produced only reasoning (which the history-prep layer strips) arrives
        // here with `content = None` and `tool_calls = None`. We can't drop the
        // message either — Anthropic requires user/assistant alternation and
        // would 400 on the resulting adjacent-user shape. Send a single-space
        // text block as the minimal valid placeholder that preserves the
        // turn boundary without polluting the context.
        let history = vec![
            user_msg("hi"),
            Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
            user_msg("hello?"),
        ];
        let out = map_messages_to_anthropic(&history);
        assert_eq!(out.len(), 3, "all three messages must be preserved");
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[2]["role"], "user");
        let content = out[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "single placeholder text block");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], " ");
    }

    #[test]
    fn assistant_message_with_only_empty_text_emits_placeholder_text() {
        // `prep_outbound_history` can leave `content = Some("")` on a turn
        // whose only payload was a `<think>...</think>` block. The placeholder
        // path is the same as for `content = None`: a single-space text
        // block keeps the message and the alternation intact.
        let history = vec![
            user_msg("hi"),
            Message {
                role: "assistant".to_string(),
                content: Some(String::new()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        ];
        let out = map_messages_to_anthropic(&history);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["role"], "assistant");
        let content = out[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], " ");
    }

    #[test]
    fn assistant_message_with_empty_text_and_tool_calls_keeps_only_tool_use() {
        // Empty text + real tool calls: the tool_use blocks satisfy the
        // "non-empty content" rule, and we should not bolt on a placeholder
        // text block that would push the tool_use out of the first position
        // Anthropic's streaming parser expects.
        let history = vec![Message {
            role: "assistant".to_string(),
            content: Some(String::new()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![tool_call("toolu_1", "bash", r#"{"command":"ls"}"#)]),
            created_at_ms: None,
        }];
        let out = map_messages_to_anthropic(&history);
        assert_eq!(out.len(), 1);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "only the tool_use block is sent");
        assert_eq!(content[0]["type"], "tool_use");
    }

    #[test]
    fn tool_result_message_wraps_in_user_role() {
        // Sanity: the tool → user-role conversion is unchanged by the new
        // guards; we want to make sure regressions here don't silently
        // break the call→result linkage Anthropic validates.
        let history = vec![
            user_msg("please ls"),
            Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![tool_call("toolu_1", "bash", r#"{"command":"ls"}"#)]),
                created_at_ms: None,
            },
            tool_result_msg("toolu_1", "app/  README.md"),
        ];
        let out = map_messages_to_anthropic(&history);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["type"], "tool_result");
        assert_eq!(out[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(out[2]["content"][0]["content"], "app/  README.md");
    }
}
