pub mod agent;
pub mod cli;
pub mod config;
pub mod hooks;
pub mod llm;
pub mod logger;
pub mod mcp;
pub mod permissions;
pub mod session;
pub mod skills;
pub mod state;
pub mod telemetry;
pub mod tools;
pub use session::storage;
pub mod console;
pub mod util;

pub use ignis_macros::tool;

// Crate-root re-exports: the public API surface.
pub use agent::{Agent, AgentEvent};
pub use llm::{Message, ToolCall, ToolCallFunction, Usage};
pub use mcp::{McpRegistry, McpServerEntry, McpStatus};
pub use session::Session;
pub use skills::{Skill, SkillRegistry, SkillScope};
pub use tools::tool::{
    AgentTool, ExecutionMode, IntoToolResult, StaticTool, ToolArgs, ToolHooks, ToolOutcome,
    ToolParam, ToolResult,
};
