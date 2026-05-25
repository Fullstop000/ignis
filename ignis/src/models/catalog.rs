//! Model metadata from [models.dev](https://models.dev) — currently just
//! context-window sizes. The catalog (`api.json`) is cached at
//! `~/.ignis/models.json` and refreshed in the background; lookups are by model
//! id across all providers, so a config `model` name maps to its window without
//! caring which provider id models.dev files it under. Config-declared
//! `[providers.X.context]` always wins over this (see [`crate::config`]).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const API_URL: &str = "https://models.dev/api.json";
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60); // refresh weekly

#[derive(Deserialize)]
struct Api {
    #[serde(flatten)]
    providers: HashMap<String, ApiProvider>,
}

#[derive(Deserialize)]
struct ApiProvider {
    #[serde(default)]
    models: HashMap<String, ApiModel>,
}

#[derive(Deserialize)]
struct ApiModel {
    limit: Option<ApiLimit>,
}

#[derive(Deserialize)]
struct ApiLimit {
    context: Option<u64>,
}

/// A flattened `model id -> context window (tokens)` lookup built from the cache.
#[derive(Default)]
pub struct ModelCatalog {
    by_model: HashMap<String, u64>,
}

impl ModelCatalog {
    /// The context window for `model`, if models.dev knows it.
    pub fn context_for(&self, model: &str) -> Option<u64> {
        self.by_model.get(model).copied()
    }
}

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ignis/models.json"))
}

fn parse(text: &str) -> Option<ModelCatalog> {
    let api: Api = serde_json::from_str(text).ok()?;
    let mut by_model = HashMap::new();
    for provider in api.providers.values() {
        for (id, model) in &provider.models {
            if let Some(ctx) = model.limit.as_ref().and_then(|l| l.context) {
                by_model.entry(id.clone()).or_insert(ctx);
            }
        }
    }
    Some(ModelCatalog { by_model })
}

/// Load the cached catalog from disk; empty if it's absent or unparseable.
pub fn load() -> ModelCatalog {
    cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| parse(&t))
        .unwrap_or_default()
}

/// `true` when the cache is missing or older than [`MAX_AGE`].
fn is_stale() -> bool {
    let Some(path) = cache_path() else {
        return false; // no home dir → nowhere to cache; don't fetch
    };
    match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .map(|age| age > MAX_AGE)
            .unwrap_or(true),
        Err(_) => true, // missing
    }
}

/// Best-effort background refresh: if the cache is missing or stale, fetch
/// models.dev and rewrite it (for the *next* launch). Network/parse failures are
/// silently ignored — the feature is purely additive.
pub async fn refresh_if_stale() {
    if !is_stale() {
        return;
    }
    let Some(path) = cache_path() else { return };
    let fetched = async {
        let resp = reqwest::Client::new()
            .get(API_URL)
            .header("User-Agent", "ignis")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        resp.text().await.ok()
    }
    .await;
    let Some(text) = fetched else { return };
    if parse(&text).is_none() {
        return; // don't cache garbage
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flattens_models_to_context() {
        let json = r#"{
            "deepseek": { "models": {
                "deepseek-chat": { "limit": { "context": 128000, "output": 8192 } }
            }},
            "anthropic": { "models": {
                "claude-x": { "limit": { "context": 200000 } },
                "no-limit": {}
            }}
        }"#;
        let cat = parse(json).unwrap();
        assert_eq!(cat.context_for("deepseek-chat"), Some(128000));
        assert_eq!(cat.context_for("claude-x"), Some(200000));
        assert_eq!(cat.context_for("no-limit"), None);
        assert_eq!(cat.context_for("unknown"), None);
    }
}
