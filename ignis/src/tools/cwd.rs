//! The session's working directory, shared by every path-resolving tool.
//!
//! File/exec tools resolve relative paths against this handle rather than the
//! process cwd, so the worktree tools ([`super::worktree`]) can redirect the
//! whole toolset at once by swapping the path inside it — no per-tool plumbing,
//! and no racy `chdir` of a multi-threaded process.
//!
//! Cloning shares the same underlying cell (it's an `Arc`). Constructing from a
//! path makes a *fresh, independent* cell — which is exactly what unit tests and
//! sub-agents want (they never switch), while the registration path threads one
//! shared cell into every tool plus `enter_worktree`/`exit_worktree`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct SessionCwd(Arc<RwLock<PathBuf>>);

impl SessionCwd {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(Arc::new(RwLock::new(path.into())))
    }

    /// The current working directory (a snapshot — the lock is not held).
    pub fn get(&self) -> PathBuf {
        self.0.read().unwrap().clone()
    }

    /// Redirect every tool sharing this handle to a new directory.
    pub fn set(&self, path: impl Into<PathBuf>) {
        *self.0.write().unwrap() = path.into();
    }
}

impl From<&Path> for SessionCwd {
    fn from(p: &Path) -> Self {
        Self::new(p.to_path_buf())
    }
}

impl From<&PathBuf> for SessionCwd {
    fn from(p: &PathBuf) -> Self {
        Self::new(p.clone())
    }
}

impl From<PathBuf> for SessionCwd {
    fn from(p: PathBuf) -> Self {
        Self::new(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_cell_so_a_switch_is_seen_everywhere() {
        let a = SessionCwd::new(PathBuf::from("/start"));
        let b = a.clone();
        a.set(PathBuf::from("/moved"));
        // The clone observes the switch — this is what lets `enter_worktree`
        // redirect tools it never touched directly.
        assert_eq!(b.get(), PathBuf::from("/moved"));
    }

    #[test]
    fn from_a_path_makes_an_independent_cell() {
        let base = PathBuf::from("/start");
        let a = SessionCwd::from(base.as_path());
        let b = SessionCwd::from(base.as_path());
        a.set(PathBuf::from("/moved"));
        // Two `From<&Path>` cells are independent — a test tool that switches
        // its own cwd can't disturb another's.
        assert_eq!(b.get(), base);
    }
}
