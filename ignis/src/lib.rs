pub mod agent;
pub mod cli;
pub mod config;
pub mod logger;
pub mod provider;
pub mod session;
pub mod tools;
pub use session::storage;
pub use tools::plugin;
pub mod console;
pub mod types;
pub mod util;

pub use ignis_macros::tool;

// Crate-root re-exports: the public API surface.
pub use agent::Agent;
pub use session::Session;
pub use tools::tool::{AgentTool, ExecutionMode, IntoToolResult, ToolHooks, ToolResult};
pub use types::{AgentEvent, Message, ToolCall, ToolCallFunction, Usage};
