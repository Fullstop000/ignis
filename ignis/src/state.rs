//! Machine-written runtime state, kept apart from the hand-authored
//! `config.toml`. Currently just the active `/model` selection, persisted to
//! `~/.ignis/state.json` and overlaid onto [`crate::config::Config`] at load.

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

/// Runtime state persisted across restarts. The selection it carries takes
/// priority over the config's optional default; `config.toml` is never rewritten.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

fn state_path() -> Result<std::path::PathBuf, anyhow::Error> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("could not determine home directory"))?
        .join(".ignis/state.json"))
}

/// Load `~/.ignis/state.json`; missing or unparseable yields the default.
pub fn load_state() -> State {
    state_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist a `/model` selection to `~/.ignis/state.json` (config is never
/// touched). The selection takes priority over the config's optional default.
pub fn persist_model_selection(
    provider: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<(), anyhow::Error> {
    let state = State {
        model: Some(format!("{provider}/{model}")),
        reasoning_effort: effort.map(str::to_string),
    };
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_as_json() {
        let state = State {
            model: Some("deepseek/deepseek-v4-pro".to_string()),
            reasoning_effort: Some("max".to_string()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(back.reasoning_effort.as_deref(), Some("max"));
    }
}
