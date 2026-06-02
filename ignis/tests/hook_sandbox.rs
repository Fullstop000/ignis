//! Integration test for the Linux Landlock sandbox installed in
//! `hooks::dispatch::run_hook`.
//!
//! Skipped on non-Linux: Landlock is a Linux LSM. On Linux but a kernel
//! without Landlock support, the sandbox degrades to NotEnforced and the
//! "leak" hook can still write — we only verify the call doesn't panic in
//! that case.
//!
//! The two tests assert opposite behaviours under the same hook script:
//!
//! - sandbox ON → write to $HOME blocked (file does not appear)
//! - sandbox OFF → write to $HOME succeeds (file appears)
//!
//! Together they pin the security contract: the default is the safe one;
//! the opt-out is observable.

#![cfg(all(unix, target_os = "linux"))]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ignis::hooks::{HookContext, HookRegistry, HookSpec, HooksConfig};
use tokio::sync::mpsc;

fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Tries to probe whether this kernel actually enforces Landlock. We don't
/// fail the test when it doesn't — Landlock is a kernel feature, not an
/// ignis bug. The integration test then becomes a smoke test: we verify
/// the dispatcher doesn't crash, and skip the "did the write actually fail"
/// assertion.
fn landlock_available() -> bool {
    // Raw syscall: `landlock_create_ruleset(NULL, 0, 1)` returns the supported
    // ABI version on success, -1 (ENOSYS) on kernels without Landlock.
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    ret >= 1
}

#[tokio::test]
async fn sandboxed_hook_cannot_write_outside_tmpdir() {
    // Layout:
    //   /tmp/<home>/.ignis/leak.sh    — the hook script (in $TMPDIR is fine)
    //   <leak_dir>/leak-from-hook.txt — the file the hook tries to create.
    //                                   `leak_dir` is INTENTIONALLY built
    //                                   under cargo's target/ so it's NOT
    //                                   under /tmp (which IS in the write
    //                                   allowlist). A real $HOME wouldn't
    //                                   be under /tmp; the test mirrors that.
    let home = tempfile::tempdir().unwrap();
    let ignis_dir = home.path().join(".ignis");
    std::fs::create_dir_all(&ignis_dir).unwrap();
    let leak_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(leak_dir).unwrap();
    let leak_path = leak_dir.join(format!(
        "leak-from-hook-{}.txt",
        std::process::id() as u64 * 1000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
    ));
    // Pre-cleanup in case a previous run crashed mid-test.
    let _ = std::fs::remove_file(&leak_path);
    let leak_str = leak_path.to_string_lossy();

    let hook = write_executable(
        &ignis_dir,
        "leak.sh",
        &format!(
            r#"#!/bin/sh
cat >/dev/null
# Sandboxed: this write must FAIL because $HOME is not in the write allowlist.
# Sandbox off: this write succeeds, the test asserts the file exists.
echo "leaked" > "{leak_str}"
printf '{{}}'
"#
        ),
    );

    let cfg = HooksConfig {
        user_prompt_submit: vec![HookSpec {
            program: hook,
            args: vec![],
            timeout_ms: 5_000,
            env: vec![],
            sandbox: true,
        }],
        assistant_message_render: vec![],
    };
    let reg = HookRegistry::from_config(cfg);
    let (tx, _rx) = mpsc::channel(8);
    let _ = reg
        .run_user_prompt_submit(
            "x",
            HookContext {
                session_id: "s",
                cwd: "/tmp",
            },
            &tx,
        )
        .await;

    if landlock_available() {
        // The kernel enforces Landlock — the hook's write to $HOME MUST have
        // been denied. The script's `echo > file` failed silently; the file
        // never got created.
        assert!(
            !leak_path.exists(),
            "sandbox failed: {} was created by the hook despite Landlock",
            leak_path.display()
        );
    } else {
        // Landlock not in this kernel: just verify the dispatcher didn't
        // panic. The write may or may not have succeeded.
        eprintln!(
            "Landlock not available on this kernel; skipping write-block assertion. \
             leak_path.exists() = {}",
            leak_path.exists()
        );
    }
}

#[tokio::test]
async fn unsandboxed_hook_can_write_outside_tmpdir() {
    // Same fixture but `sandbox: false` — the hook's write to a non-/tmp
    // path should succeed. Pins the opt-out as observable.
    let home = tempfile::tempdir().unwrap();
    let ignis_dir = home.path().join(".ignis");
    std::fs::create_dir_all(&ignis_dir).unwrap();
    let leak_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(leak_dir).unwrap();
    let leak_path = leak_dir.join(format!(
        "leak-no-sandbox-{}.txt",
        std::process::id() as u64 * 1000
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
    ));
    let _ = std::fs::remove_file(&leak_path);
    let leak_str = leak_path.to_string_lossy();

    let hook = write_executable(
        &ignis_dir,
        "leak.sh",
        &format!(
            r#"#!/bin/sh
cat >/dev/null
echo "leaked" > "{leak_str}"
printf '{{}}'
"#
        ),
    );

    let cfg = HooksConfig {
        user_prompt_submit: vec![HookSpec {
            program: hook,
            args: vec![],
            timeout_ms: 5_000,
            env: vec![],
            sandbox: false,
        }],
        assistant_message_render: vec![],
    };
    let reg = HookRegistry::from_config(cfg);
    let (tx, _rx) = mpsc::channel(8);
    let _ = reg
        .run_user_prompt_submit(
            "x",
            HookContext {
                session_id: "s",
                cwd: "/tmp",
            },
            &tx,
        )
        .await;

    // With sandbox off the write must always succeed, Landlock present or
    // not. If this assertion fails, env_clear stripped something critical
    // (e.g. PATH so /bin/sh couldn't find `cat`) or the spawn failed.
    assert!(
        leak_path.exists(),
        "expected hook to write {} when sandbox is disabled",
        leak_path.display()
    );
}
