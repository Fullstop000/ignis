use super::{bytes_to_lines, LlmProvider, LlmResponseDelta};
use crate::Message;
use anyhow::anyhow;
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};

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
    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        "gemini"
    }

    async fn chat_stream(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error> {
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
                            let args: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
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

        let res = self.client.post(&endpoint).json(&req_body).send().await?;

        if !res.status().is_success() {
            let error_text = res
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!("Gemini API returned error: {}", error_text));
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
                    let clean_line = if let Some(stripped) = line.strip_prefix('[') {
                        stripped
                    } else if let Some(stripped) = line.strip_suffix(']') {
                        stripped
                    } else if let Some(stripped) = line.strip_prefix(',') {
                        stripped
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
                                                    return Some(Ok(LlmResponseDelta::Text(
                                                        text.clone(),
                                                    )));
                                                }
                                                if let Some(fc) = &part.function_call {
                                                    let args_str = serde_json::to_string(&fc.args)
                                                        .unwrap_or_default();
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
