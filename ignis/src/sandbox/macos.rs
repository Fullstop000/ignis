//! macOS Seatbelt sandbox via Apple's `sandbox_init(3)`.
//!
//! `sandbox_init` is a private but ABI-stable Apple API (since macOS
//! 10.5, 2007) that takes a Scheme-syntax profile string and confines
//! the current process to it. It is the same primitive Chromium and
//! Firefox use for renderer confinement.
//!
//! The translation from our generic `(reads, writes)` slices to
//! Seatbelt's allow/deny grammar is deliberately blunt: we emit a
//! `(deny default)` baseline plus `(allow file-read*  (subpath "..."))`
//! and `(allow file-write* (subpath "..."))` rules for each path, with
//! a small handful of universally-required permissions (process-fork,
//! signal-self, mach-lookup, network*) so a shell-script hook can
//! actually start.
//!
//! ## Async-signal-safety
//!
//! [`build_profile`] runs in the parent and allocates freely. The
//! child-side [`apply_profile`] takes a `&CStr` borrowed from a
//! parent-built `CString` and only calls `sandbox_init`, so it does
//! not allocate.
//!
//! ## Limitations (vs the Linux Landlock path)
//!
//! * **No "partial enforcement" concept.** `sandbox_init` either
//!   installs the profile or fails. We map success → `FullyEnforced`,
//!   failure → `Err(EPERM)`.
//! * **The `/tmp` ⇄ `/private/tmp` (and `/var` ⇄ `/private/var`)
//!   symlink rewrite.** On macOS, `/tmp` is a symlink to
//!   `/private/tmp` (same for `/var`). User-facing tools open the
//!   pre-resolution path but Seatbelt matches the post-resolution
//!   path. We emit both forms so a hook that writes `/tmp/foo` is
//!   matched against the underlying `/private/tmp/foo`. This is
//!   inferred from documented Seatbelt behaviour and **unverified**
//!   on macOS hardware from this Linux host — see PR notes.

use std::ffi::{CStr, CString};
use std::io;
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::ptr;

use super::SandboxStatus;

extern "C" {
    /// Apple's `sandbox_init(profile, flags, errorbuf)`. Returns 0 on
    /// success, -1 on failure with an optional error string in
    /// `*errorbuf` (must be freed via `sandbox_free_error`).
    ///
    /// `flags` semantics per Apple's (private) header: `0` means
    /// "interpret `profile` as a literal Scheme string". The named
    /// preset constants live in `<sandbox.h>` and aren't used here.
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    /// Release the optional error buffer returned by a failed
    /// `sandbox_init` call.
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Build a Seatbelt Scheme policy that allows the listed read/write
/// paths and the small set of universal permissions a shell-script
/// hook needs to start. Runs in the parent.
///
/// The generated profile starts with `(deny default)` so anything we
/// don't explicitly allow is forbidden. The "universal essentials"
/// list (`process-fork`, `process-exec*`, `signal (target self)`,
/// `file-read-metadata`, `network*`, `mach-lookup`, `ipc-posix-shm`,
/// `sysctl-read`, plus `file-read*` on the dyld shared-cache locations
/// `/System`, `/private/var/db/dyld`, `/private/preboot/Cryptexes`) is
/// the smallest set that lets a `python3` or `/bin/sh` hook execute at
/// all on macOS — the dyld-cache reads in particular are mandatory or
/// the dynamic linker can't start *any* binary. It intentionally
/// matches the rough shape of the Linux defaults (filesystem
/// confinement, no network filtering).
pub(super) fn build_profile(reads: &[PathBuf], writes: &[PathBuf]) -> CString {
    let mut s = String::with_capacity(2048);
    s.push_str("(version 1)\n");
    s.push_str("(deny default)\n");
    // Essentials so an interpreter / shell can actually start.
    s.push_str("(allow process-fork)\n");
    s.push_str("(allow process-exec*)\n");
    s.push_str("(allow signal (target self))\n");
    s.push_str("(allow file-read-metadata)\n");
    // Network is intentionally open: v2's threat model is filesystem
    // confinement, and the env-var allowlist already prevents the hook
    // from learning a credential to exfil.
    s.push_str("(allow network*)\n");
    // Mach lookups + SysV / POSIX shared memory so common runtimes
    // (Python, Ruby, JVM) can talk to system services.
    s.push_str("(allow mach-lookup)\n");
    s.push_str("(allow ipc-posix-shm)\n");
    // Dynamic-linker essentials. Every macOS executable is dynamically
    // linked against libSystem; before `main`, dyld must read (mmap) the
    // dyld shared cache. On modern macOS that cache lives under /System
    // (incl. the Apple-Silicon /System/Cryptexes firmlink, whose backing
    // store is /private/preboot/Cryptexes) and the legacy
    // /private/var/db/dyld — NOT under /usr/lib. Without these
    // `file-read*` grants, `(deny default)` blocks the mmap and NO binary
    // can exec, so every hook soft-fails before it starts. `sysctl-read`
    // covers libSystem's init-time hw.*/kern.* probes. All read-only
    // system content (no user data), so granting them doesn't weaken the
    // write-confinement / credential-exfil threat model.
    s.push_str("(allow sysctl-read)\n");
    s.push_str("(allow file-read* (subpath \"/System\"))\n");
    s.push_str("(allow file-read* (subpath \"/private/var/db/dyld\"))\n");
    s.push_str("(allow file-read* (subpath \"/private/preboot/Cryptexes\"))\n");

    for p in reads {
        emit_allow(&mut s, "file-read*", p);
    }
    for p in writes {
        emit_allow(&mut s, "file-write*", p);
    }

    // `CString::new` only fails on interior NULs. A NUL inside a path
    // shouldn't be possible on macOS but if it ever happens we prefer
    // to confine harshly (deny everything) than to panic in a code
    // path that's already mid-fork.
    CString::new(s).unwrap_or_else(|_| {
        CString::new("(version 1)\n(deny default)\n")
            .expect("static deny-all profile contains no NUL")
    })
}

/// Emit an `(allow OP (subpath "PATH"))` line plus, for paths under
/// the well-known macOS symlinked roots, a matching line for the
/// post-resolution `/private/...` form.
fn emit_allow(s: &mut String, op: &str, p: &PathBuf) {
    let Some(path_str) = p.to_str() else {
        // Non-UTF-8 path: skip rather than emit a line we can't escape
        // correctly. Hooks on macOS with non-UTF-8 paths in the
        // allowlist are vanishingly rare.
        return;
    };
    push_subpath(s, op, path_str);

    // macOS symlinks: /tmp → /private/tmp, /var → /private/var. The
    // canonical (post-resolution) form is what Seatbelt actually
    // matches against open(2) traffic. Tools open the un-prefixed
    // path; allow both so neither side surprises us.
    //
    // Ordering matters: check the more specific `/var/tmp` prefix
    // before the bare `/var/` prefix, otherwise the `/var/...` rewrite
    // would also fire for `/var/tmp/...` and we'd emit a duplicate.
    if let Some(rest) = path_str.strip_prefix("/tmp") {
        let priv_path = format!("/private/tmp{rest}");
        push_subpath(s, op, &priv_path);
    } else if let Some(rest) = path_str.strip_prefix("/var/tmp") {
        let priv_path = format!("/private/var/tmp{rest}");
        push_subpath(s, op, &priv_path);
    } else if let Some(rest) = path_str.strip_prefix("/var/") {
        let priv_path = format!("/private/var/{rest}");
        push_subpath(s, op, &priv_path);
    }
}

fn push_subpath(s: &mut String, op: &str, path: &str) {
    s.push_str("(allow ");
    s.push_str(op);
    s.push_str(" (subpath \"");
    scheme_escape_into(s, path);
    s.push_str("\"))\n");
}

/// Escape a string for inclusion inside a Scheme double-quoted
/// literal. Only `\\` and `"` are special.
fn scheme_escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
fn scheme_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    scheme_escape_into(&mut out, s);
    out
}

/// Apply the Seatbelt profile via `sandbox_init`. **Child-side**,
/// async-signal-safe — only the `extern "C"` call plus stack pointer
/// manipulation.
pub(super) fn apply_profile(profile: &CStr) -> io::Result<SandboxStatus> {
    let mut errbuf: *mut c_char = ptr::null_mut();
    // SAFETY: `sandbox_init` is a stable (if private) Apple API. We
    // pass:
    //   * a NUL-terminated profile string owned by the parent and
    //     borrowed here as `*const c_char`;
    //   * `flags = 0` ("interpret `profile` as a literal string");
    //   * a pointer to receive an optional error buffer that we free
    //     immediately if present.
    // The function returns -1 on failure.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut errbuf) };
    if rc != 0 {
        if !errbuf.is_null() {
            // We deliberately don't read the error string — staying
            // alloc-free in the failure path is more important than
            // the diagnostic. `errno` after a failed `sandbox_init`
            // carries the kernel error code.
            //
            // SAFETY: `errbuf` was just populated by `sandbox_init`
            // and is non-null per the if-guard; `sandbox_free_error`
            // is the documented release function.
            unsafe { sandbox_free_error(errbuf) };
        }
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    // `sandbox_init` has no "partial enforcement" concept the way
    // Landlock does — either the profile installed or it didn't.
    Ok(SandboxStatus::FullyEnforced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn profile_str(reads: &[&str], writes: &[&str]) -> String {
        let reads: Vec<PathBuf> = reads.iter().map(PathBuf::from).collect();
        let writes: Vec<PathBuf> = writes.iter().map(PathBuf::from).collect();
        let cstr = build_profile(&reads, &writes);
        cstr.to_string_lossy().into_owned()
    }

    #[test]
    fn profile_starts_with_version_and_deny_default() {
        let p = profile_str(&[], &[]);
        assert!(
            p.starts_with("(version 1)\n(deny default)\n"),
            "profile prelude wrong: {p}"
        );
    }

    #[test]
    fn profile_includes_universal_essentials() {
        // These are the lines a shell-script hook needs to actually
        // launch. If someone deletes one in a refactor we want to see
        // it in CI (even if we can't dogfood the runtime effect on a
        // Linux box).
        let p = profile_str(&[], &[]);
        for required in [
            "(allow process-fork)",
            "(allow process-exec*)",
            "(allow signal (target self))",
            "(allow file-read-metadata)",
            "(allow network*)",
            "(allow mach-lookup)",
            "(allow ipc-posix-shm)",
            // dyld shared-cache + libSystem-init essentials: without
            // these no dynamically-linked binary can exec under the
            // (deny default) baseline.
            "(allow sysctl-read)",
            "(allow file-read* (subpath \"/System\"))",
            "(allow file-read* (subpath \"/private/var/db/dyld\"))",
            "(allow file-read* (subpath \"/private/preboot/Cryptexes\"))",
        ] {
            assert!(p.contains(required), "missing essential: {required}\n{p}");
        }
    }

    #[test]
    fn read_path_emits_subpath_rule() {
        let p = profile_str(&["/usr/lib"], &[]);
        assert!(
            p.contains("(allow file-read* (subpath \"/usr/lib\"))"),
            "read rule missing: {p}"
        );
    }

    #[test]
    fn write_path_emits_subpath_rule() {
        let p = profile_str(&[], &["/dev/null"]);
        assert!(
            p.contains("(allow file-write* (subpath \"/dev/null\"))"),
            "write rule missing: {p}"
        );
    }

    #[test]
    fn tmp_path_also_emits_private_form() {
        // `/tmp` is a symlink to `/private/tmp` on macOS. Seatbelt
        // matches the post-resolution path, so we emit both forms.
        let p = profile_str(&["/tmp"], &["/tmp"]);
        assert!(
            p.contains("(allow file-read* (subpath \"/tmp\"))"),
            "missing /tmp read: {p}"
        );
        assert!(
            p.contains("(allow file-read* (subpath \"/private/tmp\"))"),
            "missing /private/tmp read mirror: {p}"
        );
        assert!(
            p.contains("(allow file-write* (subpath \"/tmp\"))"),
            "missing /tmp write: {p}"
        );
        assert!(
            p.contains("(allow file-write* (subpath \"/private/tmp\"))"),
            "missing /private/tmp write mirror: {p}"
        );
    }

    #[test]
    fn var_tmp_path_emits_private_var_tmp_mirror() {
        let p = profile_str(&[], &["/var/tmp"]);
        assert!(
            p.contains("(allow file-write* (subpath \"/var/tmp\"))"),
            "missing /var/tmp: {p}"
        );
        assert!(
            p.contains("(allow file-write* (subpath \"/private/var/tmp\"))"),
            "missing /private/var/tmp mirror: {p}"
        );
    }

    #[test]
    fn var_path_emits_private_var_mirror_but_not_for_var_tmp() {
        // `/var/run/foo` should emit `/private/var/run/foo`. But a
        // path under `/var/tmp` must NOT emit BOTH `/private/var/tmp/x`
        // AND `/private/var/tmp/x` — the strip_prefix ordering picks
        // the more specific rewrite, so each input emits exactly one
        // mirror.
        let p = profile_str(&[], &["/var/run/myapp"]);
        assert!(
            p.contains("(allow file-write* (subpath \"/private/var/run/myapp\"))"),
            "missing /private/var/run/myapp mirror: {p}"
        );
        let var_tmp = profile_str(&[], &["/var/tmp/foo"]);
        // Should only have the /private/var/tmp/foo mirror, not also
        // a stray /private/var/tmp/foo from the broader /var/ branch.
        let count = var_tmp.matches("/private/var/tmp/foo").count();
        assert_eq!(
            count, 1,
            "expected exactly one /private/... mirror: {var_tmp}"
        );
    }

    #[test]
    fn scheme_escape_handles_quotes_and_backslashes() {
        assert_eq!(scheme_escape("ab\"cd"), "ab\\\"cd");
        assert_eq!(scheme_escape("ab\\cd"), "ab\\\\cd");
        assert_eq!(scheme_escape("plain"), "plain");
    }

    #[test]
    fn profile_is_nul_terminated_cstring() {
        // build_profile returns CString — proves nul-termination by
        // round-tripping back through CString::new.
        let cs = build_profile(&[PathBuf::from("/tmp")], &[]);
        // No interior NULs means we can round-trip.
        let bytes = cs.as_bytes();
        assert!(!bytes.contains(&0), "interior NUL in profile");
        // And the conversion to &CStr is sound for apply_profile().
        let _cstr: &CStr = cs.as_c_str();
    }

    #[test]
    fn path_with_interior_nul_falls_back_to_deny_all() {
        // Synthesise a "path" with an interior NUL. PathBuf accepts
        // OsStrings that contain NUL on Unix, so this is the right
        // place to test the CString fallback.
        use std::os::unix::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_vec(b"/has\0nul".to_vec());
        let path = PathBuf::from(bad);
        let cs = build_profile(&[path], &[]);
        let s = cs.to_string_lossy();
        assert_eq!(
            s, "(version 1)\n(deny default)\n",
            "interior NUL did not trigger deny-all fallback: {s}"
        );
    }

    // Round-trip a CString through CStr to prove apply_profile()'s
    // input type is reachable from build_profile()'s output. We do
    // NOT call apply_profile itself — it would actually confine the
    // cargo test binary.
    #[test]
    fn cstring_can_be_borrowed_as_cstr() {
        let cs = CString::new("(version 1)\n(deny default)\n").unwrap();
        let cstr: &CStr = cs.as_c_str();
        assert_eq!(cstr.to_bytes(), b"(version 1)\n(deny default)\n");
    }
}
