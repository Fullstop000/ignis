//! The generic `/settings` registry — the single place a config knob is
//! declared. [`build_settings`] reads current values; [`apply_setting`] writes
//! them. **Adding a knob is one entry here** (plus its effect); the protocol
//! descriptor ([`crate::console::frontend::protocol::Setting`]) and the Ink
//! panel that renders it don't change. That invariant is the whole point.

use crate::console::frontend::protocol::{Setting, SettingKind};
use crate::permissions::runtime::PermissionState;
use crate::state::{self, State};

/// Footer segments Ink can show/hide, in display order: `(id, label, help)`.
/// Only the segments Ink's footer actually renders — no `git` (Ink has none).
/// Each `id` becomes `statusline.<id>` and matches the shared `statusline_hidden`
/// keys, so native `/settings` and Ink stay consistent.
const STATUSLINE_SEGMENTS: &[(&str, &str, &str)] = &[
    ("model", "Model", "provider / model in the footer"),
    (
        "cwd",
        "Working directory",
        "current directory in the footer",
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
pub fn build_settings(perms: &PermissionState, state: &State) -> Vec<Setting> {
    let mut out = vec![bool_setting(
        "sandbox_enabled".to_string(),
        "Sandbox auto-run bash",
        "Confine unattended (AFK / headless) bash to the project + temp and away from \
         $HOME secrets. Off by default so git push etc. work out of the box.",
        "General",
        perms.sandbox_enabled(),
    )];
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
pub fn apply_setting(id: &str, value: bool, perms: &PermissionState) {
    match id {
        "sandbox_enabled" => {
            perms.set_sandbox_enabled(value);
            let _ = state::persist_sandbox_enabled(value);
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
                return; // already in the desired state — no write
            }
            let _ = state::persist_statusline_hidden(&hidden);
        }
        other => log::warn!("SetSetting: unknown setting id {other:?} (ignored)"),
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
        assert!(!find(&build_settings(&perms, &state), "sandbox_enabled").value);
        perms.set_sandbox_enabled(true);
        assert!(find(&build_settings(&perms, &state), "sandbox_enabled").value);
    }

    #[test]
    fn build_reflects_statusline_hidden() {
        let perms = PermissionState::new(Mode::Off);
        let state = State {
            statusline_hidden: vec!["cwd".to_string()],
            ..State::default()
        };
        let settings = build_settings(&perms, &state);
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
    fn apply_sandbox_flips_live_flag_and_persists() {
        let tmp = crate::util::unique_temp_dir("ignis-settings-sandbox");
        std::fs::create_dir_all(&tmp).unwrap();
        let _home = crate::util::HomeGuard::set(&tmp);

        let perms = PermissionState::new(Mode::HandsFree);
        apply_setting("sandbox_enabled", true, &perms);
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
        apply_setting("does_not_exist", true, &perms); // must not panic
        assert!(!perms.sandbox_enabled());
        assert!(!state::load_state().sandbox_enabled);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
