pub mod agent;
pub mod cli;
pub mod config;
pub mod logger;
pub mod models;
pub mod provider;
pub mod session;
pub mod state;
pub mod tools;
pub use session::storage;
pub mod console;
pub mod util;

pub use ignis_macros::tool;

// Crate-root re-exports: the public API surface.
pub use agent::{Agent, AgentEvent};
pub use provider::{Message, ToolCall, ToolCallFunction, Usage};
pub use session::Session;
pub use tools::tool::{AgentTool, ExecutionMode, IntoToolResult, ToolHooks, ToolResult};
