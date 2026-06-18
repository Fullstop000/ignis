//! End-to-end regression for the bash write-sandbox (unattended modes).
//!
//! Goes through the real `BashTool` → `tokio::process::Command` → child path.
//! Asserts the externally-observable contract: a sandboxed command may write
//! inside cwd but not outside it, while reads stay broad; an unsandboxed
//! command is unconfined.
//!
//! Skipped (smoke-only) on kernels without Landlock — the confinement can't be
//! asserted there. Linux/macOS only via the `#![cfg(unix)]` gate.

#![cfg(unix)]

use ignis::tools::{BashSandbox, BashTool};
use ignis::AgentTool;
use serde_json::json;

/// A fresh dir under cargo's target tmp (NOT under `/tmp`), so a path outside
/// cwd is neither under the cwd grant nor the temp grant — the sandbox must
/// deny writes to it.
fn fresh_base(tag: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn sandbox_confines_writes_to_cwd_when_enforced() {
    if !ignis::sandbox::is_kernel_sandbox_available() {
        eprintln!("skipping: no kernel sandbox (Landlock) on this host");
        return;
    }
    let base = fresh_base("bash-sbx");
    let cwd = base.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let outside = base.join("outside.txt");
    let readable = base.join("readable.txt");
    std::fs::write(&readable, b"secret-but-readable").unwrap();

    let tool = BashTool::new(&cwd).with_sandbox(Some(BashSandbox {
        extra_writes: vec![],
    }));

    // Write inside cwd → allowed.
    let r = tool
        .call(json!({ "command": "echo hi > inside.txt && cat inside.txt" }))
        .await;
    assert!(
        !r.is_error,
        "write inside cwd should succeed: {}",
        r.content
    );
    assert!(cwd.join("inside.txt").exists());

    // Write outside cwd (and outside /tmp) → denied by the sandbox.
    let r = tool
        .call(json!({ "command": format!("echo nope > '{}'", outside.display()) }))
        .await;
    assert!(r.is_error, "write outside cwd must be denied");
    assert!(!outside.exists(), "the outside file must not be created");

    // Read outside cwd → allowed (reads are broad).
    let r = tool
        .call(json!({ "command": format!("cat '{}'", readable.display()) }))
        .await;
    assert!(
        !r.is_error,
        "read outside cwd should succeed: {}",
        r.content
    );
    assert!(r.content.contains("secret-but-readable"));

    std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn extra_write_path_becomes_writable() {
    if !ignis::sandbox::is_kernel_sandbox_available() {
        return;
    }
    let base = fresh_base("bash-sbx-extra");
    let cwd = base.join("proj");
    let extra = base.join("allowed-extra");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&extra).unwrap();

    let tool = BashTool::new(&cwd).with_sandbox(Some(BashSandbox {
        extra_writes: vec![extra.clone()],
    }));
    let r = tool
        .call(json!({ "command": format!("echo ok > '{}/f.txt'", extra.display()) }))
        .await;
    assert!(
        !r.is_error,
        "configured extra path should be writable: {}",
        r.content
    );
    assert!(extra.join("f.txt").exists());
    std::fs::remove_dir_all(&base).ok();
}

#[tokio::test]
async fn no_sandbox_does_not_confine_writes() {
    // Off mode: BashTool with no sandbox — a write outside cwd is NOT blocked
    // by Landlock (the permission gate is the only guard there).
    let base = fresh_base("bash-nosbx");
    let cwd = base.join("proj");
    std::fs::create_dir_all(&cwd).unwrap();
    let outside = base.join("ok.txt");
    let tool = BashTool::new(&cwd); // no .with_sandbox → unsandboxed
    let r = tool
        .call(json!({ "command": format!("echo ok > '{}'", outside.display()) }))
        .await;
    assert!(!r.is_error, "no sandbox: write outside cwd allowed");
    assert!(outside.exists());
    std::fs::remove_dir_all(&base).ok();
}
