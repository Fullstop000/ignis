//! Conversation message types and token usage — the LLM protocol model shared
//! across providers, the agent loop, and persisted sessions.

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
    /// Wall-clock capture time in epoch ms. Not part of the payload sent to
    /// LLM providers — `#[serde(skip)]` keeps it out — but transcribed to the
    /// session-JSONL record envelope timestamp by `FileStorage`, and round-trips
    /// back here through `parse_jsonl_messages` so `/sessions` can render an
    /// accurate per-turn waterfall.
    #[serde(skip)]
    pub created_at_ms: Option<u64>,
}

impl Message {
    /// Builder: stamp the message with the current wall-clock time. Use at
    /// push sites that own a real-time event (user prompts, streamed assistant
    /// chunks, tool results) so the waterfall shows real durations.
    pub fn stamp_now(mut self) -> Self {
        self.created_at_ms = Some(now_ms());
        self
    }
}

/// Wall-clock epoch ms. Public so the agent loop can stamp inline literals
/// without the `.stamp_now()` builder when constructing inside a `push()` call.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Real token usage for a turn or session, mirroring how Claude/Kimi/Codex
/// record it. `input_tokens` is the full prompt (cache reads/writes included);
/// `cache_read_tokens` is the cached subset. `reasoning_tokens` is the
/// invisible-thinking subset of `output_tokens` (OpenAI o-series, Anthropic
/// extended thinking) — operators care about it for cost attribution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
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
