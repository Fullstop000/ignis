use crate::llm::{
    providers, Auth, LlmProvider, ModelCatalog, ModelOption, Protocol, ProviderConfig, Resolved,
};
use crate::state::{load_state, State};
use anyhow::anyhow;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

/// OpenTelemetry export. On by default — can be disabled with
/// `[telemetry] enabled = false` in config. Standard
/// OTEL_* env vars (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`,
/// `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_RESOURCE_ATTRIBUTES`, …) configure the
/// destination — ignis does not duplicate them in TOML.
#[derive(Debug, Deserialize, Clone)]
pub struct TelemetryConfig {
    #[serde(default = "default_telemetry_enabled")]
    pub enabled: bool,
}

fn default_telemetry_enabled() -> bool {
    true
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
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
        let request_headers = resolved_request_headers(spec, cfg);

        Ok(Resolved {
            provider_id: id,
            protocol,
            base_url,
            auth,
            api_key,
            model,
            request_headers,
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
    let path = match dirs::home_dir() {
        Some(home) => home.join(".ignis/config.toml"),
        // No home dir → no config file to look at; return an empty Config so
        // the TUI can still boot into no-provider mode.
        None => return Ok(empty_config_with_state()),
    };

    // Missing config is NOT an error any more — the TUI guides the user to
    // `/connect` from the empty state. Parse errors and validation errors do
    // still surface so a typo'd file gets a clear failure.
    if !path.exists() {
        return Ok(empty_config_with_state());
    }
    // One-time migration: pre-0.31 ignis wrote `config.toml` with the umask
    // default (often `0644`, world-readable). Silently tighten to `0600` —
    // best effort; a failure here must not block startup.
    let _ = tighten_secrets_mode(&path);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("Failed to read {}: {}", path.display(), e))?;
    let mut config: Config = toml::from_str(&content)
        .map_err(|e| anyhow!("Failed to parse {}: {}", path.display(), e))?;
    for name in config.mcp.servers.keys() {
        validate_mcp_server_name(name)?;
    }
    config.apply_state(load_state());
    Ok(config)
}

/// Set `0600` on a file that holds secrets (currently `~/.ignis/config.toml`,
/// which carries provider API keys). No-op on Windows. Idempotent.
#[cfg(unix)]
fn tighten_secrets_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::metadata(path)?.permissions();
    if perms.mode() & 0o777 != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
#[cfg(not(unix))]
fn tighten_secrets_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A `Config::default()` with the persisted state overlaid. Used when no
/// `~/.ignis/config.toml` exists — the TUI still needs to honor the user's
/// last-picked model from `state.json` if one was saved.
fn empty_config_with_state() -> Config {
    let mut cfg = Config::default();
    cfg.apply_state(load_state());
    cfg
}

/// Path to `~/.ignis/config.toml`, creating the parent directory if needed.
/// Used by writers (e.g. `write_provider_key`) that need to create the file
/// on first connect.
fn config_path() -> Result<PathBuf, anyhow::Error> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("Could not locate home directory"))?;
    let dir = home.join(".ignis");
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("Failed to create {}: {}", dir.display(), e))?;
    Ok(dir.join("config.toml"))
}

/// Set `[providers.<id>] api_key = "<key>"` in `~/.ignis/config.toml`,
/// creating the file if absent and preserving any existing comments / other
/// tables (uses `toml_edit`, not `toml`). If the table already exists with
/// other fields (e.g. `api_url`, `protocol`), those are left untouched.
///
/// Returns the path written, for the TUI to surface in its success message.
pub fn write_provider_key(provider_id: &str, api_key: &str) -> Result<PathBuf, anyhow::Error> {
    let path = config_path()?;
    let mut doc = if path.exists() {
        std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("Failed to read {}: {}", path.display(), e))?
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| anyhow!("Failed to parse {}: {}", path.display(), e))?
    } else {
        toml_edit::DocumentMut::new()
    };

    // Ensure `[providers]` exists as a table.
    if !doc.contains_key("providers") {
        doc["providers"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let providers = doc["providers"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("`providers` in config.toml is not a table"))?;

    // Ensure `[providers.<id>]` exists, then set api_key without clobbering
    // anything else under it.
    if !providers.contains_key(provider_id) {
        providers.insert(provider_id, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let entry = providers[provider_id]
        .as_table_mut()
        .ok_or_else(|| anyhow!("`providers.{provider_id}` is not a table"))?;
    entry["api_key"] = toml_edit::value(api_key);

    // Atomic write: a crash mid-write on `config.toml` would leave a truncated
    // file and the next launch can't recover (a parse failure halts startup).
    // Write to a sibling tmpfile on the same filesystem, then rename — rename
    // is atomic on POSIX, so either the old or new content is observable, never
    // a partial.
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("Config path has no parent directory: {}", path.display()))?;
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, doc.to_string())
        .map_err(|e| anyhow!("Failed to write {}: {}", tmp.display(), e))?;
    // Tighten to `0600` before the rename so the file is never observable at
    // the umask default (often `0644`) while it carries an API key.
    tighten_secrets_mode(&tmp)
        .map_err(|e| anyhow!("Failed to chmod 0600 {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| anyhow!("Failed to atomically replace {}: {}", path.display(), e))?;
    Ok(path)
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

fn resolved_request_headers(
    spec: &providers::ProviderSpec,
    cfg: Option<&ProviderConfig>,
) -> Vec<(String, String)> {
    let mut headers = spec
        .request_headers
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect::<Vec<_>>();

    if let Some(user_agent) = cfg.and_then(|c| c.user_agent.clone()) {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("User-Agent"));
        headers.push(("User-Agent".to_string(), user_agent));
    }

    headers
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
        assert_eq!(opts[0].effort_levels, vec!["high", "max"]);
        // GPT-5.5 carries its baked effort levels.
        let gpt55 = opts.iter().find(|o| o.model == "gpt-5.5").unwrap();
        assert_eq!(gpt55.provider, "openai");
        assert_eq!(
            gpt55.effort_levels,
            vec!["none", "low", "medium", "high", "xhigh"]
        );
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
            Some(("anthropic".to_string(), "claude-sonnet-4-6".to_string()))
        );
    }

    #[test]
    fn active_effort_only_returns_declared_levels() {
        // GPT-5.5's levels come from the catalog — no config `models` needed.
        let cfg: Config = toml::from_str(
            r#"
model = "openai/gpt-5.5"
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
model = "anthropic/claude-sonnet-4-6"
reasoning_effort = "high"
[providers.anthropic]
api_key = "x"
"#,
        )
        .unwrap();
        // Anthropic declarations expose no effort levels to this config path.
        assert_eq!(cfg.active_effort(), None);
    }

    #[test]
    fn active_effort_ignores_undeclared_value() {
        let cfg: Config = toml::from_str(
            r#"
model = "openai/gpt-5.5"
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
    fn baked_request_headers_are_resolved_and_user_agent_can_override() {
        let cfg: Config = toml::from_str(
            r#"
model = "kimi-code/kimi-for-coding"
[providers.kimi-code]
api_key = "x"
"#,
        )
        .unwrap();
        let r = cfg.resolve().unwrap();
        assert_eq!(
            r.request_headers,
            vec![("User-Agent".to_string(), "KimiCLI/1.44.0".to_string())]
        );

        let cfg: Config = toml::from_str(
            r#"
model = "kimi-code/kimi-for-coding"
[providers.kimi-code]
api_key = "x"
user_agent = "CustomClient/1.0"
"#,
        )
        .unwrap();
        let r = cfg.resolve().unwrap();
        assert_eq!(
            r.request_headers,
            vec![("User-Agent".to_string(), "CustomClient/1.0".to_string())]
        );
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
model = "openai/gpt-5.5"
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
            model: Some("openai/gpt-5.4-mini".to_string()),
            reasoning_effort: Some("high".to_string()),
            disabled_skills: vec![],
            disabled_mcp_servers: vec![],
            mode: None,
            permission_grants: vec![],
            update_check: None,
        });
        assert_eq!(cfg.active_model().as_deref(), Some("gpt-5.4-mini"));
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
    fn empty_config_is_no_provider_state() {
        // The TUI uses these signals to decide whether to render the
        // no-provider welcome and route /connect.
        let cfg = Config::default();
        assert!(cfg.providers.is_empty());
        assert_eq!(cfg.active_provider(), None);
        assert_eq!(cfg.active_model(), None);
        assert!(cfg.resolve().is_err());
    }

    #[test]
    fn write_provider_key_creates_new_table_preserving_other_content() {
        // Roundtrip through toml_edit must NOT clobber a hand-written comment
        // or an unrelated section. This is the property `toml` (re-serialize)
        // can't promise — that's the whole reason we picked toml_edit.
        let tmp = crate::util::unique_temp_dir("ignis-cfg-write");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("config.toml");
        let original = r#"# user-written header comment
model = "openai/gpt-5.5"

[providers.openai]
api_key = "sk-old"

[mcp.servers.gh]
command = "gh"
"#;
        std::fs::write(&path, original).unwrap();

        // Direct doc manipulation (mirrors what write_provider_key does, but
        // with an explicit path so the test isn't HOME-dependent).
        let mut doc = std::fs::read_to_string(&path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        if !doc.contains_key("providers") {
            doc["providers"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let providers = doc["providers"].as_table_mut().unwrap();
        if !providers.contains_key("anthropic") {
            providers.insert("anthropic", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        providers["anthropic"]["api_key"] = toml_edit::value("sk-ant-new");
        providers["openai"]["api_key"] = toml_edit::value("sk-new");
        std::fs::write(&path, doc.to_string()).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("# user-written header comment"),
            "header comment preserved"
        );
        assert!(
            after.contains("[mcp.servers.gh]"),
            "unrelated table preserved"
        );
        assert!(after.contains("sk-new"), "openai key updated");
        assert!(after.contains("sk-ant-new"), "anthropic key inserted");
        assert!(!after.contains("sk-old"), "old openai key gone");

        // The file must still parse as a Config — guards against producing a
        // schema-broken toml.
        let cfg: Config = toml::from_str(&after).unwrap();
        assert_eq!(
            cfg.providers
                .get("openai")
                .and_then(|p| p.api_key.as_deref()),
            Some("sk-new")
        );
        assert_eq!(
            cfg.providers
                .get("anthropic")
                .and_then(|p| p.api_key.as_deref()),
            Some("sk-ant-new")
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn state_with_model_clears_stale_effort() {
        // Switching to a non-reasoning model via state drops a prior effort.
        let mut cfg: Config = toml::from_str(
            r#"
model = "openai/gpt-5.5"
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
            update_check: None,
        });
        assert_eq!(cfg.active_model().as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.active_effort(), None);
    }

    #[cfg(unix)]
    #[test]
    fn tighten_secrets_mode_migrates_0644_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::util::unique_temp_dir("ignis-cfg-perm");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("config.toml");
        std::fs::write(&path, "model = \"x/y\"\n").unwrap();
        // Simulate a pre-fix file written at the umask default.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        super::tighten_secrets_mode(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file must end at 0600");

        // Idempotent: a second call doesn't widen or error.
        super::tighten_secrets_mode(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
