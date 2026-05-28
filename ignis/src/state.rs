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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcp_servers: Vec<String>,
    /// Persisted permission mode: `"off"`, `"hands_free"`, or
    /// `"fully_unattended"`. Omitted from JSON when None (= use the built-in
    /// `Off` default at next launch). Set by `/afk` (via the picker).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
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

/// Persist the disabled-MCP-servers set, preserving every other field
/// (notably the model selection and disabled-skills set).
pub fn persist_disabled_mcp_servers(disabled: &[String]) -> Result<(), anyhow::Error> {
    let mut state = load_state();
    state.disabled_mcp_servers = disabled.to_vec();
    write_state(&state)
}

/// Persist the permission `mode`, preserving every other field. Called by
/// `/afk` (which sets `Some("hands_free")` or `Some("fully_unattended")`) and
/// by toggling off (which sets `None`, omitted from JSON).
pub fn persist_permission_mode(mode: Option<&str>) -> Result<(), anyhow::Error> {
    let mut state = load_state();
    state.mode = mode.map(String::from);
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
            disabled_mcp_servers: vec![],
            mode: None,
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
            disabled_mcp_servers: vec![],
            mode: None,
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
        assert!(!json.contains("disabled_mcp_servers"));
    }

    #[test]
    fn state_round_trips_disabled_mcp_servers() {
        let state = State {
            model: None,
            reasoning_effort: None,
            disabled_skills: vec![],
            disabled_mcp_servers: vec!["github".to_string(), "fs".to_string()],
            mode: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.disabled_mcp_servers,
            vec!["github".to_string(), "fs".to_string()]
        );
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

    #[test]
    fn permission_mode_round_trip_through_state() {
        for value in ["off", "hands_free", "fully_unattended"] {
            let state = State {
                model: None,
                reasoning_effort: None,
                disabled_skills: vec![],
                disabled_mcp_servers: vec![],
                mode: Some(value.to_string()),
            };
            let json = serde_json::to_string(&state).unwrap();
            let back: State = serde_json::from_str(&json).unwrap();
            assert_eq!(
                back.mode.as_deref(),
                Some(value),
                "round-trip failed for {value}"
            );
        }
    }

    #[test]
    fn permission_mode_default_omits_from_json() {
        let state = State::default();
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            !json.contains("\"mode\""),
            "default state should omit mode, got: {json}"
        );
    }

    #[test]
    fn permission_mode_persist_preserves_siblings() {
        let _env = crate::util::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = crate::util::unique_temp_dir("ignis-state-mode-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        persist_disabled_skills(&["sql-review".to_string()]).unwrap();
        persist_permission_mode(Some("hands_free")).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.disabled_skills, vec!["sql-review".to_string()]);
        assert_eq!(s.mode.as_deref(), Some("hands_free"));

        // Clearing the mode must not touch model or skills.
        persist_permission_mode(None).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.disabled_skills, vec!["sql-review".to_string()]);
        assert!(s.mode.is_none());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn mcp_persist_preserves_model_and_skills() {
        let _env = crate::util::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = crate::util::unique_temp_dir("ignis-state-mcp-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        persist_disabled_skills(&["sql-review".to_string()]).unwrap();
        persist_disabled_mcp_servers(&["github".to_string()]).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(s.disabled_skills, vec!["sql-review".to_string()]);
        assert_eq!(s.disabled_mcp_servers, vec!["github".to_string()]);

        // Re-toggling MCP must not touch model or skills.
        persist_disabled_mcp_servers(&["github".to_string(), "fs".to_string()]).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.disabled_skills, vec!["sql-review".to_string()]);
        assert_eq!(s.disabled_mcp_servers.len(), 2);

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
