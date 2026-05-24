pub mod agent;
pub mod cli;
pub mod config;
pub mod logger;
pub mod provider;
pub mod session;
pub mod storage;
pub mod tool;
pub mod tools;
pub use tools::plugin;
pub mod console;
pub mod types;
pub mod util;

pub use ignis_macros::tool;

// Re-exports for backward compatibility
pub use agent::Agent;
pub use session::Session;
pub use tool::{AgentTool, ExecutionMode, IntoToolResult, ToolHooks, ToolResult};
pub use types::{AgentEvent, Message, ToolCall, ToolCallFunction};
