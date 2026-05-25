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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
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

fn write_state(state: &State) -> Result<(), anyhow::Error> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

/// Persist a `/model` selection, preserving any other fields already in
/// `state.json` (notably `disabled_skills`).
pub fn persist_model_selection(
    provider: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut state = load_state();
    state.model = Some(format!("{provider}/{model}"));
    state.reasoning_effort = effort.map(str::to_string);
    write_state(&state)
}

/// Persist the disabled-skills set, preserving the model selection.
pub fn persist_disabled_skills(disabled: &[String]) -> Result<(), anyhow::Error> {
    let mut state = load_state();
    state.disabled_skills = disabled.to_vec();
    write_state(&state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_as_json() {
        let state = State {
            model: Some("deepseek/deepseek-v4-pro".to_string()),
            reasoning_effort: Some("max".to_string()),
            disabled_skills: vec![],
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(back.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn state_round_trips_disabled_skills() {
        let state = State {
            model: Some("deepseek/deepseek-v4-pro".to_string()),
            reasoning_effort: None,
            disabled_skills: vec!["a".to_string(), "b".to_string()],
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.disabled_skills, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn empty_disabled_skills_is_omitted_from_json() {
        let state = State::default();
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("disabled_skills"));
    }

    #[test]
    fn model_persist_preserves_disabled_skills_and_vice_versa() {
        let _env = crate::util::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = crate::util::unique_temp_dir("ignis-state-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        persist_disabled_skills(&["sql-review".to_string()]).unwrap();
        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        let s = load_state();
        assert_eq!(s.disabled_skills, vec!["sql-review".to_string()]);
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));

        persist_disabled_skills(&["sql-review".to_string(), "x".to_string()]).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.disabled_skills.len(), 2);

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
