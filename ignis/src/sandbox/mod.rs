//! Generic process-confinement primitive shared by every ignis caller
//! that spawns an untrusted subprocess (hooks today, bash-tool next).
//!
//! This module owns the **mechanism**: a Linux Landlock ruleset applied
//! inside a `Command::pre_exec` closure between fork and execve. It is
//! deliberately **policy-free** — the caller passes pre-built `reads`
//! and `writes` slices and chooses what to allow. Hook-specific paths
//! (its own folder, TLS roots, scratch dirs) live in
//! [`crate::hooks::sandbox`]; future bash-tool paths (cwd, system
//! binaries, project root with `$HOME` excluded) will live alongside
//! the bash tool.
//!
//! ## Async-signal-safety
//!
//! [`apply_with_paths`] is intended to run inside a `pre_exec` closure
//! in the forked child, between `fork(2)` and `execve(2)`. It does
//! **not** allocate from the child's heap — path lists arrive as
//! borrowed slices (parent-built) and error paths use
//! `io::Error::from_raw_os_error` instead of the boxing
//! `io::Error::other`. The landlock crate's syscall surface
//! (`Ruleset::default` → `create` → `add_rule` → `restrict_self`)
//! itself only does direct syscalls with stack-only `BitFlags`.
//!
//! ## Non-Linux
//!
//! Stub returning [`SandboxStatus::PlatformUnsupported`]. Callers
//! proceed unconfined; the dispatcher emits a one-time degradation
//! warning so the user knows.

use std::path::PathBuf;

/// Outcome of attempting to install the Landlock ruleset for one call.
/// Threaded into the caller's `tracing` span via
/// `record("sandbox.status", …)` for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Kernel installed and enforced every rule we asked for.
    FullyEnforced,
    /// Kernel accepted the ruleset but downgraded some access modes
    /// (older Landlock ABI). Confinement is real but narrower than
    /// what we requested.
    PartiallyEnforced,
    /// Kernel doesn't support Landlock (or `landlock_create_ruleset`
    /// returned `ENOSYS`). The process runs **unconfined** on Linux.
    NotEnforced,
    /// Non-Linux build — sandboxing is not implemented.
    PlatformUnsupported,
    /// Caller explicitly opted out of sandboxing. Distinct from
    /// `PlatformUnsupported` so telemetry can separate "user choice"
    /// from "platform gap".
    Disabled,
}

impl SandboxStatus {
    /// Short label used in the `tracing` span attribute. Stable strings
    /// so dashboards can pivot on them.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxStatus::FullyEnforced => "full",
            SandboxStatus::PartiallyEnforced => "partial",
            SandboxStatus::NotEnforced => "not_enforced",
            SandboxStatus::PlatformUnsupported => "platform_unsupported",
            SandboxStatus::Disabled => "disabled",
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::path::PathBuf;

    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };

    use super::SandboxStatus;

    /// Build and apply a Landlock ruleset to the current (forked) process.
    ///
    /// **Async-signal-safety:** read/write path slices are pre-built by
    /// the *parent* and passed in by reference, so this function does not
    /// allocate from the child's heap. Error mapping uses
    /// `io::Error::from_raw_os_error` (no allocation) rather than the
    /// boxing `io::Error::other`.
    pub fn apply_with_paths(reads: &[PathBuf], writes: &[PathBuf]) -> io::Result<SandboxStatus> {
        // ABI V1 is the introductory Landlock ABI (Linux 5.13). All the
        // access modes we need (read_file, read_dir, write_file, etc.)
        // are present in V1, so pinning to V1 makes our confinement
        // deterministic across kernel versions — newer ABIs add
        // capabilities we don't request anyway. `BestEffort` (the
        // default on `Ruleset::new`) means: on older / unsupported
        // kernels, degrade to NotEnforced instead of erroring.
        let abi = ABI::V1;

        let ruleset_built = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
        let mut created = ruleset_built
            .create()
            .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;

        for p in reads {
            // Best-effort: a missing path (e.g. `/lib64` on Debian
            // pure-multiarch, or `/dev/urandom` in a stripped chroot) is
            // not a sandbox failure. Skip silently.
            if let Ok(fd) = PathFd::new(p) {
                created = created
                    .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
                    .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
            }
        }
        for p in writes {
            if let Ok(fd) = PathFd::new(p) {
                created = created
                    .add_rule(PathBeneath::new(fd, AccessFs::from_write(abi)))
                    .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
            }
        }

        let restricted = created
            .restrict_self()
            .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
        Ok(match restricted.ruleset {
            RulesetStatus::FullyEnforced => SandboxStatus::FullyEnforced,
            RulesetStatus::PartiallyEnforced => SandboxStatus::PartiallyEnforced,
            RulesetStatus::NotEnforced => SandboxStatus::NotEnforced,
        })
    }
}

/// Apply a Landlock ruleset with the given pre-built path slices. The
/// parent constructs the `reads` / `writes` `Vec<PathBuf>` and `move`s
/// them into the `pre_exec` closure; this function takes references
/// only and performs syscalls — it does not allocate from the child's
/// heap.
///
/// On non-Linux this is a no-op that returns
/// [`SandboxStatus::PlatformUnsupported`].
///
/// This is the *async-signal-safe* entry point — use it from a
/// `Command::pre_exec` closure on Unix.
pub fn apply_with_paths(reads: &[PathBuf], writes: &[PathBuf]) -> std::io::Result<SandboxStatus> {
    #[cfg(target_os = "linux")]
    {
        linux::apply_with_paths(reads, writes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (reads, writes);
        Ok(SandboxStatus::PlatformUnsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_status_label_strings_are_stable() {
        // Dashboards pivot on these — changing them silently is a
        // behaviour break for any operator watching telemetry.
        assert_eq!(SandboxStatus::FullyEnforced.as_str(), "full");
        assert_eq!(SandboxStatus::PartiallyEnforced.as_str(), "partial");
        assert_eq!(SandboxStatus::NotEnforced.as_str(), "not_enforced");
        assert_eq!(
            SandboxStatus::PlatformUnsupported.as_str(),
            "platform_unsupported"
        );
        assert_eq!(SandboxStatus::Disabled.as_str(), "disabled");
    }
}
