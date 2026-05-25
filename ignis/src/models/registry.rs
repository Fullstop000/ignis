//! What models exist and how to reach them: the per-provider declarations from
//! `config.toml` (`ProviderConfig`) and the flattened `/model` picker entries
//! (`ModelOption`). The active selection and provider construction live on
//! [`crate::config::Config`].

use anyhow::anyhow;
use serde::Deserialize;

/// A model in a provider's `models` list: either a bare name, or an inline table
/// carrying that model's metadata. So both of these are valid:
///
/// ```toml
/// models = [
///   "deepseek-v4-flash",
///   { name = "deepseek-v4-pro", reasoning = ["high", "max"], context = 128000 },
/// ]
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModelEntry {
    Name(String),
    Full(ModelDef),
}

/// The inline-table form of a model entry: a name plus optional metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelDef {
    pub name: String,
    /// Reasoning-effort levels, in display order (empty = no effort control).
    /// Levels differ by model (GPT: minimal..xhigh, Opus: low..max).
    #[serde(default)]
    pub reasoning: Vec<String>,
    /// Context window in tokens — an explicit override; otherwise it comes from
    /// models.dev.
    pub context: Option<u64>,
}

impl ModelEntry {
    pub fn name(&self) -> &str {
        match self {
            ModelEntry::Name(n) => n,
            ModelEntry::Full(d) => &d.name,
        }
    }

    pub fn reasoning(&self) -> &[String] {
        match self {
            ModelEntry::Name(_) => &[],
            ModelEntry::Full(d) => &d.reasoning,
        }
    }

    pub fn context(&self) -> Option<u64> {
        match self {
            ModelEntry::Name(_) => None,
            ModelEntry::Full(d) => d.context,
        }
    }
}

/// A provider entry: credentials plus the models it offers (each with optional
/// per-model metadata). The *active* model/effort live at the top level (see
/// [`crate::config::Config`]), not here.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub api_url: Option<String>,
    pub user_agent: Option<String>,
    /// Models this provider offers in the `/model` picker, in display order.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

impl ProviderConfig {
    /// Declared effort levels for `model`, in display order (empty = no effort).
    pub(crate) fn effort_levels(&self, model: &str) -> Vec<String> {
        self.models
            .iter()
            .find(|m| m.name() == model)
            .map(|m| m.reasoning().to_vec())
            .unwrap_or_default()
    }

    /// Config-declared context window for `model` (the explicit override).
    pub(crate) fn context(&self, model: &str) -> Option<u64> {
        self.models
            .iter()
            .find(|m| m.name() == model)
            .and_then(|m| m.context())
    }

    pub(crate) fn require(
        &self,
        field: Option<String>,
        provider: &str,
        name: &str,
    ) -> Result<String, anyhow::Error> {
        field.ok_or_else(|| anyhow!("{} provider requires {}", provider, name))
    }
}

/// One selectable entry in the `/model` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub provider: String,
    pub model: String,
    /// Effort levels this model accepts, in display order (empty = none).
    pub effort_levels: Vec<String>,
    /// Context window in tokens (config override, else models.dev); `None` = unknown.
    pub context: Option<u64>,
}
