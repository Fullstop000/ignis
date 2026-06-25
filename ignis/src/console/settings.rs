//! The generic `/settings` registry — the single place a config knob is
//! declared. [`build_settings`] reads current values; [`apply_setting`] writes
//! them. **Adding a knob is one entry here** (plus its effect); the protocol
//! descriptor ([`crate::console::frontend::protocol::Setting`]) and the Ink
//! panel that renders it don't change. That invariant is the whole point.

use crate::config::Config;
use crate::console::frontend::protocol::{Setting, SettingKind};
use crate::permissions::runtime::PermissionState;
use crate::state::{self, State};

/// What the runner must do after [`apply_setting`] persists a knob. Most knobs
/// take effect immediately (the live `PermissionState` flip, or the next
/// snapshot reading `state.json`); the `config.toml`-overlay knobs need the
/// agent loop to re-read its merged config so the next prompt honors them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Persisted; nothing more for the runner to do.
    None,
    /// The agent loop must re-run `load_config()` (an `AgentRequest::ReloadConfig`)
    /// so the new overlaid value reaches the next prompt.
    ReloadConfig,
}

/// Footer segments Ink can show/hide, in display order: `(id, label, help)`.
/// Each `id` becomes `statusline.<id>` and matches the shared `statusline_hidden`
/// keys, so native `/settings` and Ink stay consistent.
const STATUSLINE_SEGMENTS: &[(&str, &str, &str)] = &[
    ("model", "Model", "provider / model in the footer"),
    (
        "cwd",
        "Working directory",
        "current directory in the footer",
    ),
    (
        "git",
        "Git branch",
        "current git branch in the footer (oh-my-zsh style)",
    ),
    ("turns", "Turns", "turn count in the footer"),
    (
        "tokens",
        "Tokens / context %",
        "token + context-fill gauge in the footer",
    ),
];

fn bool_setting(id: String, label: &str, help: &str, section: &str, value: bool) -> Setting {
    Setting {
        id,
        label: label.to_string(),
        help: help.to_string(),
        section: section.to_string(),
        kind: SettingKind::Bool,
        value,
    }
}

/// Build the current settings list from live runtime (`perms`) + persisted
/// `state`. THIS is the registry: every knob the `/settings` panel shows is
/// declared here, in display order. To add one, append a `bool_setting(...)`
/// and give it an arm in [`apply_setting`].
pub fn build_settings(perms: &PermissionState, state: &State, config: &Config) -> Vec<Setting> {
    let mut out = vec![bool_setting(
        "sandbox_enabled".to_string(),
        "Sandbox auto-run bash",
        "Confine unattended (AFK / headless) bash to the project + temp and away from \
         $HOME secrets. Off by default so git push etc. work out of the box.",
        "General",
        perms.sandbox_enabled(),
    )];
    // Context knobs read the *merged* config (`config.toml` with any `state.json`
    // TUI override already overlaid by `Config::apply_state`), so the panel shows
    // the effective value. `strip-think` defaults to ON when unset.
    out.push(bool_setting(
        "compaction_auto".to_string(),
        "Auto-compaction",
        "Automatically compact the conversation before a prompt when it grows large, \
         to stay under the context window. On by default.",
        "Context",
        config.compaction.auto,
    ));
    out.push(bool_setting(
        "strip_think".to_string(),
        "Strip reasoning from history",
        "Drop prior-turn reasoning (<think> blocks) from the history sent to the model. \
         Cache-stable; on by default.",
        "Context",
        config.settings.strip_think.unwrap_or(true),
    ));
    for (seg, label, help) in STATUSLINE_SEGMENTS {
        let shown = !state.statusline_hidden.iter().any(|h| h == seg);
        out.push(bool_setting(
            format!("statusline.{seg}"),
            label,
            help,
            "Statusline",
            shown,
        ));
    }
    out
}

/// Apply a `SetSetting` from the frontend: update the live + persisted value for
/// `id`. Each arm is the one place a knob's effect lives. Unknown ids are
/// ignored (a stale or newer frontend must never crash the engine).
pub fn apply_setting(id: &str, value: bool, perms: &PermissionState) -> Effect {
    match id {
        "sandbox_enabled" => {
            perms.set_sandbox_enabled(value);
            let _ = state::persist_sandbox_enabled(value);
            Effect::None
        }
        // Context knobs persist a `state.json` override that overlays
        // `config.toml`; the agent loop must reload to pick it up live.
        "compaction_auto" => {
            let _ = state::persist_compaction_auto(value);
            Effect::ReloadConfig
        }
        "strip_think" => {
            let _ = state::persist_strip_think(value);
            Effect::ReloadConfig
        }
        _ if id.starts_with("statusline.") => {
            let seg = id.trim_start_matches("statusline.");
            // RMW the shared hidden set. `value` = shown ⇒ remove from hidden.
            let mut hidden = state::load_state().statusline_hidden;
            let present = hidden.iter().any(|h| h == seg);
            if value && present {
                hidden.retain(|h| h != seg);
            } else if !value && !present {
                hidden.push(seg.to_string());
            } else {
                return Effect::None; // already in the desired state — no write
            }
            let _ = state::persist_statusline_hidden(&hidden);
            Effect::None
        }
        other => {
            log::warn!("SetSetting: unknown setting id {other:?} (ignored)");
            Effect::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Mode;

    fn find<'a>(settings: &'a [Setting], id: &str) -> &'a Setting {
        settings
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("setting {id} missing"))
    }

    #[test]
    fn build_reflects_sandbox_flag() {
        let perms = PermissionState::new(Mode::HandsFree);
        let state = State::default();
        let cfg = Config::default();
        assert!(!find(&build_settings(&perms, &state, &cfg), "sandbox_enabled").value);
        perms.set_sandbox_enabled(true);
        assert!(find(&build_settings(&perms, &state, &cfg), "sandbox_enabled").value);
    }

    #[test]
    fn build_reflects_statusline_hidden() {
        let perms = PermissionState::new(Mode::Off);
        let state = State {
            statusline_hidden: vec!["cwd".to_string()],
            ..State::default()
        };
        let settings = build_settings(&perms, &state, &Config::default());
        // Hidden segment → shown=false; others → shown=true.
        assert!(!find(&settings, "statusline.cwd").value);
        assert!(find(&settings, "statusline.model").value);
        // Section + kind are set for every knob.
        let s = find(&settings, "sandbox_enabled");
        assert_eq!(s.section, "General");
        assert!(matches!(s.kind, SettingKind::Bool));
        assert_eq!(find(&settings, "statusline.cwd").section, "Statusline");
    }

    #[test]
    fn build_reflects_merged_context_settings() {
        let perms = PermissionState::new(Mode::Off);
        let state = State::default();
        // Defaults: both ON (auto-compaction default true, strip-think unset⇒true).
        let settings = build_settings(&perms, &state, &Config::default());
        assert!(find(&settings, "compaction_auto").value);
        assert!(find(&settings, "strip_think").value);
        assert_eq!(find(&settings, "compaction_auto").section, "Context");
        assert_eq!(find(&settings, "strip_think").section, "Context");

        // A merged config with both off (as Config::apply_state would leave it)
        // is reflected verbatim.
        let mut cfg = Config::default();
        cfg.compaction.auto = false;
        cfg.settings.strip_think = Some(false);
        let settings = build_settings(&perms, &state, &cfg);
        assert!(!find(&settings, "compaction_auto").value);
        assert!(!find(&settings, "strip_think").value);
    }

    #[test]
    fn apply_context_settings_persist_state_and_request_reload() {
        let tmp = crate::util::unique_temp_dir("ignis-settings-context");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        let perms = PermissionState::new(Mode::Off);
        assert_eq!(
            apply_setting("compaction_auto", false, &perms),
            Effect::ReloadConfig
        );
        assert_eq!(state::load_state().compaction_auto, Some(false));
        assert_eq!(
            apply_setting("strip_think", false, &perms),
            Effect::ReloadConfig
        );
        assert_eq!(state::load_state().strip_think, Some(false));
        // Both overrides coexist (siblings preserved).
        assert_eq!(state::load_state().compaction_auto, Some(false));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn apply_sandbox_flips_live_flag_and_persists() {
        let tmp = crate::util::unique_temp_dir("ignis-settings-sandbox");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        let perms = PermissionState::new(Mode::HandsFree);
        assert_eq!(
            apply_setting("sandbox_enabled", true, &perms),
            Effect::None,
            "state.json knob needs no config reload"
        );
        assert!(perms.sandbox_enabled(), "live flag flipped");
        assert!(state::load_state().sandbox_enabled, "persisted");

        apply_setting("sandbox_enabled", false, &perms);
        assert!(!perms.sandbox_enabled());
        assert!(!state::load_state().sandbox_enabled);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn apply_statusline_toggles_hidden_set_and_persists() {
        let tmp = crate::util::unique_temp_dir("ignis-settings-statusline");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        let perms = PermissionState::new(Mode::Off);
        // Hide "cwd" (value=false ⇒ not shown).
        apply_setting("statusline.cwd", false, &perms);
        assert_eq!(
            state::load_state().statusline_hidden,
            vec!["cwd".to_string()]
        );
        // Show it again ⇒ removed from hidden, set stays clean.
        apply_setting("statusline.cwd", true, &perms);
        assert!(state::load_state().statusline_hidden.is_empty());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn apply_unknown_id_is_a_noop() {
        let tmp = crate::util::unique_temp_dir("ignis-settings-unknown");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        let perms = PermissionState::new(Mode::HandsFree);
        assert_eq!(
            apply_setting("does_not_exist", true, &perms),
            Effect::None,
            "unknown id is a no-op"
        ); // must not panic
        assert!(!perms.sandbox_enabled());
        assert!(!state::load_state().sandbox_enabled);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
