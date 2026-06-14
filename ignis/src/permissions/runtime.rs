//! Session-scoped permission state: current `Mode` + in-memory session-only
//! "approve session" set. Shared between the agent loop (via
//! `PermissionChecker`), the slash commands, and the state-persistence layer.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use super::rule::RuleSet;
use super::Mode;

/// Mutable per-session state for the permission system. Wrapped in `Arc<RwLock>`
/// so the agent loop, the console slash commands, and the state-persistence
/// layer can all see/mutate the same instance without ownership headaches.
#[derive(Debug, Default)]
pub struct PermissionState {
    inner: RwLock<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    mode: Mode,
    /// Tool names the user picked "Approve session" for. Keyed by tool name
    /// only (no argument patterns) — `bash` once approved means every
    /// subsequent `bash` call passes without prompting for this session.
    session_allow: HashSet<String>,
    /// The user-rule layer: `config.toml` `[permissions]` rules with persisted
    /// grants folded into `allow`. Consulted by `check()` on every tool call.
    rules: RuleSet,
    /// The persisted grant strings alone (config rules excluded), so an
    /// "Always allow" click can re-write the full grant list to `state.json`.
    grants: Vec<String>,
}

impl PermissionState {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Inner {
                mode,
                ..Inner::default()
            }),
        })
    }

    /// Construct with a config-derived `RuleSet` and the persisted grants from
    /// `state.json`; the grants fold into the rule set's `allow` list.
    pub fn with_rules(mode: Mode, mut rules: RuleSet, grants: Vec<String>) -> Arc<Self> {
        for g in &grants {
            rules.add_grant(g);
        }
        Arc::new(Self {
            inner: RwLock::new(Inner {
                mode,
                rules,
                grants,
                ..Inner::default()
            }),
        })
    }

    /// A clone of the live rule set, for feeding into `check()`.
    pub fn rules_snapshot(&self) -> RuleSet {
        self.read().rules.clone()
    }

    /// The persisted-grant strings, for re-writing `state.json`.
    pub fn grants(&self) -> Vec<String> {
        self.read().grants.clone()
    }

    /// Fold a new "Always allow" grant into the live rules and the grant list.
    pub fn add_grant(&self, grant: &str) {
        let mut inner = self.write();
        inner.rules.add_grant(grant);
        inner.grants.push(grant.to_string());
    }

    pub fn mode(&self) -> Mode {
        self.read().mode
    }

    pub fn set_mode(&self, mode: Mode) {
        self.write().mode = mode;
    }

    pub fn is_session_allowed(&self, tool_name: &str) -> bool {
        self.read().session_allow.contains(tool_name)
    }

    pub fn add_session_allow(&self, tool_name: impl Into<String>) {
        self.write().session_allow.insert(tool_name.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_safe_state() {
        let s = PermissionState::new(Mode::default());
        assert_eq!(s.mode(), Mode::Off);
        assert!(!s.is_session_allowed("bash"));
    }

    #[test]
    fn mode_set_persists() {
        let s = PermissionState::new(Mode::Off);
        s.set_mode(Mode::HandsFree);
        assert_eq!(s.mode(), Mode::HandsFree);
        s.set_mode(Mode::FullyUnattended);
        assert_eq!(s.mode(), Mode::FullyUnattended);
        s.set_mode(Mode::Off);
        assert_eq!(s.mode(), Mode::Off);
    }

    #[test]
    fn session_allow_persists_until_cleared() {
        let s = PermissionState::new(Mode::Off);
        s.add_session_allow("bash");
        assert!(s.is_session_allowed("bash"));
        assert!(!s.is_session_allowed("edit_file"));
    }

    #[test]
    fn new_state_has_no_rules() {
        let s = PermissionState::new(Mode::Off);
        assert!(s.rules_snapshot().is_empty());
        assert!(s.grants().is_empty());
    }

    #[test]
    fn with_rules_folds_grants_into_allow() {
        use crate::permissions::Decision;
        let rules = RuleSet::from_strings(&[], &[], &["bash(rm -rf *)".to_string()]);
        let grants = vec!["bash(git status *)".to_string()];
        let s = PermissionState::with_rules(Mode::Off, rules, grants);
        let snap = s.rules_snapshot();
        // The config deny is present…
        assert!(matches!(
            snap.decide("bash", &serde_json::json!({"command": "rm -rf foo"})),
            Some(Decision::Deny { .. })
        ));
        // …and the grant became an allow.
        assert_eq!(
            snap.decide("bash", &serde_json::json!({"command": "git status -s"})),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn add_grant_updates_live_rules_and_grant_list() {
        use crate::permissions::Decision;
        let s = PermissionState::new(Mode::Off);
        s.add_grant("bash(cargo *)");
        assert_eq!(s.grants(), vec!["bash(cargo *)".to_string()]);
        assert_eq!(
            s.rules_snapshot()
                .decide("bash", &serde_json::json!({"command": "cargo build"})),
            Some(Decision::Allow)
        );
    }
}
