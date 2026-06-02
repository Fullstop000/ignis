//! Hook-specific sandbox policy.
//!
//! The mechanism (Landlock ruleset construction, async-signal-safe
//! syscalls, kernel ABI handling) lives in [`crate::sandbox`]; this
//! module owns the **policy** — what a hook subprocess is allowed to
//! read and write by default. Other subprocess callers (e.g. the bash
//! tool, future v3) ship their own policy modules and reuse the
//! same primitive.
//!
//! ## What's allowed by default
//!
//! Reads: the hook's own folder (so `import` / `require` of sibling
//! files works), the system library paths (`/usr/lib`, `/lib`,
//! `/lib64`), the standard binary paths (`/bin`, `/usr/bin`, `/sbin`,
//! `/usr/sbin`) so a `#!/bin/sh` hook can actually exec `sh`, TLS
//! roots (`/etc/ssl/certs`), DNS config (`/etc/resolv.conf`),
//! `/dev/urandom` + `/dev/zero` (for RNG and shell-script zero-fills),
//! and the kernel-managed scratch directories `/tmp` + `/var/tmp`.
//! Writes: `/tmp`, `/var/tmp`, `/dev/null`.
//!
//! `$TMPDIR` is **not** trusted from the environment: a user launching
//! ignis with `TMPDIR=$HOME` would otherwise expose the entire home
//! directory to every sandboxed hook.

use std::path::{Path, PathBuf};

// Re-export the generic status type so existing `hooks::sandbox::SandboxStatus`
// imports continue to work after the split.
pub use crate::sandbox::SandboxStatus;

/// The hardcoded scratch directories the hook sandbox allows
/// reads/writes on. See module doc for why `$TMPDIR` is not trusted.
const TMPDIRS: &[&str] = &["/tmp", "/var/tmp"];

fn tmpdirs() -> impl Iterator<Item = PathBuf> {
    TMPDIRS.iter().map(PathBuf::from)
}

/// Default-allowed read paths for a hook subprocess.
///
/// Tested separately so the documented list is checked even on
/// non-Linux builds where [`apply`] is a stub. Pass `Some(folder)` to
/// allow the hook to read its own directory (typical); pass `None` for
/// a bare program (e.g. `python3 hook.py` resolved by PATH) — the
/// universal defaults still apply but the hook will fail to find its
/// script unless it sits in `/tmp` or `/var/tmp`.
pub fn default_read_paths(hook_folder: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/etc/ssl/certs"),
        PathBuf::from("/usr/lib"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/dev/urandom"),
        PathBuf::from("/dev/zero"),
    ];
    v.extend(tmpdirs());
    // Only add the hook's folder when we know it. Bare programs like
    // `python3 hook.py` (no parent) used to fall back to `/`, which
    // silently disabled read confinement — those hooks now confine to
    // the universal paths above and will fail to find their script
    // unless it sits in /tmp or /var/tmp. The right user-error to
    // surface.
    if let Some(folder) = hook_folder {
        v.push(folder.to_path_buf());
    }
    v
}

/// Default-allowed write paths for a hook subprocess.
///
/// Includes `/dev/null` because almost every shell pipeline somewhere
/// writes to it (`cat >/dev/null`, `2>/dev/null`) — denying it breaks
/// the lowest common denominator hook (a shell script) for negligible
/// security gain. `/dev/null` is a write-only sink with no observable
/// state.
pub fn default_write_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = tmpdirs().collect();
    v.push(PathBuf::from("/dev/null"));
    v
}

/// Allocating convenience wrapper: build the hook default read/write
/// paths and apply them via [`crate::sandbox::apply_with_paths`]. **Not**
/// for use inside `pre_exec` — call the generic primitive directly from
/// there with parent-built slices.
pub fn apply(hook_folder: Option<&Path>) -> std::io::Result<SandboxStatus> {
    let reads = default_read_paths(hook_folder);
    let writes = default_write_paths();
    crate::sandbox::apply_with_paths(&reads, &writes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_read_list_includes_documented_paths() {
        // Pins the spec's documented default ruleset — if someone
        // removes `/etc/ssl/certs` the translator hook silently
        // breaks. Better to notice in CI.
        let hook = PathBuf::from("/home/me/.ignis/hooks/translate-en");
        let paths = default_read_paths(Some(&hook));
        assert!(paths.contains(&hook));
        for required in [
            "/etc/ssl/certs",
            "/usr/lib",
            "/lib",
            "/lib64",
            "/bin",
            "/usr/bin",
            "/sbin",
            "/usr/sbin",
            "/etc/resolv.conf",
            "/dev/urandom",
            "/dev/zero",
            "/tmp",
            "/var/tmp",
        ] {
            assert!(
                paths.iter().any(|p| p == Path::new(required)),
                "missing default read path: {required}"
            );
        }
    }

    #[test]
    fn default_read_list_omits_hook_folder_when_none() {
        // Bare programs (`python3 hook.py`) have no parent → no hook
        // folder. Pre-fix this fell back to `/`, silently disabling read
        // confinement.
        let paths = default_read_paths(None);
        for p in &paths {
            assert_ne!(p, Path::new("/"), "must not allow reads beneath /");
        }
        // Universal paths still present.
        assert!(paths.iter().any(|p| p == Path::new("/usr/lib")));
        assert!(paths.iter().any(|p| p == Path::new("/tmp")));
    }

    #[test]
    fn default_write_list_includes_tmpdirs_and_dev_null() {
        let writes = default_write_paths();
        assert!(writes.iter().any(|p| p == Path::new("/tmp")));
        assert!(writes.iter().any(|p| p == Path::new("/var/tmp")));
        assert!(writes.iter().any(|p| p == Path::new("/dev/null")));
        // Keep the list short — adding new write paths is a sandbox
        // loosening and should be deliberate. Pin the count so a stray
        // push trips CI.
        assert_eq!(writes.len(), 3);
    }
}
