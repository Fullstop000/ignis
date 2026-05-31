//! LLM domain: model metadata, provider-brand declarations, and wire protocols.

pub mod catalog;
pub mod protocols;
pub mod providers;
pub mod registry;

pub use catalog::ModelCatalog;
pub use protocols::{
    build, now_ms, Auth, LlmProvider, LlmResponseDelta, Message, Protocol, Resolved, ToolCall,
    ToolCallFunction, Usage,
};
pub use registry::{ModelOption, ProviderConfig};
