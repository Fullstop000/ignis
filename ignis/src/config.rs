use crate::models::{ModelCatalog, ModelOption, ProviderConfig};
use crate::provider::LlmProvider;
use crate::state::{load_state, State};
use anyhow::anyhow;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

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
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
}

/// Pre-declared permission rules. Each entry is a `Tool(pattern)` string
/// (e.g. `"bash(git *)"`, `"edit_file(src/**)"`, a bare `"bash"` for every use);
/// see `permissions::rule`. Evaluated `deny > ask > allow`, beneath the safety
/// floor and above session-allow / auto-approve modes.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// OpenTelemetry export. Off by default — telemetry is also gated at runtime by
/// the `IGNIS_ENABLE_TELEMETRY=1` env var, which overrides this config. Standard
/// OTEL_* env vars (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`,
/// `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_RESOURCE_ATTRIBUTES`, …) configure the
/// destination — ignis does not duplicate them in TOML.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// MCP (Model Context Protocol) server configuration. Each entry under
/// `[mcp.servers.<name>]` becomes a connection ignis spawns at startup; tools
/// the server advertises are exposed to the model as `mcp__<name>__<tool>`.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

/// One MCP server entry. `command` is required; everything else has a default.
/// `startup_timeout_secs` bounds the `initialize` handshake; `tool_timeout_secs`
/// bounds each individual `tools/call`.
#[derive(Debug, Deserialize, Clone)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
    #[serde(default = "default_tool_timeout_secs")]
    pub tool_timeout_secs: u64,
}

fn default_startup_timeout_secs() -> u64 {
    30
}
fn default_tool_timeout_secs() -> u64 {
    120
}

/// Validate that an MCP server name is safe to embed in `mcp__<name>__<tool>`
/// (which must satisfy the OpenAI tool-name regex `^[a-zA-Z0-9_-]{1,64}$`).
/// Capping the server name at 40 leaves room for `mcp__` (5) + `__` (2) + tool
/// name (up to 17) before hitting the 64-char limit.
pub fn validate_mcp_server_name(name: &str) -> Result<(), anyhow::Error> {
    if name.is_empty() || name.len() > 40 {
        return Err(anyhow!(
            "MCP server name '{}' must be 1-40 characters",
            name
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(anyhow!(
            "MCP server name '{}' contains invalid characters; allowed: [a-zA-Z0-9_-]",
            name
        ));
    }
    Ok(())
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
                .first()
                .map(|m| (name.clone(), m.name().to_string()))
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
    /// then by the order models are listed). Each model's context window is its
    /// config-declared override, else the models.dev `catalog` value.
    pub fn model_options(&self, catalog: &ModelCatalog) -> Vec<ModelOption> {
        let mut names: Vec<&String> = self.providers.keys().collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let pcfg = &self.providers[name];
            for entry in &pcfg.models {
                let model = entry.name().to_string();
                let context = entry.context().or_else(|| catalog.context_for(&model));
                out.push(ModelOption {
                    provider: name.clone(),
                    model,
                    effort_levels: entry.reasoning().to_vec(),
                    context,
                });
            }
        }
        out
    }

    /// Context window of the active model: its config-declared override, else the
    /// models.dev `catalog` value.
    pub fn active_context(&self, catalog: &ModelCatalog) -> Option<u64> {
        let (provider, model) = self.active_selection()?;
        self.providers
            .get(&provider)
            .and_then(|p| p.context(&model))
            .or_else(|| catalog.context_for(&model))
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
                        for name in config.mcp.servers.keys() {
                            validate_mcp_server_name(name)?;
                        }
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
                "openai",
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
                "kimi-code",
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
                "Moonshot Platform CN",
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
models = ["deepseek-v4-flash", { name = "deepseek-v4-pro", reasoning = ["high", "max"] }]
[providers.kimi-code]
api_key = "y"
models = ["kimi-for-coding"]
"#,
        )
        .unwrap();
        let opts = cfg.model_options(&ModelCatalog::default());
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
    fn inline_context_is_an_override() {
        // A model's inline `context` wins; a bare model has none (empty catalog).
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-pro"
[providers.deepseek]
api_key = "x"
models = ["deepseek-v4-flash", { name = "deepseek-v4-pro", context = 128000 }]
"#,
        )
        .unwrap();
        let opts = cfg.model_options(&ModelCatalog::default());
        assert_eq!(opts[0].context, None);
        assert_eq!(opts[1].context, Some(128000));
        assert_eq!(cfg.active_context(&ModelCatalog::default()), Some(128000));
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
models = ["deepseek-v4-flash", { name = "deepseek-v4-pro", reasoning = ["high", "max"] }]
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
models = ["deepseek-v4-flash", { name = "deepseek-v4-pro", reasoning = ["high", "max"] }]
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
models = [{ name = "deepseek-v4-pro", reasoning = ["high", "max"] }]
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
models = ["deepseek-v4-flash", { name = "deepseek-v4-pro", reasoning = ["high", "max"] }]
"#,
        )
        .unwrap();
        cfg.apply_state(State {
            model: Some("deepseek/deepseek-v4-pro".to_string()),
            reasoning_effort: Some("high".to_string()),
            disabled_skills: vec![],
            disabled_mcp_servers: vec![],
            mode: None,
            permission_grants: vec![],
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
    fn permissions_section_parses_three_lists() {
        let cfg: Config = toml::from_str(
            r#"
[providers.deepseek]
api_key = "x"
models = ["m"]

[permissions]
allow = ["bash(git *)", "edit_file(src/**)"]
ask = ["bash(git push *)"]
deny = ["bash(rm -rf *)"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.permissions.allow.len(), 2);
        assert_eq!(cfg.permissions.ask, vec!["bash(git push *)".to_string()]);
        assert_eq!(cfg.permissions.deny, vec!["bash(rm -rf *)".to_string()]);
    }

    #[test]
    fn empty_permissions_section_is_default() {
        let cfg: Config = toml::from_str(
            r#"
[providers.deepseek]
api_key = "x"
models = ["m"]
"#,
        )
        .unwrap();
        assert!(cfg.permissions.allow.is_empty());
        assert!(cfg.permissions.ask.is_empty());
        assert!(cfg.permissions.deny.is_empty());
    }

    #[test]
    fn mcp_config_defaults() {
        let cfg: Config = toml::from_str(
            r#"
[providers.deepseek]
api_key = "x"
models = ["m"]

[mcp.servers.github]
command = "gh"
args = ["mcp"]
"#,
        )
        .unwrap();
        let s = cfg.mcp.servers.get("github").unwrap();
        assert_eq!(s.command, "gh");
        assert_eq!(s.args, vec!["mcp"]);
        assert!(s.env.is_empty());
        assert_eq!(s.cwd, None);
        assert_eq!(s.startup_timeout_secs, 30);
        assert_eq!(s.tool_timeout_secs, 120);
    }

    #[test]
    fn mcp_config_explicit_fields() {
        let cfg: Config = toml::from_str(
            r#"
[providers.deepseek]
api_key = "x"
models = ["m"]

[mcp.servers.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = { NODE_OPTIONS = "--max-old-space-size=4096" }
cwd = "/work"
startup_timeout_secs = 10
tool_timeout_secs = 60
"#,
        )
        .unwrap();
        let s = cfg.mcp.servers.get("fs").unwrap();
        assert_eq!(s.command, "npx");
        assert_eq!(s.args.len(), 3);
        assert_eq!(
            s.env.get("NODE_OPTIONS").map(String::as_str),
            Some("--max-old-space-size=4096")
        );
        assert_eq!(s.cwd.as_ref().unwrap().to_str(), Some("/work"));
        assert_eq!(s.startup_timeout_secs, 10);
        assert_eq!(s.tool_timeout_secs, 60);
    }

    #[test]
    fn validate_mcp_server_name_accepts_alphanum_underscore_dash() {
        for ok in ["github", "fs", "my-server", "my_server", "abc123", "a"] {
            assert!(validate_mcp_server_name(ok).is_ok(), "expected '{ok}' ok");
        }
    }

    #[test]
    fn validate_mcp_server_name_rejects_invalid() {
        for bad in [
            "",
            "with space",
            "with.dot",
            "with/slash",
            "with:colon",
            // 41 chars - over the cap
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(
                validate_mcp_server_name(bad).is_err(),
                "expected '{bad}' rejected"
            );
        }
    }

    #[test]
    fn empty_mcp_section_is_default() {
        let cfg: Config = toml::from_str(
            r#"
[providers.deepseek]
api_key = "x"
models = ["m"]
"#,
        )
        .unwrap();
        assert!(cfg.mcp.servers.is_empty());
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
models = ["deepseek-v4-flash", { name = "deepseek-v4-pro", reasoning = ["high", "max"] }]
"#,
        )
        .unwrap();
        cfg.apply_state(State {
            model: Some("deepseek/deepseek-v4-flash".to_string()),
            reasoning_effort: None,
            disabled_skills: vec![],
            disabled_mcp_servers: vec![],
            mode: None,
            permission_grants: vec![],
        });
        assert_eq!(cfg.active_model().as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(cfg.active_effort(), None);
    }
}
