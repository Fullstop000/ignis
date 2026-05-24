use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Real token usage for a turn or session, mirroring how Claude/Kimi/Codex
/// record it. `input_tokens` is the full prompt (cache reads/writes included);
/// `cache_read_tokens` is the cached subset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    /// input + output (what the footer/loader headline).
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn is_zero(&self) -> bool {
        *self == Usage::default()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum AgentEvent {
    #[serde(rename = "agent_start")]
    AgentStart,
    #[serde(rename = "turn_start")]
    TurnStart,
    #[serde(rename = "message_start")]
    MessageStart { message: Message },
    #[serde(rename = "message_update")]
    MessageUpdate { delta: String },
    #[serde(rename = "message_end")]
    MessageEnd { message: Message },
    #[serde(rename = "tool_execution_start")]
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        arguments: String,
    },
    #[serde(rename = "tool_execution_end")]
    ToolExecutionEnd {
        tool_call_id: String,
        result: crate::tools::tool::ToolResult,
    },
    #[serde(rename = "turn_end")]
    TurnEnd,
    #[serde(rename = "usage")]
    Usage(Usage),
    #[serde(rename = "agent_end")]
    AgentEnd,
}
