//! Session-scoped permission state: current mode + AFK toggle + in-memory
//! session-only "approve session" set. Shared between the agent loop (via
//! `PermissionChecker`), the slash commands, and the state-persistence layer.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

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
    afk: bool,
    /// Tool names the user picked "Approve session" for. v0.16.0 keys by
    /// tool name only (no argument patterns) — `bash` once approved means
    /// every subsequent `bash` call passes without prompting for this session.
    /// v0.17.0 adds arg-pattern keys.
    session_allow: HashSet<String>,
}

impl PermissionState {
    pub fn new(mode: Mode, afk: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Inner {
                mode,
                afk,
                session_allow: HashSet::new(),
            }),
        })
    }

    pub fn mode(&self) -> Mode {
        self.inner.read().expect("permissions lock poisoned").mode
    }

    pub fn set_mode(&self, mode: Mode) {
        self.inner.write().expect("permissions lock poisoned").mode = mode;
    }

    pub fn afk(&self) -> bool {
        self.inner.read().expect("permissions lock poisoned").afk
    }

    pub fn set_afk(&self, afk: bool) {
        self.inner.write().expect("permissions lock poisoned").afk = afk;
    }

    pub fn toggle_afk(&self) -> bool {
        let mut inner = self.inner.write().expect("permissions lock poisoned");
        inner.afk = !inner.afk;
        inner.afk
    }

    pub fn is_session_allowed(&self, tool_name: &str) -> bool {
        self.inner
            .read()
            .expect("permissions lock poisoned")
            .session_allow
            .contains(tool_name)
    }

    pub fn add_session_allow(&self, tool_name: impl Into<String>) {
        self.inner
            .write()
            .expect("permissions lock poisoned")
            .session_allow
            .insert(tool_name.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_safe_state() {
        let s = PermissionState::new(Mode::default(), false);
        assert_eq!(s.mode(), Mode::Default);
        assert!(!s.afk());
        assert!(!s.is_session_allowed("bash"));
    }

    #[test]
    fn mode_toggle_persists() {
        let s = PermissionState::new(Mode::Default, false);
        s.set_mode(Mode::BypassPermissions);
        assert_eq!(s.mode(), Mode::BypassPermissions);
    }

    #[test]
    fn afk_toggle_returns_new_state() {
        let s = PermissionState::new(Mode::Default, false);
        assert!(s.toggle_afk());
        assert!(s.afk());
        assert!(!s.toggle_afk());
        assert!(!s.afk());
    }

    #[test]
    fn session_allow_persists_until_cleared() {
        let s = PermissionState::new(Mode::Default, false);
        s.add_session_allow("bash");
        assert!(s.is_session_allowed("bash"));
        assert!(!s.is_session_allowed("edit_file"));
    }
}
