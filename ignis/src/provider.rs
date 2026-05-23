use crate::Message;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    ) -> Result<
        BoxStream<'static, Result<LlmResponseDelta, Box<dyn std::error::Error + Send + Sync>>>,
        Box<dyn std::error::Error + Send + Sync>,
    >;
}

fn is_empty_slice(slice: &[serde_json::Value]) -> bool {
    slice.is_empty()
}

// ==========================================
// 1. OpenAiProvider
// ==========================================

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
    model: String,
}

impl OpenAiProvider {
    pub fn new(api_key: String, api_url: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            api_url,
            model,
        }
    }
}

#[derive(Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "is_empty_slice")]
    tools: &'a [serde_json::Value],
    stream: bool,
}

#[derive(Deserialize, Debug)]
struct Chunk {
    choices: Option<Vec<ChunkChoice>>,
}

#[derive(Deserialize, Debug)]
struct ChunkChoice {
    delta: ChunkDelta,
}

#[derive(Deserialize, Debug)]
struct ChunkDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Deserialize, Debug)]
struct ChunkToolCall {
    index: usize,
    id: Option<String>,
    function: Option<ChunkFunction>,
}

#[derive(Deserialize, Debug)]
struct ChunkFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<
        BoxStream<'static, Result<LlmResponseDelta, Box<dyn std::error::Error + Send + Sync>>>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
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
            .header("User-Agent", "KimiCLI/1.44.0")
            .json(&req_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let error_text = res.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(format!("LLM API returned error: {}", error_text).into());
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
                    if !line.starts_with("data:") {
                        return None;
                    }
                    let data_part = line["data:".len()..].trim();
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
                                            return Some(Ok(LlmResponseDelta::Text(content.clone())));
                                        }
                                    }
                                    if let Some(reasoning) = &choice.delta.reasoning_content {
                                        if !reasoning.is_empty() {
                                            return Some(Ok(LlmResponseDelta::Reasoning(reasoning.clone())));
                                        }
                                    }
                                    if let Some(tool_calls) = &choice.delta.tool_calls {
                                        if let Some(tc) = tool_calls.first() {
                                            let name = tc.function.as_ref().and_then(|f| f.name.clone());
                                            let args = tc.function.as_ref().and_then(|f| f.arguments.clone()).unwrap_or_default();
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
                            None
                        }
                    }
                }
            }
        });

        Ok(delta_stream.boxed())
    }
}

// ==========================================
// 2. AnthropicProvider
// ==========================================

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
    ContentBlockDelta {
        index: usize,
        delta: AnthropicDelta,
    },
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
    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<
        BoxStream<'static, Result<LlmResponseDelta, Box<dyn std::error::Error + Send + Sync>>>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
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
                            let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
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
            let error_text = res.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(format!("Anthropic API returned error: {}", error_text).into());
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

                        if line.starts_with("event:") {
                            state_lock.current_event_type = line["event:".len()..].trim().to_string();
                            return None;
                        }
                        if !line.starts_with("data:") {
                            return None;
                        }
                        let data_part = line["data:".len()..].trim();

                        match serde_json::from_str::<AnthropicEvent>(data_part) {
                            Err(_) => None,
                            Ok(event) => match event {
                                AnthropicEvent::ContentBlockStart { index, content_block } => {
                                    if let AnthropicContentBlock::ToolUse { id, name } = content_block {
                                        state_lock.active_tool_calls.insert(index, (id.clone(), name.clone()));
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
                                        let (id, name) = state_lock.active_tool_calls
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

// ==========================================
// 3. GeminiProvider
// ==========================================

pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
        }
    }
}

#[derive(Serialize)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    system_instruction: GeminiInstruction,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct GeminiChunk {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Deserialize, Debug)]
struct GeminiCandidate {
    content: Option<GeminiChunkContent>,
}

#[derive(Deserialize, Debug)]
struct GeminiChunkContent {
    parts: Option<Vec<GeminiChunkPart>>,
}

#[derive(Deserialize, Debug)]
struct GeminiChunkPart {
    text: Option<String>,
    #[serde(rename = "functionCall")]
    function_call: Option<GeminiFunctionCall>,
}

#[derive(Deserialize, Debug)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<
        BoxStream<'static, Result<LlmResponseDelta, Box<dyn std::error::Error + Send + Sync>>>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let decls: Vec<serde_json::Value> = tools
            .iter()
            .map(|ot| {
                let func = &ot["function"];
                serde_json::json!({
                    "name": func["name"].as_str().unwrap_or_default(),
                    "description": func["description"].as_str().unwrap_or_default(),
                    "parameters": func["parameters"].clone()
                })
            })
            .collect();

        let gemini_tools = if decls.is_empty() {
            Vec::new()
        } else {
            vec![serde_json::json!({
                "functionDeclarations": decls
            })]
        };

        // Map messages
        let mut gemini_contents = Vec::new();
        for msg in messages {
            match msg.role.as_str() {
                "user" => {
                    gemini_contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: Some(msg.content.clone().unwrap_or_default()),
                            function_call: None,
                            function_response: None,
                        }],
                    });
                }
                "assistant" => {
                    let mut parts = Vec::new();
                    if let Some(text) = &msg.content {
                        parts.push(GeminiPart {
                            text: Some(text.clone()),
                            function_call: None,
                            function_response: None,
                        });
                    }
                    if let Some(tool_calls) = &msg.tool_calls {
                        for tc in tool_calls {
                            let args: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(serde_json::Value::Null);
                            parts.push(GeminiPart {
                                text: None,
                                function_call: Some(serde_json::json!({
                                    "name": tc.function.name.clone(),
                                    "args": args
                                })),
                                function_response: None,
                            });
                        }
                    }
                    gemini_contents.push(GeminiContent {
                        role: "model".to_string(),
                        parts,
                    });
                }
                "tool" => {
                    let name = msg.name.clone().unwrap_or_default();
                    let content_json: serde_json::Value = msg
                        .content
                        .as_ref()
                        .and_then(|c| serde_json::from_str(c).ok())
                        .unwrap_or(serde_json::json!({ "result": msg.content.clone() }));
                    
                    gemini_contents.push(GeminiContent {
                        role: "user".to_string(),
                        parts: vec![GeminiPart {
                            text: None,
                            function_call: None,
                            function_response: Some(serde_json::json!({
                                "name": name,
                                "response": content_json
                            })),
                        }],
                    });
                }
                _ => {}
            }
        }

        let req_body = GeminiRequest {
            contents: gemini_contents,
            system_instruction: GeminiInstruction {
                parts: vec![GeminiPart {
                    text: Some(system_prompt.to_string()),
                    function_call: None,
                    function_response: None,
                }],
            },
            tools: gemini_tools,
        };

        // Stream endpoint
        let endpoint = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?key={}",
            self.model, self.api_key
        );

        let res = self
            .client
            .post(&endpoint)
            .json(&req_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let error_text = res.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(format!("Gemini API returned error: {}", error_text).into());
        }

        let byte_stream = res.bytes_stream();
        let line_stream = bytes_to_lines(byte_stream);

        // Gemini stream wraps JSON array elements or streams JSON lines.
        // We clean brackets and parse each line.
        let delta_stream = line_stream.filter_map(|line_result| async move {
            match line_result {
                Err(err) => Some(Err(err)),
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        return None;
                    }
                    // Strip JSON array characters if they wrap stream elements
                    let clean_line = if line.starts_with('[') {
                        &line[1..]
                    } else if line.ends_with(']') {
                        &line[..line.len() - 1]
                    } else if line.starts_with(',') {
                        &line[1..]
                    } else {
                        line
                    };

                    let clean_line = clean_line.trim();
                    if clean_line.is_empty() {
                        return None;
                    }

                    match serde_json::from_str::<GeminiChunk>(clean_line) {
                        Err(_) => None,
                        Ok(chunk) => {
                            if let Some(candidates) = &chunk.candidates {
                                if let Some(candidate) = candidates.first() {
                                    if let Some(content) = &candidate.content {
                                        if let Some(parts) = &content.parts {
                                            if let Some(part) = parts.first() {
                                                if let Some(text) = &part.text {
                                                    return Some(Ok(LlmResponseDelta::Text(text.clone())));
                                                }
                                                if let Some(fc) = &part.function_call {
                                                    let args_str = serde_json::to_string(&fc.args).unwrap_or_default();
                                                    return Some(Ok(LlmResponseDelta::ToolCall {
                                                        index: 0,
                                                        id: Some(format!("gemini_{}", fc.name)),
                                                        name: Some(fc.name.clone()),
                                                        arguments: args_str,
                                                    }));
                                                }
                                            }
                                        }
                                    }
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

// ==========================================
// 4. OllamaProvider
// ==========================================

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
    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        _tools: &[serde_json::Value],
    ) -> Result<
        BoxStream<'static, Result<LlmResponseDelta, Box<dyn std::error::Error + Send + Sync>>>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
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
            let error_text = res.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(format!("Ollama API returned error: {}", error_text).into());
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

// ==========================================
// 5. DeepSeekProvider
// ==========================================

pub struct DeepSeekProvider {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
    model: String,
}

impl DeepSeekProvider {
    pub fn new(api_key: String, model: String) -> Self {
        Self::with_url(api_key, "https://api.deepseek.com/v1".to_string(), model)
    }
    pub fn with_url(api_key: String, api_url: String, model: String) -> Self {
        Self { client: reqwest::Client::new(), api_key, api_url, model }
    }
}

#[async_trait]
impl LlmProvider for DeepSeekProvider {
    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<
        BoxStream<'static, Result<LlmResponseDelta, Box<dyn std::error::Error + Send + Sync>>>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
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
            .json(&req_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let error_text = res.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(format!("LLM API returned error: {}", error_text).into());
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
                    if !line.starts_with("data:") {
                        return None;
                    }
                    let data_part = line["data:".len()..].trim();
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
                                            return Some(Ok(LlmResponseDelta::Text(content.clone())));
                                        }
                                    }
                                    if let Some(reasoning) = &choice.delta.reasoning_content {
                                        if !reasoning.is_empty() {
                                            return Some(Ok(LlmResponseDelta::Reasoning(reasoning.clone())));
                                        }
                                    }
                                    if let Some(tool_calls) = &choice.delta.tool_calls {
                                        if let Some(tc) = tool_calls.first() {
                                            let name = tc.function.as_ref().and_then(|f| f.name.clone());
                                            let args = tc.function.as_ref().and_then(|f| f.arguments.clone()).unwrap_or_default();
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
                            None
                        }
                    }
                }
            }
        });

        Ok(delta_stream.boxed())
    }
}

// ==========================================
// Helpers
// ==========================================

fn bytes_to_lines<S, E>(
    stream: S,
) -> impl Stream<Item = Result<String, Box<dyn std::error::Error + Send + Sync>>>
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
                        return Some((
                            Err(Box::new(err) as Box<dyn std::error::Error + Send + Sync>),
                            (stream, buffer),
                        ));
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
