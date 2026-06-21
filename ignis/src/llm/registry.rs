//! What models exist and how to reach them: the per-provider declarations from
//! `config.toml` (`ProviderConfig`) and the flattened `/model` picker entries
//! (`ModelOption`). The active selection and provider construction live on
//! [`crate::config::Config`].

use crate::llm::protocols::Protocol;
use serde::Deserialize;

/// A model in a provider's `models` list: either a bare name, or an inline table
/// carrying that model's metadata. So both of these are valid:
///
/// ```toml
/// models = [
///   "deepseek-v4-flash",
///   { name = "deepseek-v4-pro", reasoning = ["high", "max"], tier = "high" },
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
    /// Capability tier override — `"low"`, `"medium"`, or `"high"`. Overrides the
    /// baked catalog tier for this model and lets a sub-agent route a task by
    /// complexity. Omit to inherit the baked default (or stay untiered).
    #[serde(default)]
    pub tier: Option<String>,
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

    /// The config-declared tier override, if any (`None` for a bare name).
    pub fn tier(&self) -> Option<&str> {
        match self {
            ModelEntry::Name(_) => None,
            ModelEntry::Full(d) => d.tier.as_deref(),
        }
    }
}

/// A provider entry: credentials plus the models it offers (each with optional
/// per-model metadata). The *active* model/effort live at the top level (see
/// [`crate::config::Config`]), not here.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    /// Override the selected endpoint's base URL (required for the `custom` brand).
    pub api_url: Option<String>,
    /// Force a protocol when a brand offers more than one (e.g. `"openai"` to use
    /// MiniMax's OpenAI-compatible endpoint instead of the default Anthropic one).
    pub protocol: Option<Protocol>,
    pub user_agent: Option<String>,
    /// Extra models on top of the baked catalog (and the only source for `custom`).
    /// Merged by name, config winning on a clash.
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
