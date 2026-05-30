use crate::llm::{
    providers, Auth, LlmProvider, ModelCatalog, ModelOption, Protocol, ProviderConfig, Resolved,
};
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
            if let Some(m) = self.providers[name].models.first() {
                return Some((name.clone(), m.name().to_string()));
            }
            providers::lookup(name)
                .and_then(|s| s.models.first())
                .map(|m| (name.clone(), m.name.to_string()))
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
        self.effort_levels_for(&provider, &model)
            .into_iter()
            .find(|l| l == level)
    }

    /// Merged reasoning-effort levels for `(id, model)`: a config override (if
    /// non-empty) else the baked provider declaration's levels.
    fn effort_levels_for(&self, id: &str, model: &str) -> Vec<String> {
        if let Some(cfg) = self.providers.get(id) {
            let lvls = cfg.effort_levels(model);
            if !lvls.is_empty() {
                return lvls;
            }
        }
        providers::lookup(id)
            .and_then(|s| s.models.iter().find(|m| m.name == model))
            .map(|m| m.reasoning_effort.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }

    /// Merge provider metadata with config overrides into a [`Resolved`] selection.
    pub(crate) fn resolve(&self) -> Result<Resolved, anyhow::Error> {
        let (id, model) = self.active_selection().ok_or_else(|| {
            anyhow!("no active model — set `model = \"provider/model\"` and a provider's `models`")
        })?;
        let spec = providers::lookup(&id).ok_or_else(|| {
            let known = providers::all()
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!("unknown provider '{id}'; known: {known}")
        })?;
        let cfg = self.providers.get(&id);
        let (protocol, base_url, auth) = select_endpoint(spec, cfg)?;

        let api_key = cfg.and_then(|c| c.api_key.clone());
        if spec.api_key_required && api_key.is_none() {
            return Err(anyhow!("provider '{id}' requires `api_key` in config"));
        }
        let user_agent = cfg
            .and_then(|c| c.user_agent.clone())
            .or_else(|| spec.user_agent.map(str::to_string));

        Ok(Resolved {
            provider_id: id,
            protocol,
            base_url,
            auth,
            api_key,
            model,
            user_agent,
            reasoning_effort: self.active_effort(),
        })
    }

    /// Flatten every provider's declared models into picker entries (sorted by provider,
    /// then by the order models are listed). Each model's context window is its
    /// config-declared override, else the models.dev catalog value.
    pub fn model_options(&self, catalog_dev: &ModelCatalog) -> Vec<ModelOption> {
        let mut ids: Vec<&String> = self.providers.keys().collect();
        ids.sort();
        let mut out = Vec::new();
        for id in ids {
            let cfg = &self.providers[id];
            let mut seen = std::collections::HashSet::new();
            // Baked provider models first, with any per-model config override applied.
            if let Some(spec) = providers::lookup(id) {
                for m in spec.models {
                    let cfg_entry = cfg.models.iter().find(|e| e.name() == m.name);
                    let effort = cfg_entry
                        .map(|e| e.reasoning().to_vec())
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| {
                            m.reasoning_effort.iter().map(|s| s.to_string()).collect()
                        });
                    let context = cfg_entry
                        .and_then(|e| e.context())
                        .or(m.context)
                        .or_else(|| catalog_dev.context_for(m.name));
                    out.push(ModelOption {
                        provider: id.clone(),
                        model: m.name.to_string(),
                        effort_levels: effort,
                        context,
                    });
                    seen.insert(m.name.to_string());
                }
            }
            // Config-only models (not baked in), e.g. `custom` or extras.
            for entry in &cfg.models {
                if seen.contains(entry.name()) {
                    continue;
                }
                let model = entry.name().to_string();
                let context = entry.context().or_else(|| catalog_dev.context_for(&model));
                out.push(ModelOption {
                    provider: id.clone(),
                    model,
                    effort_levels: entry.reasoning().to_vec(),
                    context,
                });
            }
        }
        out
    }

    /// Context window of the active model: its config-declared override, else the
    /// models.dev catalog value.
    pub fn active_context(&self, catalog_dev: &ModelCatalog) -> Option<u64> {
        let (provider, model) = self.active_selection()?;
        if let Some(c) = self
            .providers
            .get(&provider)
            .and_then(|p| p.context(&model))
        {
            return Some(c);
        }
        if let Some(c) = providers::lookup(&provider)
            .and_then(|s| s.models.iter().find(|m| m.name == model))
            .and_then(|m| m.context)
        {
            return Some(c);
        }
        catalog_dev.context_for(&model)
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
    Ok(crate::llm::build(config.resolve()?))
}

/// Choose the endpoint for a provider: a config `protocol` override, else the
/// provider declaration's default (`endpoints[0]`). For `custom` (no baked endpoints) the
/// endpoint is synthesized as OpenAI + Bearer from the config `api_url`.
fn select_endpoint(
    spec: &providers::ProviderSpec,
    cfg: Option<&ProviderConfig>,
) -> Result<(Protocol, String, Auth), anyhow::Error> {
    let forced = cfg.and_then(|c| c.protocol);

    if spec.endpoints.is_empty() {
        let base = cfg
            .and_then(|c| c.api_url.clone())
            .ok_or_else(|| anyhow!("provider '{}' requires `api_url` in config", spec.id))?;
        return Ok((forced.unwrap_or(Protocol::OpenAi), base, Auth::Bearer));
    }

    let endpoint = match forced {
        Some(want) => spec
            .endpoints
            .iter()
            .find(|e| e.protocol == want)
            .ok_or_else(|| {
                let offered = spec
                    .endpoints
                    .iter()
                    .map(|e| e.protocol.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow!(
                    "provider '{}' does not offer protocol '{}'; offers: {offered}",
                    spec.id,
                    want.label()
                )
            })?,
        None => &spec.endpoints[0],
    };
    let base = cfg
        .and_then(|c| c.api_url.clone())
        .unwrap_or_else(|| endpoint.base_url.to_string());
    Ok((endpoint.protocol, base, endpoint.auth))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_options_uses_catalog_with_effort_levels() {
        // No config `models`: the picker is populated from the baked catalog.
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
[providers.deepseek]
api_key = "x"
[providers.openai]
api_key = "y"
"#,
        )
        .unwrap();
        let opts = cfg.model_options(&ModelCatalog::default());
        // Sorted by provider (deepseek before openai); 2 models each.
        assert_eq!(opts.len(), 4);
        assert_eq!(
            (opts[0].provider.as_str(), opts[0].model.as_str()),
            ("deepseek", "deepseek-v4-flash")
        );
        assert!(opts[0].effort_levels.is_empty());
        // o3 carries its baked effort levels.
        let o3 = opts.iter().find(|o| o.model == "o3").unwrap();
        assert_eq!(o3.provider, "openai");
        assert_eq!(o3.effort_levels, vec!["low", "medium", "high"]);
    }

    #[test]
    fn config_models_extend_and_override_catalog() {
        let cfg: Config = toml::from_str(
            r#"
[providers.deepseek]
api_key = "x"
models = ["my-extra", { name = "deepseek-v4-flash", reasoning = ["low", "high"] }]
"#,
        )
        .unwrap();
        let opts = cfg.model_options(&ModelCatalog::default());
        // Config effort override applied to the catalog model.
        let chat = opts
            .iter()
            .find(|o| o.model == "deepseek-v4-flash")
            .unwrap();
        assert_eq!(chat.effort_levels, vec!["low", "high"]);
        // Config-only model is appended.
        assert!(opts.iter().any(|o| o.model == "my-extra"));
    }

    #[test]
    fn inline_context_is_an_override() {
        // A config-declared `context` wins over the catalog / models.dev.
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
[providers.deepseek]
api_key = "x"
models = [{ name = "deepseek-v4-flash", context = 128000 }]
"#,
        )
        .unwrap();
        let opts = cfg.model_options(&ModelCatalog::default());
        let chat = opts
            .iter()
            .find(|o| o.model == "deepseek-v4-flash")
            .unwrap();
        assert_eq!(chat.context, Some(128000));
        assert_eq!(cfg.active_context(&ModelCatalog::default()), Some(128000));
    }

    #[test]
    fn baked_context_is_used_when_no_override() {
        // Kimi's window is baked into the catalog (models.dev doesn't know it).
        let cfg: Config = toml::from_str(
            r#"
model = "kimi-code/kimi-for-coding"
[providers.kimi-code]
api_key = "x"
"#,
        )
        .unwrap();
        assert_eq!(cfg.active_context(&ModelCatalog::default()), Some(262144));
    }

    #[test]
    fn active_selection_parses_provider_slash_model() {
        let cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-pro"
[providers.deepseek]
api_key = "x"
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
    fn active_selection_falls_back_to_first_catalog_model() {
        // No top-level `model`: first provider (sorted) → its first catalog model.
        let cfg: Config = toml::from_str(
            r#"
[providers.deepseek]
api_key = "x"
[providers.anthropic]
api_key = "y"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.active_selection(),
            Some((
                "anthropic".to_string(),
                "claude-sonnet-4-20250514".to_string()
            ))
        );
    }

    #[test]
    fn active_effort_only_returns_declared_levels() {
        // o3's levels come from the catalog — no config `models` needed.
        let cfg: Config = toml::from_str(
            r#"
model = "openai/o3"
reasoning_effort = "high"
[providers.openai]
api_key = "x"
"#,
        )
        .unwrap();
        assert_eq!(cfg.active_effort().as_deref(), Some("high"));
    }

    #[test]
    fn active_effort_none_for_model_without_levels() {
        let cfg: Config = toml::from_str(
            r#"
model = "openai/gpt-4o"
reasoning_effort = "high"
[providers.openai]
api_key = "x"
"#,
        )
        .unwrap();
        // gpt-4o declares no levels → no effort even though one is set.
        assert_eq!(cfg.active_effort(), None);
    }

    #[test]
    fn active_effort_ignores_undeclared_value() {
        let cfg: Config = toml::from_str(
            r#"
model = "openai/o3"
reasoning_effort = "ultra"
[providers.openai]
api_key = "x"
"#,
        )
        .unwrap();
        assert_eq!(cfg.active_effort(), None);
    }

    #[test]
    fn minimax_defaults_to_anthropic_then_openai_override() {
        let cfg: Config = toml::from_str(
            r#"
model = "minimax-token-plan/MiniMax-M2.7"
[providers.minimax-token-plan]
api_key = "sk-cp-x"
"#,
        )
        .unwrap();
        let r = cfg.resolve().unwrap();
        assert_eq!(r.protocol, Protocol::Anthropic);
        assert_eq!(r.base_url, "https://api.minimaxi.com/anthropic");
        assert_eq!(r.auth, Auth::Bearer);
        assert_eq!(r.model, "MiniMax-M2.7");

        let cfg: Config = toml::from_str(
            r#"
model = "minimax-token-plan/MiniMax-M2.7"
[providers.minimax-token-plan]
api_key = "sk-cp-x"
protocol = "openai"
"#,
        )
        .unwrap();
        let r = cfg.resolve().unwrap();
        assert_eq!(r.protocol, Protocol::OpenAi);
        assert_eq!(r.base_url, "https://api.minimaxi.com/v1");
    }

    #[test]
    fn resolve_unknown_provider_errors() {
        let cfg: Config = toml::from_str(
            r#"
model = "nope/m"
[providers.nope]
api_key = "x"
"#,
        )
        .unwrap();
        assert!(cfg.resolve().is_err());
    }

    #[test]
    fn resolve_missing_api_key_errors() {
        let cfg: Config = toml::from_str(
            r#"
model = "openai/gpt-4o"
[providers.openai]
"#,
        )
        .unwrap();
        assert!(cfg.resolve().is_err());
    }

    #[test]
    fn custom_synthesizes_openai_endpoint_and_requires_api_url() {
        // Without api_url → error.
        let cfg: Config = toml::from_str(
            r#"
model = "custom/my-model"
[providers.custom]
api_key = "x"
models = ["my-model"]
"#,
        )
        .unwrap();
        assert!(cfg.resolve().is_err());

        // With api_url → OpenAI + Bearer at that URL.
        let cfg: Config = toml::from_str(
            r#"
model = "custom/my-model"
[providers.custom]
api_key = "x"
api_url = "https://my.endpoint/v1"
models = ["my-model"]
"#,
        )
        .unwrap();
        let r = cfg.resolve().unwrap();
        assert_eq!(r.protocol, Protocol::OpenAi);
        assert_eq!(r.base_url, "https://my.endpoint/v1");
        assert_eq!(r.auth, Auth::Bearer);
    }

    #[test]
    fn state_overrides_config_default() {
        let mut cfg: Config = toml::from_str(
            r#"
model = "openai/gpt-4o"
[providers.openai]
api_key = "x"
"#,
        )
        .unwrap();
        cfg.apply_state(State {
            model: Some("openai/o3".to_string()),
            reasoning_effort: Some("high".to_string()),
            disabled_skills: vec![],
            disabled_mcp_servers: vec![],
            mode: None,
            permission_grants: vec![],
        });
        assert_eq!(cfg.active_model().as_deref(), Some("o3"));
        assert_eq!(cfg.active_effort().as_deref(), Some("high"));
    }

    #[test]
    fn empty_state_keeps_config_default() {
        let mut cfg: Config = toml::from_str(
            r#"
model = "deepseek/deepseek-v4-flash"
[providers.deepseek]
api_key = "x"
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
model = "openai/o3"
reasoning_effort = "high"
[providers.openai]
api_key = "x"
"#,
        )
        .unwrap();
        cfg.apply_state(State {
            model: Some("openai/gpt-4o".to_string()),
            reasoning_effort: None,
            disabled_skills: vec![],
            disabled_mcp_servers: vec![],
            mode: None,
            permission_grants: vec![],
        });
        assert_eq!(cfg.active_model().as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.active_effort(), None);
    }
}
