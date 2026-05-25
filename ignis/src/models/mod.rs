//! Model domain: the provider/model declarations and selectable options
//! (`registry`) and the models.dev metadata catalog (`catalog`). This is *which*
//! models exist and their metadata; the runtime LLM clients live in
//! [`crate::provider`].

pub mod catalog;
pub mod registry;

pub use catalog::ModelCatalog;
pub use registry::{ModelOption, ProviderConfig};
