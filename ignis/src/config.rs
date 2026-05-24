use crate::provider::LlmProvider;
use anyhow::anyhow;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub api_url: Option<String>,
    pub model: Option<String>,
    pub user_agent: Option<String>,
}

impl ProviderConfig {
    fn require(
        &self,
        field: Option<String>,
        provider: &str,
        name: &str,
    ) -> Result<String, anyhow::Error> {
        field.ok_or_else(|| anyhow!("{} provider requires {}", provider, name))
    }
}

/// Web search backend configuration. `provider` selects the backend
/// (default "brave"); `api_key` is the credential for that backend.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct WebSearchConfig {
    pub provider: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub active_provider: String,
    pub auto_resume_last_session: Option<bool>,
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub web_search: WebSearchConfig,
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
                    Ok(config) => return Ok(config),
                    Err(e) => last_err = Some(anyhow!("Failed to parse {}: {}", path.display(), e)),
                },
                Err(e) => last_err = Some(anyhow!("Failed to read {}: {}", path.display(), e)),
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("config.toml not found")))
}

pub fn build_provider(config: &Config) -> Result<Box<dyn LlmProvider>, anyhow::Error> {
    let provider_name = &config.active_provider;
    let prov_cfg = config.providers.get(provider_name).ok_or_else(|| {
        anyhow!(
            "Configuration for active provider '{}' not found",
            provider_name
        )
    })?;

    match provider_name.as_str() {
        "openai" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "openai", "api_key")?;
            let api_url = prov_cfg.require(prov_cfg.api_url.clone(), "openai", "api_url")?;
            let model = prov_cfg.require(prov_cfg.model.clone(), "openai", "model")?;
            Ok(Box::new(crate::provider::OpenAiProvider::new(
                api_key,
                api_url,
                model,
                prov_cfg.user_agent.clone(),
            )))
        }
        "deepseek" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "deepseek", "api_key")?;
            let api_url = prov_cfg
                .api_url
                .clone()
                .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string());
            let model = prov_cfg.require(prov_cfg.model.clone(), "deepseek", "model")?;
            Ok(Box::new(crate::provider::DeepSeekProvider::with_url(
                api_key, api_url, model,
            )))
        }
        "kimi-code" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "kimi-code", "api_key")?;
            let api_url = prov_cfg
                .api_url
                .clone()
                .unwrap_or_else(|| "https://api.kimi.com/coding/v1".to_string());
            let model = prov_cfg.require(prov_cfg.model.clone(), "kimi-code", "model")?;
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
            )))
        }
        "Moonshot Platform CN" => {
            let api_key =
                prov_cfg.require(prov_cfg.api_key.clone(), "Moonshot Platform CN", "api_key")?;
            let api_url =
                prov_cfg.require(prov_cfg.api_url.clone(), "Moonshot Platform CN", "api_url")?;
            let model =
                prov_cfg.require(prov_cfg.model.clone(), "Moonshot Platform CN", "model")?;
            Ok(Box::new(crate::provider::OpenAiProvider::new(
                api_key,
                api_url,
                model,
                prov_cfg.user_agent.clone(),
            )))
        }
        "anthropic" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "anthropic", "api_key")?;
            let model = prov_cfg.require(prov_cfg.model.clone(), "anthropic", "model")?;
            Ok(Box::new(crate::provider::AnthropicProvider::new(
                api_key, model,
            )))
        }
        "gemini" => {
            let api_key = prov_cfg.require(prov_cfg.api_key.clone(), "gemini", "api_key")?;
            let model = prov_cfg.require(prov_cfg.model.clone(), "gemini", "model")?;
            Ok(Box::new(crate::provider::GeminiProvider::new(
                api_key, model,
            )))
        }
        "ollama" => {
            let api_url = prov_cfg
                .api_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let model = prov_cfg.require(prov_cfg.model.clone(), "ollama", "model")?;
            Ok(Box::new(crate::provider::OllamaProvider::new(
                api_url, model,
            )))
        }
        other => Err(anyhow!("Unknown provider type: {}", other)),
    }
}
