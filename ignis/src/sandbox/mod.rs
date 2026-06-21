//! Generic process-confinement primitive shared by every ignis caller
//! that spawns an untrusted subprocess (hooks today, bash-tool next).
//!
//! This module owns the **mechanism**: a per-platform process-confinement
//! API applied inside a `Command::pre_exec` closure between fork and
//! execve. It is deliberately **policy-free** — the caller passes
//! pre-built `reads` and `writes` slices and chooses what to allow.
//! Hook-specific paths (its own folder, TLS roots, scratch dirs) live in
//! [`crate::hooks::sandbox`]; future bash-tool paths (cwd, system
//! binaries, project root with `$HOME` excluded) will live alongside
//! the bash tool.
//!
//! ## Platforms
//!
//! * **Linux** uses Landlock (ABI V2) via the `landlock` crate. See
//!   [`linux`].
//! * **macOS** uses Apple's `sandbox_init(3)` ("Seatbelt") with a
//!   Scheme-syntax profile built in the parent. See [`macos`]. The
//!   `sandbox_init` ABI is a private Apple API but has been stable
//!   since macOS 10.5 (2007) and is the same primitive Chromium and
//!   Firefox use for renderer confinement.
//! * **Other Unix / Windows** return [`SandboxStatus::PlatformUnsupported`]
//!   and the dispatcher emits a one-time degradation warning.
//!
//! ## Async-signal-safety
//!
//! The two-step `SandboxPolicy::new` → `SandboxPolicy::apply` split
//! exists so the *child-side* `apply` call is allocation-free:
//!
//! * `SandboxPolicy::new` runs in the **parent**, where allocation is
//!   safe. On Linux it clones the path lists; on macOS it serialises
//!   the Seatbelt profile into a `CString`.
//! * `SandboxPolicy::apply` runs inside a `pre_exec` closure in the
//!   forked child, between `fork(2)` and `execve(2)`. In that window
//!   heap allocation is **unsafe** — the allocator's mutex may be held
//!   by a thread that no longer exists in the child. `apply` therefore
//!   only does syscalls plus stack-resident pointer manipulation, and
//!   error paths use `io::Error::from_raw_os_error` (no boxing) instead
//!   of the boxing `io::Error::other`.

use std::path::PathBuf;

/// Outcome of attempting to install the platform sandbox for one call.
/// Threaded into the caller's `tracing` span via
/// `record("sandbox.status", …)` for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Kernel installed and enforced every rule we asked for.
    FullyEnforced,
    /// Kernel accepted the ruleset but downgraded some access modes
    /// (older Landlock ABI). Confinement is real but narrower than
    /// what we requested. macOS's `sandbox_init` has no partial-
    /// enforcement concept and never reports this variant.
    PartiallyEnforced,
    /// Kernel doesn't support the sandbox primitive (e.g. Landlock
    /// `ENOSYS` on older Linux). The process runs **unconfined**.
    NotEnforced,
    /// Build target has no sandbox implementation (currently anything
    /// other than Linux or macOS).
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
        // ABI V2 (Linux 5.19) is the floor because it is the first ABI
        // with `LANDLOCK_ACCESS_FS_REFER` — the right governing `rename(2)`
        // / `link(2)` *across* directories. When a ruleset does NOT handle
        // REFER (as ABI V1 cannot), Landlock denies every cross-directory
        // rename/link with **EXDEV** (a deliberate kernel back-compat
        // behaviour, not a real cross-device move). Build tools we run
        // under this sandbox — cargo/rustc above all — atomically replace
        // artifacts by writing a temp file in one directory and renaming
        // it into another *within `target/`*, so V1 turned every such
        // build into a spurious EXDEV failure. `from_all(V2)` adds REFER
        // to the handled set and `from_write(V2)` grants it on the write
        // roots (see `from_write`), so cross-directory reparenting is
        // allowed *between writable directories* while still denied out to
        // read-only paths. `BestEffort` (the default on `Ruleset::new`)
        // means: on a pre-5.19 kernel, REFER is silently dropped (back to
        // the V1 behaviour — unavoidable, the kernel can't express it) and
        // on no-Landlock kernels we degrade to NotEnforced instead of
        // erroring. We request no V3+ capabilities, so enforcement stays
        // deterministic (FullyEnforced) on any kernel ≥ 5.19.
        let abi = ABI::V2;

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

#[cfg(target_os = "macos")]
mod macos;

/// A parent-built, child-applied sandbox policy.
///
/// Construction (`new`) runs in the parent and is allowed to allocate.
/// Application (`apply`) runs in the forked child between `fork` and
/// `execve` and is allocation-free — it only does syscalls and stack-
/// resident pointer work.
///
/// Internally the representation is per-target:
///
/// * **Linux** stores the parent-owned `Vec<PathBuf>` lists; `apply`
///   borrows slices and walks them with Landlock.
/// * **macOS** stores a single parent-built Seatbelt `CString`; `apply`
///   borrows it as `&CStr` and hands the pointer to `sandbox_init`.
/// * Other targets store a unit `_phantom` and `apply` reports
///   `PlatformUnsupported`.
pub struct SandboxPolicy {
    #[cfg(target_os = "linux")]
    reads: Vec<PathBuf>,
    #[cfg(target_os = "linux")]
    writes: Vec<PathBuf>,
    #[cfg(target_os = "macos")]
    profile: std::ffi::CString,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    _phantom: (),
}

impl SandboxPolicy {
    /// Build a policy from path lists. Runs in the **parent**; allowed
    /// to allocate. On macOS this is where the Seatbelt profile string
    /// is serialised into a `CString`, so by the time we reach the
    /// child only a pointer needs to be handed to `sandbox_init`.
    pub fn new(reads: &[PathBuf], writes: &[PathBuf]) -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                reads: reads.to_vec(),
                writes: writes.to_vec(),
            }
        }
        #[cfg(target_os = "macos")]
        {
            Self {
                profile: macos::build_profile(reads, writes),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (reads, writes);
            Self { _phantom: () }
        }
    }

    /// Apply the policy to the current process. Intended to run in a
    /// `Command::pre_exec` closure in the forked child.
    ///
    /// **Async-signal-safety:** this call performs syscalls (and on
    /// macOS, one call into Apple's `sandbox_init`) and pointer
    /// dereferences only. No heap allocation in the success path; the
    /// error path uses `io::Error::from_raw_os_error` rather than the
    /// boxing `io::Error::other`.
    pub fn apply(&self) -> std::io::Result<SandboxStatus> {
        #[cfg(target_os = "linux")]
        {
            linux::apply_with_paths(&self.reads, &self.writes)
        }
        #[cfg(target_os = "macos")]
        {
            macos::apply_profile(&self.profile)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Ok(SandboxStatus::PlatformUnsupported)
        }
    }
}

/// Allocating convenience wrapper: build a [`SandboxPolicy`] from the
/// given path slices and immediately apply it. Equivalent to
/// `SandboxPolicy::new(reads, writes).apply()`.
///
/// **Not** for use inside a `pre_exec` closure — the construction step
/// allocates. Build the `SandboxPolicy` in the parent and call
/// [`SandboxPolicy::apply`] in the child.
pub fn apply_with_paths(reads: &[PathBuf], writes: &[PathBuf]) -> std::io::Result<SandboxStatus> {
    SandboxPolicy::new(reads, writes).apply()
}

/// Whether the host kernel will actually enforce a sandbox primitive for
/// this process. Used by integration tests to decide whether to assert
/// the strict "write was denied" contract or to downgrade to a smoke
/// test on a host without confinement support.
///
/// * **Linux** — probes Landlock via a raw
///   `landlock_create_ruleset(NULL, 0, VERSION)` syscall. Returns `true`
///   on any kernel that recognises the syscall (5.13+), `false` on
///   `ENOSYS` / `EOPNOTSUPP`.
/// * **macOS** — `sandbox_init` is present on every supported version
///   (10.5+), so this is `true`. We can't probe it without confining
///   this process, so we trust the documented ABI.
/// * **Other targets** — `false` (the dispatcher reports
///   `PlatformUnsupported` and tests should downgrade accordingly).
///
/// `is_kernel_sandbox_available()` does NOT consult `spec.sandbox` — it
/// only answers "would the *kernel* confine this process if asked?". The
/// caller's `sandbox_status` is the combination of this and the
/// per-hook `sandbox: bool` opt-out.
pub fn is_kernel_sandbox_available() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(probe_kernel_sandbox_available)
}

fn probe_kernel_sandbox_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        // ABI >= 1 means the kernel recognises Landlock (5.13+).
        linux_landlock_abi() >= 1
    }
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Highest Landlock ABI the running kernel supports, or `-1` if Landlock is
/// unavailable (`ENOSYS` / `EOPNOTSUPP`). Cached — the probe is a pure
/// query that never mutates userspace.
#[cfg(target_os = "linux")]
fn linux_landlock_abi() -> libc::c_long {
    use std::sync::OnceLock;
    static CACHED: OnceLock<libc::c_long> = OnceLock::new();
    *CACHED.get_or_init(|| {
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
        // SAFETY: NULL + size 0 + flags = VERSION is documented to never
        // mutate userspace; it only reports the supported ABI as the
        // return value (>= 1 on success, -1 with ENOSYS / EOPNOTSUPP if the
        // kernel doesn't know Landlock).
        unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        }
    })
}

/// Whether the host's sandbox permits a cross-directory `rename(2)` /
/// `link(2)` *within* the writable set. This is the capability the bash
/// sandbox relies on for cargo/rustc artifact writes, and the gate the
/// REFER regression test skips on when it is absent.
///
/// * **Linux** — `true` only when Landlock ABI >= 2, the ABI that adds
///   `LANDLOCK_ACCESS_FS_REFER`. On a kernel whose Landlock is only ABI V1
///   (5.13–5.18) the kernel denies every cross-directory reparenting with a
///   synthetic `EXDEV` regardless of our ruleset, so the assertion can't
///   hold there. (A kernel with no Landlock confines nothing, but then the
///   caller is unconfined anyway — also reported as `false` so confinement
///   tests downgrade.)
/// * **macOS** — `true`; the Seatbelt profile has no cross-directory
///   reparenting restriction.
/// * **Other targets** — `false`.
pub fn sandbox_allows_cross_directory_rename() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_landlock_abi() >= 2
    }
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
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
