use crate::provider::LlmProvider;
use crate::state::{load_state, State};
use anyhow::anyhow;
use serde::Deserialize;
use std::collections::HashMap;

/// A provider entry: credentials plus the catalog of models it offers. The
/// *active* model/effort live at the top level (see [`Config::model`]), not here.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub api_url: Option<String>,
    pub user_agent: Option<String>,
    /// Models this provider offers in the `/model` picker.
    pub models: Option<Vec<String>>,
    /// Per-model reasoning-effort levels, e.g. `{ "deepseek-v4-pro" = ["high",
    /// "max"] }`. Levels differ by model (GPT: minimal..xhigh, Opus: low..max),
    /// so they're declared, not hardcoded. A model absent here has no effort.
    pub reasoning: Option<HashMap<String, Vec<String>>>,
}

impl ProviderConfig {
    /// Declared effort levels for `model`, in display order (empty = no effort).
    fn effort_levels(&self, model: &str) -> Vec<String> {
        self.reasoning
            .as_ref()
            .and_then(|r| r.get(model))
            .cloned()
            .unwrap_or_default()
    }

    fn require(
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
}

/// Web search backend configuration. `provider` selects the backend
/// (default "brave"); `api_key` is the credential for that backend.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct WebSearchConfig {
    pub provider: Option<String>,
    pub api_key: Option<String>,
}

/// Context-compaction settings. Token counts use a chars/4 estimate.
/// `#[serde(default)]` fills any omitted field from `Default`, so a partial
/// `[compaction]` table is fine.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CompactionConfig {
    /// Auto-compact before a prompt when estimated history exceeds the threshold.
    pub auto: bool,
    /// Estimated-token threshold that triggers auto-compaction.
    pub threshold_tokens: usize,
    /// Estimated tokens of recent history to keep verbatim when compacting.
    pub keep_recent_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: true,
            threshold_tokens: 120_000,
            keep_recent_tokens: 16_000,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Config {
    /// The active selection as `"provider/model"` (e.g. `"deepseek/deepseek-v4-pro"`).
    /// `/model` writes this. If unset, the first configured provider's first model.
    pub model: Option<String>,
    /// The active reasoning effort; applied only if it's a declared level for the
    /// active model. `/model` writes this.
    pub reasoning_effort: Option<String>,
    pub auto_resume_last_session: Option<bool>,
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub web_search: WebSearchConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
}

impl Config {
    /// The active `(provider, model)`: parsed from `model = "provider/model"`,
    /// else the first configured provider (sorted) that lists a model.
    pub fn active_selection(&self) -> Option<(String, String)> {
        if let Some(sel) = &self.model {
            let (provider, model) = sel.split_once('/')?;
            return Some((provider.to_string(), model.to_string()));
        }
        let mut names: Vec<&String> = self.providers.keys().collect();
        names.sort();
        names.into_iter().find_map(|name| {
            self.providers[name]
                .models
                .as_ref()
                .and_then(|m| m.first())
                .map(|m| (name.clone(), m.clone()))
        })
    }

    pub fn active_provider(&self) -> Option<String> {
        self.active_selection().map(|(p, _)| p)
    }

    pub fn active_model(&self) -> Option<String> {
        self.active_selection().map(|(_, m)| m)
    }

    /// Overlay the runtime state's `/model` selection (it takes priority over
    /// the config's optional default). A state with a model fully defines the
    /// active selection, including clearing effort for a non-reasoning model.
    pub fn apply_state(&mut self, state: State) {
        if state.model.is_some() {
            self.model = state.model;
            self.reasoning_effort = state.reasoning_effort;
        }
    }

    /// The reasoning effort to send: only if it's a declared level for the active
    /// model (OpenAI-compatible providers honor it).
    pub fn active_effort(&self) -> Option<String> {
        let level = self.reasoning_effort.as_deref()?;
        let (provider, model) = self.active_selection()?;
        self.providers
            .get(&provider)?
            .effort_levels(&model)
            .into_iter()
            .find(|l| l == level)
    }

    /// Flatten every provider's catalog into picker entries (sorted by provider,
    /// then by the order models are listed).
    pub fn model_options(&self) -> Vec<ModelOption> {
        let mut names: Vec<&String> = self.providers.keys().collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let pcfg = &self.providers[name];
            for model in pcfg.models.clone().unwrap_or_default() {
                let effort_levels = pcfg.effort_levels(&model);
                out.push(ModelOption {
                    provider: name.clone(),
                    model,
                    effort_levels,
                });
            }
        }
        out
    }
}

pub fn load_config() -> Result<Config, anyhow::Error> {
    let mut paths = Vec::new();
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".ignis/config.toml"));
    }

    let mut last_err = None;
    for path in paths {
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<Config>(&content) {
                    Ok(mut config) => {
                        config.apply_state(load_state());
                        return Ok(config);
                    }
                    Err(e) => last_err = Some(anyhow!("Failed to parse {}: {}", path.display(), e)),
                },
                Err(e) => last_err = Some(anyhow!("Failed to read {}: {}", path.display(), e)),
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("config.toml not found")))
}

pub fn build_provider(config: &Config) -> Result<Box<dyn LlmProvider>, anyhow::Error> {
    let (provider_name, model) = config.active_selection().ok_or_else(|| {
        anyhow!("no active model — set `model = \"provider/model\"` and a provider's `models`")
    })?;
    let prov_cfg = config.providers.get(&provider_name).ok_or_else(|| {
        anyhow!(
            "Configuration for active provider '{}' not found",
            provider_name
        )
    })?;

    // Reasoning effort applies only to OpenAI-compatible providers below, and
    // only when it's a declared level for the active model.
    let effort = config.active_effort();

    match provider_name.as_str() {
        "openai" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "openai", "api_key")?;
            let api_url = prov_cfg.require(prov_cfg.api_url.clone(), "openai", "api_url")?;
            Ok(Box::new(crate::provider::OpenAiProvider::new(
                api_key,
                api_url,
                model,
                prov_cfg.user_agent.clone(),
                effort,
            )))
        }
        "deepseek" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "deepseek", "api_key")?;
            let api_url = prov_cfg
                .api_url
                .clone()
                .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());
            Ok(Box::new(crate::provider::DeepSeekProvider::with_url(
                api_key, api_url, model, effort,
            )))
        }
        "kimi-code" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "kimi-code", "api_key")?;
            let api_url = prov_cfg
                .api_url
                .clone()
                .unwrap_or_else(|| "https://api.kimi.com/coding/v1".to_string());
            // Kimi Coding Plan requires a whitelisted User-Agent
            let ua = prov_cfg
                .user_agent
                .clone()
                .unwrap_or_else(|| "KimiCLI/1.44.0".to_string());
            Ok(Box::new(crate::provider::OpenAiProvider::new(
                api_key,
                api_url,
                model,
                Some(ua),
                effort,
            )))
        }
        "Moonshot Platform CN" => {
            let api_key =
                prov_cfg.require(prov_cfg.api_key.clone(), "Moonshot Platform CN", "api_key")?;
            let api_url =
                prov_cfg.require(prov_cfg.api_url.clone(), "Moonshot Platform CN", "api_url")?;
            Ok(Box::new(crate::provider::OpenAiProvider::new(
                api_key,
                api_url,
                model,
                prov_cfg.user_agent.clone(),
                effort,
            )))
        }
        "anthropic" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "anthropic", "api_key")?;
            Ok(Box::new(crate::provider::AnthropicProvider::new(
                api_key, model,
            )))
        }
        "gemini" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "gemini", "api_key")?;
            Ok(Box::new(crate::provider::GeminiProvider::new(
                api_key, model,
            )))
        }
        "ollama" => {
            let api_url = prov_cfg
                .api_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            Ok(Box::new(crate::provider::OllamaProvider::new(
                api_url, model,
            )))
        }
        other => Err(anyhow!("Unknown provider type: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_options_flattens_providers_with_effort_levels() {
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
[providers.deepseek.reasoning]
deepseek-v4-pro = ["high", "max"]
[providers.kimi-code]
api_key = "y"
models = ["kimi-for-coding"]
"#,
        )
        .unwrap();
        let opts = cfg.model_options();
        // Sorted by provider name (deepseek before kimi-code).
        assert_eq!(opts.len(), 3);
        assert_eq!(
            (opts[0].provider.as_str(), opts[0].model.as_str()),
            ("deepseek", "deepseek-v4-flash")
        );
        assert!(opts[0].effort_levels.is_empty());
        assert_eq!(opts[1].model, "deepseek-v4-pro");
        assert_eq!(opts[1].effort_levels, vec!["high", "max"]);
        assert_eq!(
            (opts[2].provider.as_str(), opts[2].model.as_str()),
            ("kimi-code", "kimi-for-coding")
        );
        assert!(opts[2].effort_levels.is_empty());
    }

    #[test]
    fn active_selection_parses_provider_slash_model() {
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-pro"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.active_selection(),
            Some(("deepseek".to_string(), "deepseek-v4-pro".to_string()))
        );
        assert_eq!(cfg.active_provider().as_deref(), Some("deepseek"));
        assert_eq!(cfg.active_model().as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn active_selection_falls_back_to_first_provider_model() {
        // No top-level `model`: first provider (sorted) with a catalog wins.
        let cfg: Config = toml::from_str(
            r#"
[providers.kimi-code]
api_key = "y"
models = ["kimi-for-coding"]
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.active_selection(),
            Some(("deepseek".to_string(), "deepseek-v4-flash".to_string()))
        );
    }

    #[test]
    fn active_effort_only_returns_declared_levels() {
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-pro"
reasoning_effort = "max"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
[providers.deepseek.reasoning]
deepseek-v4-pro = ["high", "max"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.active_effort().as_deref(), Some("max"));
    }

    #[test]
    fn active_effort_none_for_model_without_levels() {
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
reasoning_effort = "max"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
[providers.deepseek.reasoning]
deepseek-v4-pro = ["high", "max"]
"#,
        )
        .unwrap();
        // flash declares no levels → no effort even though one is set.
        assert_eq!(cfg.active_effort(), None);
    }

    #[test]
    fn active_effort_ignores_undeclared_value() {
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-pro"
reasoning_effort = "ultra"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-pro"]
[providers.deepseek.reasoning]
deepseek-v4-pro = ["high", "max"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.active_effort(), None);
    }

    #[test]
    fn state_overrides_config_default() {
        let mut cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
[providers.deepseek.reasoning]
deepseek-v4-pro = ["high", "max"]
"#,
        )
        .unwrap();
        cfg.apply_state(State {
            model: Some("deepseek/deepseek-v4-pro".to_string()),
            reasoning_effort: Some("high".to_string()),
        });
        assert_eq!(cfg.active_model().as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(cfg.active_effort().as_deref(), Some("high"));
    }

    #[test]
    fn empty_state_keeps_config_default() {
        let mut cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
"#,
        )
        .unwrap();
        cfg.apply_state(State::default());
        assert_eq!(cfg.active_model().as_deref(), Some("deepseek-v4-flash"));
    }

    #[test]
    fn state_with_model_clears_stale_effort() {
        // Switching to a non-reasoning model via state drops a prior effort.
        let mut cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-pro"
reasoning_effort = "high"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", "deepseek-v4-pro"]
[providers.deepseek.reasoning]
deepseek-v4-pro = ["high", "max"]
"#,
        )
        .unwrap();
        cfg.apply_state(State {
            model: Some("deepseek/deepseek-v4-flash".to_string()),
            reasoning_effort: None,
        });
        assert_eq!(cfg.active_model().as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(cfg.active_effort(), None);
    }
}
