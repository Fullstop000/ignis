//! Integration test for the filesystem sandbox installed in
//! `extensions::dispatch::run_hook` — Linux Landlock and macOS Seatbelt.
//!
//! Skipped on every other target (the sandbox primitive is OS-specific).
//! On Linux with a kernel that lacks Landlock the sandbox degrades to
//! `NotEnforced`; we detect that and downgrade the write-block assertion
//! to a smoke test (no panic). macOS Seatbelt is available on every
//! supported version (10.5+), so there is no equivalent escape hatch.
//!
//! The two tests assert opposite behaviours under the same extension script:
//!
//! - sandbox ON  → write to a non-allowlisted dir is blocked (file absent)
//! - sandbox OFF → that write succeeds (file present)
//!
//! Together they pin the security contract: the default is the safe one;
//! the opt-out is observable.
//!
//! ## Liveness signal (why the extension emits `updatedInput`)
//!
//! A naive "did the leak file appear?" test has a false-pass hole: if the
//! sandbox profile is so tight the extension never *starts* (e.g. `/bin/sh`
//! can't exec, or the stdout pipe can't be written), the leak file is also
//! absent — for the wrong reason. So the extension emits
//! `{"hookSpecificOutput":{"updatedInput":"HOOK_RAN"}}`, which surfaces as
//! `PromptExtensionResult::Continue("HOOK_RAN")`. Asserting on that proves
//! the extension executed *and* wrote to its stdout pipe under the sandbox —
//! a necessary condition for any extension to function. Only then is "leak
//! file absent" meaningful as "the forbidden write was denied".

#![cfg(all(unix, any(target_os = "linux", target_os = "macos")))]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ignis::extensions::{
    ExtensionContext, ExtensionRegistry, ExtensionSpec, ExtensionsConfig, PromptExtensionResult,
};
use ignis::sandbox::is_kernel_sandbox_available;
use tokio::sync::mpsc;

fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Whether the kernel will actually enforce the sandbox on this host.
/// Delegates to [`ignis::sandbox::is_kernel_sandbox_available`] so the
/// production probe and the test probe cannot drift apart — both run the
/// same Landlock ABI check on Linux and trust the Seatbelt ABI on macOS.
fn sandbox_enforced() -> bool {
    is_kernel_sandbox_available()
}

/// An extension that drains stdin, tries to write `leak_path`, then emits a
/// liveness signal on stdout. The leak write is the security-relevant
/// action; the `updatedInput` proves the extension ran regardless of whether
/// that write succeeded.
fn leak_hook_body(leak_path: &str) -> String {
    format!(
        r#"#!/bin/sh
cat >/dev/null
echo "leaked" > "{leak_path}"
printf '%s' '{{"hookSpecificOutput":{{"updatedInput":"HOOK_RAN"}}}}'
"#
    )
}

/// Build a leak path under cargo's per-test temp dir. That dir lives under
/// the crate's `target/`, which on a normal checkout is NOT under `/tmp`
/// or `/var/tmp` (the only write-allowlisted roots), so a sandboxed
/// extension must be denied the write. (If you check the repo out *under*
/// /tmp this assumption breaks — same caveat as any sandbox allowlist test.)
fn fresh_leak_path(tag: &str) -> PathBuf {
    let leak_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(leak_dir).unwrap();
    let p = leak_dir.join(format!(
        "{tag}-{}.txt",
        std::process::id() as u64 * 1000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
    ));
    let _ = std::fs::remove_file(&p);
    p
}

async fn run_leak_hook(sandbox: bool, leak_path: &Path) -> PromptExtensionResult {
    let home = tempfile::tempdir().unwrap();
    let ignis_dir = home.path().join(".ignis");
    std::fs::create_dir_all(&ignis_dir).unwrap();
    let hook = write_executable(
        &ignis_dir,
        "leak.sh",
        &leak_hook_body(&leak_path.to_string_lossy()),
    );

    let cfg = ExtensionsConfig {
        user_prompt_submit: vec![ExtensionSpec {
            program: hook,
            timeout_ms: 5_000,
            sandbox,
            ..ExtensionSpec::default()
        }],
        ..ExtensionsConfig::default()
    };
    let reg = ExtensionRegistry::from_config(cfg);
    let (tx, _rx) = mpsc::channel(8);
    reg.run_user_prompt_submit(
        "x",
        ExtensionContext {
            session_id: "s",
            cwd: "/tmp",
        },
        &tx,
    )
    .await
}

#[tokio::test]
async fn sandboxed_hook_cannot_write_outside_tmpdir() {
    let leak_path = fresh_leak_path("leak-from-hook");
    let result = run_leak_hook(true, &leak_path).await;

    // Liveness: the extension executed and wrote its JSON to the stdout pipe
    // under the sandbox. If this fails the profile is too tight (the
    // extension never started or couldn't talk back) — a different, louder
    // failure than "the write was denied", and we want to see it distinctly.
    assert_eq!(
        result,
        PromptExtensionResult::Continue("HOOK_RAN".to_string()),
        "extension did not run/communicate under the sandbox — profile likely too tight"
    );

    if sandbox_enforced() {
        assert!(
            !leak_path.exists(),
            "sandbox failed: {} was created by the extension despite the sandbox",
            leak_path.display()
        );
    } else {
        eprintln!(
            "sandbox not enforced on this host (Linux without Landlock); \
             skipping write-block assertion. leak_path.exists() = {}",
            leak_path.exists()
        );
    }
}

#[tokio::test]
async fn unsandboxed_hook_can_write_outside_tmpdir() {
    // With `sandbox: false` the write must always succeed, on either OS.
    // If this fails, env_clear stripped something critical (e.g. PATH so
    // `/bin/sh` couldn't find `cat`) or the spawn failed outright.
    let leak_path = fresh_leak_path("leak-no-sandbox");
    let result = run_leak_hook(false, &leak_path).await;

    assert_eq!(
        result,
        PromptExtensionResult::Continue("HOOK_RAN".to_string()),
        "unsandboxed extension did not run/communicate"
    );
    assert!(
        leak_path.exists(),
        "expected extension to write {} when sandbox is disabled",
        leak_path.display()
    );
}
