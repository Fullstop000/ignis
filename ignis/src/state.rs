//! Machine-written runtime state, kept apart from the hand-authored
//! `config.toml`. Currently just the active `/model` selection, persisted to
//! `~/.ignis/state.json` and overlaid onto [`crate::config::Config`] at load.

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

/// Cached result of the most recent auto-update check. Lets the TUI surface
/// "new version available" in the footer without firing a network call on
/// every launch (TTL gate lives in `cli::upgrade::check_for_update_cached`).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckState {
    /// Unix-epoch seconds when the GitHub-Releases fetch last succeeded. A
    /// missing record (None on State) means "never checked"; a present record
    /// with `checked_at` older than the TTL means "re-check on next launch."
    pub checked_at: u64,
    /// The `tag_name` GitHub returned at `checked_at` (e.g. `v0.31.0`).
    pub latest_tag: String,
}

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
    /// Persisted "always allow" permission grants — `Tool(pattern)` strings in
    /// the same grammar as `config.toml`'s `[permissions]`, folded into the
    /// `allow` list at launch. Appended when the user picks "Always allow".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_grants: Vec<String>,
    /// Cached auto-update-check result. Missing on first launch and on any
    /// state file written before this field existed (serde defaults to None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_check: Option<UpdateCheckState>,
    /// Footer segments hidden via `/settings` → Statusline (segment ids like
    /// `"git"`, `"turns"`). Empty / missing = every segment shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statusline_hidden: Vec<String>,
    /// Whether the bash auto-run sandbox is active in the unattended (AFK /
    /// headless) modes. Default / missing = `false` = OFF, so an AFK run is
    /// unsandboxed (credentialed commands like `git push` work) unless the user
    /// opts in via `/sandbox` (Ink) or `/settings` (native). Persisted on each
    /// toggle. Has no effect in interactive `Off` mode (never sandboxed).
    #[serde(default, skip_serializing_if = "is_false")]
    pub sandbox_enabled: bool,
    /// TUI override for `[compaction] auto` in `config.toml`. `None` (missing) =
    /// the TUI never set it ⇒ fall back to `config.toml` / the built-in default
    /// (`true`); `Some(v)` overlays the config. Set by `/settings` → Context.
    /// `Option` (not bool) so a default state doesn't override config's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_auto: Option<bool>,
    /// TUI override for `[settings] strip-think` in `config.toml`. Same overlay
    /// semantics as [`State::compaction_auto`]: `None` ⇒ config / default
    /// (`true`); `Some(v)` overlays. Set by `/settings` → Context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_think: Option<bool>,
}

/// `skip_serializing_if` predicate for the boolean toggles above — keeps a
/// default-`false` flag out of `state.json` (matching the Option/Vec fields,
/// which omit their empty defaults too).
fn is_false(b: &bool) -> bool {
    !*b
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
    // Atomic write: a crash mid-write would leave state.json truncated, and
    // load_state silently swallows parse failures into State::default — that
    // would erase the model selection + permission grants. With a background
    // writer now landing on this file (auto-update check), the corruption
    // window is also a TOCTOU window worth shrinking. Write to a sibling
    // tmpfile + rename — atomic on POSIX, so either old or new content is
    // observable, never a partial.
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("state path has no parent: {}", path.display()))?;
    let tmp = dir.join(format!(".state.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)
        .map_err(|e| anyhow!("write {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| anyhow!("atomically replace {}: {}", path.display(), e))?;
    Ok(())
}

/// Serializes the read-modify-write cycle of the `persist_*` helpers below.
/// Without it, the detached auto-update-check task (`cli::upgrade`) can read a
/// stale snapshot and write it back seconds later, clobbering a `/model`,
/// `/afk`, or grant the user persisted in between (a lost update). The atomic
/// rename in `write_state` prevents a *torn* file, but not interleaved RMW.
static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Load state, apply `mutate`, and write it back — atomically and under the
/// global lock, so two concurrent persists can't lose each other's updates.
fn update_state(mutate: impl FnOnce(&mut State)) -> Result<(), anyhow::Error> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state();
    mutate(&mut state);
    write_state(&state)
}

/// Persist a `/model` selection, preserving any other fields already in
/// `state.json` (notably `disabled_skills`).
pub fn persist_model_selection(
    provider: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<(), anyhow::Error> {
    update_state(|state| {
        state.model = Some(format!("{provider}/{model}"));
        state.reasoning_effort = effort.map(str::to_string);
    })
}

/// Persist the disabled-skills set, preserving the model selection.
pub fn persist_disabled_skills(disabled: &[String]) -> Result<(), anyhow::Error> {
    update_state(|state| state.disabled_skills = disabled.to_vec())
}

/// Persist the disabled-MCP-servers set, preserving every other field
/// (notably the model selection and disabled-skills set).
pub fn persist_disabled_mcp_servers(disabled: &[String]) -> Result<(), anyhow::Error> {
    update_state(|state| state.disabled_mcp_servers = disabled.to_vec())
}

/// Persist the permission `mode`, preserving every other field. Called by
/// `/afk` (which sets `Some("hands_free")` or `Some("fully_unattended")`) and
/// by toggling off (which sets `None`, omitted from JSON).
pub fn persist_permission_mode(mode: Option<&str>) -> Result<(), anyhow::Error> {
    update_state(|state| state.mode = mode.map(String::from))
}

/// Persist the "always allow" permission grants, preserving every other field.
/// Called when the user picks "Always allow" in the permission picker.
pub fn persist_permission_grants(grants: &[String]) -> Result<(), anyhow::Error> {
    update_state(|state| state.permission_grants = grants.to_vec())
}

/// Persist the cached auto-update-check result. `None` clears the cache (the
/// next launch will re-check). Preserves every other field.
pub fn persist_update_check(check: Option<UpdateCheckState>) -> Result<(), anyhow::Error> {
    update_state(|state| state.update_check = check)
}

/// Persist the hidden-footer-segments set, preserving every other field.
/// Called by the `/settings` Statusline tab on each toggle.
pub fn persist_statusline_hidden(hidden: &[String]) -> Result<(), anyhow::Error> {
    update_state(|state| state.statusline_hidden = hidden.to_vec())
}

/// Persist the bash-sandbox on/off toggle, preserving every other field.
/// Called by `/sandbox` (Ink) and the `/settings` Sandbox tab (native).
pub fn persist_sandbox_enabled(enabled: bool) -> Result<(), anyhow::Error> {
    update_state(|state| state.sandbox_enabled = enabled)
}

/// Persist the auto-compaction TUI override, preserving every other field.
/// Called by `/settings` → Context. Always writes `Some(_)` (the panel only
/// produces explicit on/off).
pub fn persist_compaction_auto(enabled: bool) -> Result<(), anyhow::Error> {
    update_state(|state| state.compaction_auto = Some(enabled))
}

/// Persist the strip-reasoning-from-history TUI override, preserving every other
/// field. Called by `/settings` → Context.
pub fn persist_strip_think(enabled: bool) -> Result<(), anyhow::Error> {
    update_state(|state| state.strip_think = Some(enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_grants_round_trip() {
        let state = State {
            permission_grants: vec!["bash(git status *)".to_string()],
            ..State::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.permission_grants,
            vec!["bash(git status *)".to_string()]
        );
    }

    #[test]
    fn empty_permission_grants_omitted_from_json() {
        let json = serde_json::to_string(&State::default()).unwrap();
        assert!(!json.contains("permission_grants"));
    }

    #[test]
    fn persist_permission_grants_preserves_siblings() {
        let tmp = crate::util::unique_temp_dir("ignis-state-grants-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        persist_permission_grants(&["bash(git status *)".to_string()]).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.permission_grants, vec!["bash(git status *)".to_string()]);

        // Re-saving grants must not touch the model selection.
        persist_permission_grants(&[
            "bash(git status *)".to_string(),
            "edit_file(src/main.rs)".to_string(),
        ])
        .unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.permission_grants.len(), 2);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn state_round_trips_as_json() {
        let state = State {
            model: Some("deepseek/deepseek-v4-pro".to_string()),
            reasoning_effort: Some("max".to_string()),
            disabled_skills: vec![],
            disabled_mcp_servers: vec![],
            mode: None,
            permission_grants: vec![],
            update_check: None,
            statusline_hidden: vec![],
            sandbox_enabled: false,
            compaction_auto: None,
            strip_think: None,
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
            permission_grants: vec![],
            update_check: None,
            statusline_hidden: vec![],
            sandbox_enabled: false,
            compaction_auto: None,
            strip_think: None,
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
            permission_grants: vec![],
            update_check: None,
            statusline_hidden: vec![],
            sandbox_enabled: false,
            compaction_auto: None,
            strip_think: None,
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
        let tmp = crate::util::unique_temp_dir("ignis-state-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        persist_disabled_skills(&["sql-review".to_string()]).unwrap();
        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        let s = load_state();
        assert_eq!(s.disabled_skills, vec!["sql-review".to_string()]);
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));

        persist_disabled_skills(&["sql-review".to_string(), "x".to_string()]).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.disabled_skills.len(), 2);

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
                permission_grants: vec![],
                update_check: None,
                statusline_hidden: vec![],
                sandbox_enabled: false,
                compaction_auto: None,
                strip_think: None,
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
        let tmp = crate::util::unique_temp_dir("ignis-state-mode-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

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

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn update_check_round_trip() {
        let state = State {
            update_check: Some(UpdateCheckState {
                checked_at: 1_717_180_000,
                latest_tag: "v0.31.0".to_string(),
            }),
            ..State::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        let back_uc = back.update_check.unwrap();
        assert_eq!(back_uc.checked_at, 1_717_180_000);
        assert_eq!(back_uc.latest_tag, "v0.31.0");
    }

    #[test]
    fn update_check_absent_in_legacy_json_loads_as_none() {
        // A state file written before update_check existed must still load —
        // serde(default) handles this; here we just lock the contract in.
        let legacy = r#"{"model":"openai/gpt-5.5"}"#;
        let back: State = serde_json::from_str(legacy).unwrap();
        assert!(back.update_check.is_none());
    }

    #[test]
    fn update_check_none_omitted_from_json() {
        let json = serde_json::to_string(&State::default()).unwrap();
        assert!(!json.contains("update_check"));
    }

    #[test]
    fn statusline_hidden_round_trip_and_default_omitted() {
        let json = serde_json::to_string(&State::default()).unwrap();
        assert!(!json.contains("statusline_hidden"), "default omits it");
        let state = State {
            statusline_hidden: vec!["git".to_string(), "turns".to_string()],
            ..State::default()
        };
        let back: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            back.statusline_hidden,
            vec!["git".to_string(), "turns".to_string()]
        );
    }

    #[test]
    fn persist_statusline_hidden_preserves_siblings() {
        let tmp = crate::util::unique_temp_dir("ignis-state-statusline-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        persist_statusline_hidden(&["git".to_string()]).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.statusline_hidden, vec!["git".to_string()]);

        // Re-saving the model must not drop the hidden set.
        persist_model_selection("openai", "gpt-5.5", None).unwrap();
        let s = load_state();
        assert_eq!(s.statusline_hidden, vec!["git".to_string()]);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sandbox_enabled_round_trip_and_default_omitted() {
        // Default is false and omitted from JSON (so legacy state files load it
        // as the OFF default).
        let json = serde_json::to_string(&State::default()).unwrap();
        assert!(!json.contains("sandbox_enabled"), "default omits it");
        let legacy = r#"{"model":"openai/gpt-5.5"}"#;
        let back: State = serde_json::from_str(legacy).unwrap();
        assert!(!back.sandbox_enabled);
        // True survives a round-trip and IS written.
        let state = State {
            sandbox_enabled: true,
            ..State::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("sandbox_enabled"));
        let back: State = serde_json::from_str(&json).unwrap();
        assert!(back.sandbox_enabled);
    }

    #[test]
    fn persist_sandbox_enabled_preserves_siblings() {
        let tmp = crate::util::unique_temp_dir("ignis-state-sandbox-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        persist_sandbox_enabled(true).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert!(s.sandbox_enabled);

        // Toggling it back off must not drop the model selection.
        persist_sandbox_enabled(false).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert!(!s.sandbox_enabled);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn context_overrides_round_trip_and_default_omitted() {
        // Both default to None and are omitted (so config.toml / built-in
        // defaults apply); a legacy state file loads them as None.
        let json = serde_json::to_string(&State::default()).unwrap();
        assert!(!json.contains("compaction_auto"), "default omits it");
        assert!(!json.contains("strip_think"), "default omits it");
        let legacy = r#"{"model":"openai/gpt-5.5"}"#;
        let back: State = serde_json::from_str(legacy).unwrap();
        assert_eq!(back.compaction_auto, None);
        assert_eq!(back.strip_think, None);
        // Explicit values survive a round-trip and ARE written.
        let state = State {
            compaction_auto: Some(false),
            strip_think: Some(false),
            ..State::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("compaction_auto"));
        assert!(json.contains("strip_think"));
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.compaction_auto, Some(false));
        assert_eq!(back.strip_think, Some(false));
    }

    #[test]
    fn persist_context_overrides_preserve_siblings() {
        let tmp = crate::util::unique_temp_dir("ignis-state-context-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        persist_model_selection("deepseek", "deepseek-v4-pro", Some("high")).unwrap();
        persist_compaction_auto(false).unwrap();
        persist_strip_think(false).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.compaction_auto, Some(false));
        assert_eq!(s.strip_think, Some(false));

        // Re-toggling one must not drop the model or the other override.
        persist_compaction_auto(true).unwrap();
        let s = load_state();
        assert_eq!(s.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(s.compaction_auto, Some(true));
        assert_eq!(s.strip_think, Some(false));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn mcp_persist_preserves_model_and_skills() {
        let tmp = crate::util::unique_temp_dir("ignis-state-mcp-rmw");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

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

        std::fs::remove_dir_all(&tmp).ok();
    }
}
