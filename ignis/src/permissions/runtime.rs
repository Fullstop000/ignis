//! Session-scoped permission state: current `Mode` + in-memory session-only
//! "approve session" set. Shared between the agent loop (via
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
    /// Tool names the user picked "Approve session" for. Keyed by tool name
    /// only (no argument patterns) — `bash` once approved means every
    /// subsequent `bash` call passes without prompting for this session.
    /// Per-command allowlist grammar lands in v0.18.0.
    session_allow: HashSet<String>,
}

impl PermissionState {
    pub fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Inner {
                mode,
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
}
