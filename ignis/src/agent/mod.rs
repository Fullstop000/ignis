use std::collections::HashMap;
use std::sync::Arc;

use crate::provider::{LlmProvider, LlmResponseDelta};
use crate::tool::{AgentTool, ExecutionMode, ToolHooks, ToolResult};
use crate::types::{AgentEvent, Message, ToolCall, ToolCallFunction};

/// Execution engine: given a conversation `history`, runs the model + tool
/// loop and emits events. State and persistence live in [`crate::Session`].
pub struct Agent {
    system_prompt: String,
    provider: Box<dyn LlmProvider>,
    tools: Vec<Arc<dyn AgentTool>>,
    hooks: Option<Box<dyn ToolHooks>>,
}

struct AccumulatingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl Agent {
    pub fn new(system_prompt: String, provider: Box<dyn LlmProvider>) -> Self {
        Self {
            system_prompt,
            provider,
            tools: Vec::new(),
            hooks: None,
        }
    }

    pub fn register_tool(&mut self, tool: Arc<dyn AgentTool>) {
        self.tools.push(tool);
    }

    pub fn set_hooks(&mut self, hooks: Box<dyn ToolHooks>) {
        self.hooks = Some(hooks);
    }

    /// One-shot, tool-less completion: stream a response for `messages` and
    /// return the concatenated text. Used by [`crate::Session::compact`] to
    /// summarize history.
    pub async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
    ) -> Result<String, anyhow::Error> {
        use futures_util::stream::StreamExt;
        let mut stream = self
            .provider
            .chat_stream(system_prompt, messages, &[])
            .await?;
        let mut out = String::new();
        while let Some(delta) = stream.next().await {
            if let Ok(LlmResponseDelta::Text(text)) = delta {
                out.push_str(&text);
            }
        }
        Ok(out)
    }

    fn sanitize_and_truncate_error(err: &str) -> String {
        // Redact potential API keys/secrets patterns
        let redacted = err.replace(r"sk-[a-zA-Z0-9]{32,}", "[REDACTED_API_KEY]");
        // Truncate to maximum 500 characters
        if redacted.len() > 500 {
            format!("{}... [truncated]", &redacted[..500])
        } else {
            redacted
        }
    }

    /// Run the model + tool loop over `history`, appending assistant and tool
    /// messages in place and emitting events. Does not load or persist; the
    /// caller ([`crate::Session`]) owns history and storage.
    pub async fn run(
        &self,
        history: &mut Vec<Message>,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<(), anyhow::Error> {
        let _ = tx.send(AgentEvent::AgentStart).await;

        let tool_schemas: Vec<serde_json::Value> = self
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters(),
                    }
                })
            })
            .collect();

        loop {
            let _ = tx.send(AgentEvent::TurnStart).await;

            let mut stream = match self
                .provider
                .chat_stream(&self.system_prompt, &*history, &tool_schemas)
                .await
            {
                Ok(s) => s,
                Err(err) => {
                    let err_msg = Self::sanitize_and_truncate_error(&err.to_string());
                    let error_content = format!("Error: {}", err_msg);
                    let _ = tx
                        .send(AgentEvent::MessageStart {
                            message: Message {
                                role: "assistant".to_string(),
                                content: Some(String::new()),
                                reasoning_content: None,
                                name: None,
                                tool_call_id: None,
                                tool_calls: None,
                            },
                        })
                        .await;
                    let _ = tx
                        .send(AgentEvent::MessageUpdate {
                            delta: error_content.clone(),
                        })
                        .await;
                    let _ = tx
                        .send(AgentEvent::MessageEnd {
                            message: Message {
                                role: "assistant".to_string(),
                                content: Some(error_content),
                                reasoning_content: None,
                                name: None,
                                tool_call_id: None,
                                tool_calls: None,
                            },
                        })
                        .await;
                    break;
                }
            };

            let mut assistant_content = String::new();
            let mut reasoning_content = String::new();
            let mut pending_tool_calls: HashMap<usize, AccumulatingToolCall> = HashMap::new();
            let mut message_started = false;

            use futures_util::stream::StreamExt;
            while let Some(delta_result) = stream.next().await {
                match delta_result {
                    Err(err) => {
                        let err_msg = Self::sanitize_and_truncate_error(&err.to_string());
                        let _ = tx
                            .send(AgentEvent::MessageUpdate {
                                delta: format!("\n[Error in stream: {}]", err_msg),
                            })
                            .await;
                    }
                    Ok(delta) => match delta {
                        LlmResponseDelta::Text(content) => {
                            if !message_started {
                                let _ = tx
                                    .send(AgentEvent::MessageStart {
                                        message: Message {
                                            role: "assistant".to_string(),
                                            content: Some(String::new()),
                                            reasoning_content: None,
                                            name: None,
                                            tool_call_id: None,
                                            tool_calls: None,
                                        },
                                    })
                                    .await;
                                message_started = true;
                            }
                            assistant_content.push_str(&content);
                            let _ = tx.send(AgentEvent::MessageUpdate { delta: content }).await;
                        }
                        LlmResponseDelta::Reasoning(reasoning) => {
                            reasoning_content.push_str(&reasoning);
                        }
                        LlmResponseDelta::ToolCall {
                            index,
                            id,
                            name,
                            arguments,
                        } => {
                            let entry = pending_tool_calls.entry(index).or_insert_with(|| {
                                AccumulatingToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: String::new(),
                                }
                            });
                            if let Some(id_val) = id {
                                entry.id.push_str(&id_val);
                            }
                            if let Some(name_val) = name {
                                entry.name.push_str(&name_val);
                            }
                            entry.arguments.push_str(&arguments);
                        }
                    },
                }
            }

            // Sort by index to maintain deterministic order
            let mut tool_calls = Vec::new();
            if !pending_tool_calls.is_empty() {
                let mut sorted_keys: Vec<&usize> = pending_tool_calls.keys().collect();
                sorted_keys.sort();
                for key in sorted_keys {
                    if let Some(pending) = pending_tool_calls.get(key) {
                        tool_calls.push(ToolCall {
                            id: pending.id.clone(),
                            r#type: "function".to_string(),
                            function: ToolCallFunction {
                                name: pending.name.clone(),
                                arguments: pending.arguments.clone(),
                            },
                        });
                    }
                }
            }

            let has_content = !assistant_content.is_empty();
            let has_reasoning = !reasoning_content.is_empty();
            let has_tools = !tool_calls.is_empty();

            if has_content || has_reasoning || has_tools {
                let msg = Message {
                    role: "assistant".to_string(),
                    content: if has_content {
                        Some(assistant_content.clone())
                    } else {
                        None
                    },
                    reasoning_content: if has_reasoning {
                        Some(reasoning_content.clone())
                    } else {
                        None
                    },
                    name: None,
                    tool_call_id: None,
                    tool_calls: if has_tools {
                        Some(tool_calls.clone())
                    } else {
                        None
                    },
                };

                if message_started {
                    let _ = tx
                        .send(AgentEvent::MessageEnd {
                            message: msg.clone(),
                        })
                        .await;
                } else if has_tools || has_reasoning {
                    let _ = tx
                        .send(AgentEvent::MessageStart {
                            message: msg.clone(),
                        })
                        .await;
                    let _ = tx
                        .send(AgentEvent::MessageEnd {
                            message: msg.clone(),
                        })
                        .await;
                }

                history.push(msg.clone());
            }

            if has_tools {
                let tx_clone = tx.clone();
                let tools_map: HashMap<String, Arc<dyn AgentTool>> = self
                    .tools
                    .iter()
                    .map(|t| (t.name().to_string(), t.clone()))
                    .collect();

                // Check if any tool requires sequential execution
                let force_sequential = tool_calls.iter().any(|tc| {
                    tools_map
                        .get(&tc.function.name)
                        .map(|t| t.execution_mode() == ExecutionMode::Sequential)
                        .unwrap_or(false)
                });

                let hooks = &self.hooks;
                let tool_calls_owned = tool_calls.clone();

                let execute_single_tool =
                    |tc: ToolCall,
                     tools_map: HashMap<String, Arc<dyn AgentTool>>,
                     tx_inner: tokio::sync::mpsc::Sender<AgentEvent>| {
                        async move {
                            let tc_id = tc.id;
                            let tool_name = tc.function.name;
                            let arguments_str = tc.function.arguments;
                            let maybe_tool = tools_map.get(&tool_name).cloned();

                            let _ = tx_inner
                                .send(AgentEvent::ToolExecutionStart {
                                    tool_call_id: tc_id.clone(),
                                    tool_name: tool_name.clone(),
                                    arguments: arguments_str.clone(),
                                })
                                .await;

                            let parsed_args_res: Result<serde_json::Value, serde_json::Error> =
                                serde_json::from_str(&arguments_str);

                            let result = match parsed_args_res {
                                Err(e) => {
                                    let err_msg =
                                        Agent::sanitize_and_truncate_error(&e.to_string());
                                    ToolResult::error(format!(
                                        "Invalid JSON arguments: {}",
                                        err_msg
                                    ))
                                }
                                Ok(args) => match maybe_tool {
                                    None => {
                                        ToolResult::error(format!("Tool '{}' not found", tool_name))
                                    }
                                    Some(tool) => tool.call(args).await,
                                },
                            };

                            let _ = tx_inner
                                .send(AgentEvent::ToolExecutionEnd {
                                    tool_call_id: tc_id.clone(),
                                    result: result.clone(),
                                })
                                .await;

                            // Build tool result message — send content as JSON for consistency
                            let result_json = serde_json::json!({
                                "result": result.content,
                                "is_error": result.is_error
                            });

                            Message {
                                role: "tool".to_string(),
                                content: Some(result_json.to_string()),
                                reasoning_content: None,
                                name: Some(tool_name),
                                tool_call_id: Some(tc_id),
                                tool_calls: None,
                            }
                        }
                    };

                let results = if force_sequential {
                    // Sequential execution
                    let mut results = Vec::new();
                    for tc in tool_calls_owned {
                        // Run beforeToolCall hook
                        let args_val: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        if let Some(h) = hooks {
                            if let Err(reason) =
                                h.before_tool_call(&tc.function.name, &args_val).await
                            {
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionStart {
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.function.name.clone(),
                                        arguments: tc.function.arguments.clone(),
                                    })
                                    .await;
                                let blocked_result =
                                    ToolResult::error(format!("Blocked by hook: {}", reason));
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionEnd {
                                        tool_call_id: tc.id.clone(),
                                        result: blocked_result.clone(),
                                    })
                                    .await;
                                let result_json = serde_json::json!({
                                    "result": blocked_result.content,
                                    "is_error": blocked_result.is_error
                                });
                                results.push(Message {
                                    role: "tool".to_string(),
                                    content: Some(result_json.to_string()),
                                    reasoning_content: None,
                                    name: Some(tc.function.name),
                                    tool_call_id: Some(tc.id),
                                    tool_calls: None,
                                });
                                continue;
                            }
                        }
                        let msg =
                            execute_single_tool(tc, tools_map.clone(), tx_clone.clone()).await;
                        results.push(msg);
                    }
                    results
                } else {
                    // Parallel execution — run beforeToolCall hooks first sequentially
                    let mut allowed_calls = Vec::new();
                    let mut blocked_results = Vec::new();

                    for tc in &tool_calls_owned {
                        let args_val: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        if let Some(h) = hooks {
                            if let Err(reason) =
                                h.before_tool_call(&tc.function.name, &args_val).await
                            {
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionStart {
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.function.name.clone(),
                                        arguments: tc.function.arguments.clone(),
                                    })
                                    .await;
                                let blocked_result =
                                    ToolResult::error(format!("Blocked by hook: {}", reason));
                                let _ = tx_clone
                                    .send(AgentEvent::ToolExecutionEnd {
                                        tool_call_id: tc.id.clone(),
                                        result: blocked_result.clone(),
                                    })
                                    .await;
                                let result_json = serde_json::json!({
                                    "result": blocked_result.content,
                                    "is_error": blocked_result.is_error
                                });
                                blocked_results.push((
                                    tc.id.clone(),
                                    Message {
                                        role: "tool".to_string(),
                                        content: Some(result_json.to_string()),
                                        reasoning_content: None,
                                        name: Some(tc.function.name.clone()),
                                        tool_call_id: Some(tc.id.clone()),
                                        tool_calls: None,
                                    },
                                ));
                                continue;
                            }
                        }
                        allowed_calls.push(tc.clone());
                    }

                    let tool_futures = allowed_calls
                        .into_iter()
                        .map(|tc| execute_single_tool(tc, tools_map.clone(), tx_clone.clone()));

                    let mut parallel_results: Vec<Message> =
                        futures_util::stream::iter(tool_futures)
                            .buffer_unordered(5)
                            .collect::<Vec<Message>>()
                            .await;

                    // Merge blocked results
                    for (_id, msg) in blocked_results {
                        parallel_results.push(msg);
                    }
                    parallel_results
                };

                // Re-align results with original order to maintain history determinism
                let mut results_by_id: HashMap<String, Message> = results
                    .into_iter()
                    .filter_map(|msg| {
                        if let Some(id) = &msg.tool_call_id {
                            Some((id.clone(), msg))
                        } else {
                            None
                        }
                    })
                    .collect();

                for tc in &tool_calls {
                    if let Some(msg) = results_by_id.remove(&tc.id) {
                        // Run afterToolCall hook
                        if let Some(h) = hooks {
                            let content_str = msg.content.clone().unwrap_or_default();
                            let parsed: serde_json::Value =
                                serde_json::from_str(&content_str).unwrap_or_default();
                            let original_result = ToolResult {
                                content: parsed["result"]
                                    .as_str()
                                    .unwrap_or(&content_str)
                                    .to_string(),
                                is_error: parsed["is_error"].as_bool().unwrap_or(false),
                            };
                            let transformed = h
                                .after_tool_call(msg.name.as_deref().unwrap_or(""), original_result)
                                .await;
                            let result_json = serde_json::json!({
                                "result": transformed.content,
                                "is_error": transformed.is_error
                            });
                            let transformed_msg = Message {
                                content: Some(result_json.to_string()),
                                ..msg
                            };
                            history.push(transformed_msg);
                        } else {
                            history.push(msg);
                        }
                    }
                }

                let _ = tx.send(AgentEvent::TurnEnd).await;
                // Continue Turn loop since we provided tool results back to LLM
            } else {
                let _ = tx.send(AgentEvent::TurnEnd).await;
                break; // Break turn loop if no tools were called
            }
        }

        let _ = tx.send(AgentEvent::AgentEnd).await;
        Ok(())
    }
}
