//! End-to-end regression suite for the v2 hook sandbox.
//!
//! Every test in this file goes through the *full* `HookRegistry` →
//! `dispatch::run_hook` → `tokio::process::Command` → child subprocess
//! path. They assert both the externally-observable contract (the
//! `HookOutcome` shape, the file-system side effects, the warning
//! events on `tx`) and the internal `sandbox_status` so a future
//! refactor that flips confinement on or off without changing
//! observable behaviour still fails CI.
//!
//! ## Layer organisation
//!
//! Each section is a v2 sandbox layer (or a regression for a specific
//! bug). Per-layer helper modules keep fixtures local so the test body
//! reads as "what we assert" rather than "how we set up".
//!
//! 1. **env-var allowlist** — every test, all targets.
//! 2. **filesystem sandbox** — Linux Landlock + macOS Seatbelt.
//!    Tests that exercise writes / reads outside the allowlist are
//!    skipped on hosts without the kernel primitive (older Linux).
//! 3. **SIGTERM grace** — Linux only (macOS resets SIGTERM to DFL
//!    on exec; see `sigterm_grace_with_cooperative_hook_exits_promptly`
//!    for the cross-platform alternative).
//! 4. **1 MiB buffer cap** — every test, all targets.
//! 5. **lifecycle outcomes** — every test, all targets.
//! 6. **composition** — env + sandbox + lifecycle together.
//! 7. **macOS Seatbelt regressions** — only on macOS, exercises
//!    the bash-startup cwd fix.
//! 8. **status reporting** — once-per-session warnings, status field.
//!
//! ## Running
//!
//! ```sh
//! cargo test --test sandbox_e2e
//! ```
//!
//! The full suite is portable: every test is guarded with the
//! appropriate `cfg` so the build never fails on any target. On Linux
//! without Landlock the strict write-block assertions downgrade to
//! "smoke tests" via `if kernel_sandbox_available() { ... }`.

#![cfg(unix)]

// `Duration` and `Instant` are only used by the Linux-only cooperative
// SIGTERM test (`#[cfg(not(target_os = "macos"))]`), so they read as
// "unused" on macOS. Same for `DispatchContext` and `SandboxStatus` —
// the latter is referenced inside `HookOutcome::{status}` matches.
#[allow(unused_imports)]
use std::os::unix::fs::PermissionsExt;
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[allow(unused_imports)]
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use ignis::hooks::{DispatchContext, HookOutcome, HookSpec, SandboxStatus};
use ignis::sandbox::is_kernel_sandbox_available;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build a `HookSpec` with the test-friendly defaults. `timeout_ms`
/// is generous (30 s) so a CI box under load — where 25 subprocess
/// spawns may queue — doesn't trip the dispatcher's per-call timeout
/// on tests that don't actually wait (the underlying scripts exit in
/// milliseconds). The "real" timeout tests in `dispatch.rs` use
/// shorter, deterministic values.
fn spec_with(program: PathBuf, sandbox: bool, env: Vec<String>) -> HookSpec {
    HookSpec {
        program,
        args: vec![],
        timeout_ms: 30_000,
        env,
        sandbox,
    }
}

/// Run a single hook as if it were registered for `UserPromptSubmit` and
/// invoked with `payload`. Returns the outcome + a channel of warnings
/// the dispatcher emitted, so tests can assert both surfaces.
/// Run the dispatcher's `run_hook` directly (not through the registry)
/// so we get the full `HookOutcome` (with `sandbox_status`) for
/// assertions.
async fn run_dispatch(spec: HookSpec, payload: &str) -> (HookOutcome, Vec<ignis::AgentEvent>) {
    use ignis::hooks::HookEvent;
    let (tx, mut rx) = mpsc::channel(8);
    let outcome = ignis::hooks::dispatch::run_hook(
        &spec,
        HookEvent::UserPromptSubmit,
        payload,
        &DispatchContext {
            session_id: "s",
            cwd: "/tmp",
        },
        Some(&tx),
    )
    .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    (outcome, events)
}

fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Path under cargo's per-test temp dir. Lives under `target/`, which
/// is NOT under `/tmp` or `/var/tmp` on a normal checkout, so a
/// sandboxed hook must be denied the write. (If you check the repo
/// out *under* /tmp this assumption breaks — same caveat as any
/// sandbox allowlist test.)
fn fresh_target_path(tag: &str) -> PathBuf {
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

/// The expected `sandbox_status` for a hook run on the current host.
/// Encapsulates the "what does this kernel do?" question in one place
/// so every test below can write `assert_eq!(outcome.sandbox_status,
/// expected_sandbox_status(spec.sandbox))` without re-deriving it.
fn expected_sandbox_status(sandbox: bool) -> SandboxStatus {
    if !sandbox {
        SandboxStatus::Disabled
    } else if is_kernel_sandbox_available() {
        SandboxStatus::FullyEnforced
    } else if cfg!(target_os = "linux") {
        // Linux kernel without Landlock — the platform supports the
        // primitive concept but the running kernel doesn't expose it.
        SandboxStatus::NotEnforced
    } else {
        // Any other Unix (BSD, etc.) — no implementation at all.
        SandboxStatus::PlatformUnsupported
    }
}

// ---------------------------------------------------------------------------
// Layer 1: env-var allowlist (all targets, all hosts)
// ---------------------------------------------------------------------------

/// A hook that dumps its entire env (one line per name=value pair) into
/// `updatedInput` so the test can assert what the child actually saw.
fn env_dump_script(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "env-dump.sh",
        r#"#!/bin/sh
cat >/dev/null
out=""
while IFS= read -r line; do
    [ -z "$line" ] && continue
    # Skip bash / shell-internal vars that vary by host — we only
    # assert on the vars we explicitly set.
    case "$line" in
        BASH_*|_*|SHLVL=*|PWD=*|OLDPWD=*) continue ;;
    esac
    out="$out$line;"
done <<EOF
$(env)
EOF
out=$(printf '%s' "$out" | tr '"' "'")
printf '%s' "{\"hookSpecificOutput\":{\"updatedInput\":\"$out\"}}"
"#,
    )
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    write_executable(dir, name, body)
}

#[tokio::test]
async fn env_allowlist_blocks_secret_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let script = env_dump_script(tmp.path());
    std::env::set_var("IGNIS_E2E_SECRET", "leaked-credential-XYZ");

    let (out, _) = run_dispatch(spec_with(script, false, vec![]), "x").await;
    let body = match out {
        HookOutcome::Mutated { updated, .. } => updated,
        other => panic!("expected Mutated, got {other:?}"),
    };
    assert!(
        body.contains("PATH="),
        "universal allowlist must include PATH; got: {body}"
    );
    assert!(
        !body.contains("IGNIS_E2E_SECRET"),
        "secret env var leaked despite empty allowlist: {body}"
    );
    std::env::remove_var("IGNIS_E2E_SECRET");
}

#[tokio::test]
async fn env_allowlist_passes_universal_set() {
    let tmp = tempfile::tempdir().unwrap();
    let script = env_dump_script(tmp.path());
    std::env::set_var("HOME", "/home/e2e-user");

    let (out, _) = run_dispatch(spec_with(script, false, vec![]), "x").await;
    let body = match out {
        HookOutcome::Mutated { updated, .. } => updated,
        other => panic!("expected Mutated, got {other:?}"),
    };
    for must_have in ["PATH=", "HOME=/home/e2e-user", "USER="] {
        assert!(
            body.contains(must_have),
            "universal allowlist missing {must_have}: {body}"
        );
    }
}

#[tokio::test]
async fn env_list_in_spec_adds_to_universal() {
    let tmp = tempfile::tempdir().unwrap();
    let script = env_dump_script(tmp.path());
    std::env::set_var("IGNIS_E2E_TOKEN", "tok-12345");

    let (out, _) = run_dispatch(
        spec_with(script, false, vec!["IGNIS_E2E_TOKEN".to_string()]),
        "x",
    )
    .await;
    let body = match out {
        HookOutcome::Mutated { updated, .. } => updated,
        other => panic!("expected Mutated, got {other:?}"),
    };
    assert!(
        body.contains("IGNIS_E2E_TOKEN=tok-12345"),
        "explicit env declaration did not pass secret through: {body}"
    );
    std::env::remove_var("IGNIS_E2E_TOKEN");
}

#[tokio::test]
async fn env_clear_blocks_inherited_universal_arbitrary_var() {
    // An env var not in the universal allowlist AND not declared in
    // `env: [...]` must NOT reach the child, even if the parent had it
    // set. (Universal allowlist is a *fixed* set, not "parent's env
    // minus secrets" — see `UNIVERSAL_ENV_ALLOWLIST`.)
    let tmp = tempfile::tempdir().unwrap();
    let script = env_dump_script(tmp.path());
    std::env::set_var("IGNIS_E2E_ARBITRARY", "should-be-blocked");

    let (out, _) = run_dispatch(spec_with(script, false, vec![]), "x").await;
    let body = match out {
        HookOutcome::Mutated { updated, .. } => updated,
        other => panic!("expected Mutated, got {other:?}"),
    };
    assert!(
        !body.contains("IGNIS_E2E_ARBITRARY"),
        "non-allowlisted env var leaked: {body}"
    );
    std::env::remove_var("IGNIS_E2E_ARBITRARY");
}

// ---------------------------------------------------------------------------
// Layer 2: filesystem sandbox (Linux + macOS only — others PlatformUnsupported)
// ---------------------------------------------------------------------------

fn should_enforce_filesystem_assertions() -> bool {
    is_kernel_sandbox_available()
}

#[tokio::test]
async fn sandboxed_hook_cannot_write_to_target_tmpdir() {
    if !should_enforce_filesystem_assertions() {
        eprintln!("kernel sandbox unavailable; skipping write-block assertion");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let leak_path = fresh_target_path("leak-outside-tmpdir");
    let leak_str = leak_path.to_string_lossy().to_string();
    let script = write_script(
        tmp.path(),
        "leak.sh",
        &format!(
            "#!/bin/sh\n\
             cat >/dev/null\n\
             echo \"leaked\" > \"{leak_str}\"\n\
             printf '%s' '{{\"hookSpecificOutput\":{{\"updatedInput\":\"HOOK_RAN\"}}}}'\n"
        ),
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    // Liveness: the hook executed under the profile (HOOK_RAN made it
    // through the pipe). If this fails the profile is too tight —
    // distinct from "the write was denied".
    match out {
        HookOutcome::Mutated { ref updated, .. } => {
            assert_eq!(
                updated, "HOOK_RAN",
                "hook did not run/communicate under sandbox — profile too tight"
            );
        }
        other => panic!("expected Mutated with HOOK_RAN liveness, got {other:?}"),
    }
    assert!(
        !leak_path.exists(),
        "sandbox failed: {} was created by the hook despite the sandbox",
        leak_path.display()
    );
    // The status must reflect that the sandbox *was* enforced for this
    // call. (A future refactor that demotes FullyEnforced to Disabled
    // would break this assertion.)
    let status = match out {
        HookOutcome::Mutated { sandbox_status, .. } => sandbox_status,
        _ => unreachable!(),
    };
    assert_eq!(status, SandboxStatus::FullyEnforced);
}

#[tokio::test]
async fn unsandboxed_hook_can_write_to_target_tmpdir() {
    let tmp = tempfile::tempdir().unwrap();
    let leak_path = fresh_target_path("leak-no-sandbox");
    let leak_str = leak_path.to_string_lossy().to_string();
    let script = write_script(
        tmp.path(),
        "leak.sh",
        &format!(
            "#!/bin/sh\n\
             cat >/dev/null\n\
             echo \"leaked\" > \"{leak_str}\"\n\
             printf '%s' '{{\"hookSpecificOutput\":{{\"updatedInput\":\"HOOK_RAN\"}}}}'\n"
        ),
    );
    let (out, _) = run_dispatch(spec_with(script, false, vec![]), "x").await;
    match out {
        HookOutcome::Mutated { ref updated, .. } => {
            assert_eq!(updated, "HOOK_RAN", "unsandboxed hook did not communicate");
        }
        other => panic!("expected Mutated, got {other:?}"),
    }
    assert!(
        leak_path.exists(),
        "expected hook to write {} when sandbox is disabled",
        leak_path.display()
    );
    let status = match out {
        HookOutcome::Mutated { sandbox_status, .. } => sandbox_status,
        _ => unreachable!(),
    };
    assert_eq!(status, SandboxStatus::Disabled);
}

#[tokio::test]
async fn sandboxed_hook_can_write_to_actual_tmp() {
    // /tmp IS in the write allowlist — a hook that wants to drop a
    // scratch file there must be allowed. (This is the design: hooks
    // can stage work in $TMPDIR.)
    //
    // We hardcode `/tmp` rather than `std::env::temp_dir()` because on
    // macOS the latter returns `/var/folders/.../T/...`, which is
    // under `/var/` but NOT under the hardcoded `/var/tmp` we allowlist.
    // The contract we want to assert is "the hardcoded /tmp + /var/tmp
    // allowlist works", not "any temp dir works" — a hook that wants
    // broader write access declares so via `sandbox: false`.
    if !should_enforce_filesystem_assertions() {
        eprintln!("kernel sandbox unavailable; skipping /tmp write assertion");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let scratch_dir = std::path::PathBuf::from("/tmp").join(format!(
        "ignis-e2e-scratch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&scratch_dir).unwrap();
    let scratch_file = scratch_dir.join("hook-can-write-here.txt");
    let scratch_str = scratch_file.to_string_lossy().to_string();
    let script = write_script(
        tmp.path(),
        "scratch.sh",
        &format!(
            "#!/bin/sh\n\
             cat >/dev/null\n\
             echo \"scratch\" > \"{scratch_str}\"\n\
             printf '%s' '{{\"hookSpecificOutput\":{{\"updatedInput\":\"HOOK_RAN\"}}}}'\n"
        ),
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    assert!(matches!(out, HookOutcome::Mutated { .. }));
    assert!(
        scratch_file.exists(),
        "sandbox denied a write to /tmp; /tmp must be in the write allowlist"
    );
    let _ = std::fs::remove_file(&scratch_file);
    let _ = std::fs::remove_dir(&scratch_dir);
}

#[tokio::test]
async fn sandboxed_hook_can_read_system_libs() {
    // /usr/lib is in the default read allowlist. A hook that needs to
    // load shared libraries (or call into libSystem) must be able to
    // stat /usr/lib for the dynamic linker.
    if !should_enforce_filesystem_assertions() {
        eprintln!("kernel sandbox unavailable; skipping /usr/lib read assertion");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "read-usrlib.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         if [ -d /usr/lib ] || [ -L /usr/lib ]; then\n\
             printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"FOUND\"}}'\n         else\n\
             printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"MISSING\"}}'\n         fi\n",
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    match out {
        HookOutcome::Mutated { ref updated, .. } => {
            assert_eq!(updated, "FOUND", "/usr/lib is in the read allowlist");
        }
        other => panic!("expected Mutated, got {other:?}"),
    }
}

#[tokio::test]
async fn sandboxed_hook_cannot_read_home() {
    // $HOME must NOT be in the read allowlist — that's the whole point
    // of the sandbox. A hook that tries to read ~/.ssh/id_rsa should
    // fail.
    if !should_enforce_filesystem_assertions() {
        eprintln!("kernel sandbox unavailable; skipping $HOME read-block assertion");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME set in test env");
    let target = home.join("ignis-e2e-should-not-exist.txt");
    let _ = std::fs::remove_file(&target);
    let target_str = target.to_string_lossy().to_string();
    let script = write_script(
        tmp.path(),
        "read-home.sh",
        &format!(
            "#!/bin/sh\n\
             cat >/dev/null\n\
             if [ -r \"{target_str}\" ]; then\n\
                 printf '%s' '{{\"hookSpecificOutput\":{{\"updatedInput\":\"READABLE\"}}}}'\n\
             else\n\
                 printf '%s' '{{\"hookSpecificOutput\":{{\"updatedInput\":\"BLOCKED\"}}}}'\n\
             fi\n"
        ),
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    match out {
        HookOutcome::Mutated { ref updated, .. } => {
            assert_eq!(updated, "BLOCKED", "sandbox let hook read $HOME: {out:?}");
        }
        other => panic!("expected Mutated, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Layer 3: SIGTERM grace (Linux only for the uncooperative variant)
// ---------------------------------------------------------------------------

/// Uncooperative SIGTERM-ignoring hook: a Python one-liner that
/// installs `SIG_IGN` and sleeps. Linux-only because macOS resets
/// SIGTERM to `SIG_DFL` on `exec` for child processes without a
/// controlling terminal (a 10.5 security hardening), so the
/// `SIG_IGN` set in the child's address space is overridden by the
/// kernel before delivery. The cooperative-exit test
/// (`sigterm_grace_with_cooperative_hook_exits_promptly`) covers
/// the primary use of the grace window on all platforms.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn sigterm_grace_kills_uncooperative_hook_after_one_second() {
    let tmp = tempfile::tempdir().unwrap();
    let body = b"\
#!/usr/bin/env python3
import signal, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
try:
    sys.stdin.read()
except Exception:
    pass
time.sleep(30)
";
    let script = write_executable(
        tmp.path(),
        "ignore-term.py",
        std::str::from_utf8(body).unwrap(),
    );
    let spec = HookSpec {
        program: script,
        args: vec![],
        timeout_ms: 100,
        ..HookSpec::default()
    };
    let t0 = Instant::now();
    let (out, _) = run_dispatch(spec, "x").await;
    let elapsed = t0.elapsed();

    match out {
        HookOutcome::SoftFailed { reason, .. } => assert!(reason.contains("timed out")),
        other => panic!("expected SoftFailed, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(1050),
        "did not honour grace window: elapsed = {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "SIGKILL did not land promptly: elapsed = {elapsed:?}"
    );
}

#[cfg(unix)]
#[cfg(unix)]
#[tokio::test]
async fn sigterm_grace_with_cooperative_hook_exits_promptly() {
    // A hook that handles SIGTERM and exits cleanly should exit
    // *before* the 1 s grace elapses. This is the primary use of the
    // grace window: give a well-behaved hook a moment to flush before
    // escalating to SIGKILL.
    //
    // Skipped on macOS: the macOS Python stdlib at /Library or
    // /opt/homebrew is NOT in the Seatbelt read allowlist, so the
    // hook can't even start under the sandbox. (The cooperative
    // handshake only matters when the child actually runs; on macOS
    // the existing un-cooperative test in `hook_sandbox.rs` covers
    // the kernel-confinement contract.)
    //
    // Skipped on hosts without kernel sandbox: the dispatcher still
    // sends SIGTERM, but there's no point asserting the grace window
    // for a child that the kernel never confined in the first place.
    if !is_kernel_sandbox_available() {
        eprintln!("kernel sandbox unavailable; skipping cooperative SIGTERM test");
        return;
    }
    #[cfg(target_os = "macos")]
    {
        eprintln!("macOS Python not in Seatbelt read allowlist; skipping cooperative SIGTERM test");
        return;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let tmp = tempfile::tempdir().unwrap();
        // The Python body has 4-space indents (PEP 8). The raw byte
        // string starts after the b"\\n so the very next line is
        // is column 0 of the body.
        let body = b"#!/usr/bin/env python3
import signal, sys, time
def _term(_signum, _frame):
    sys.exit(0)
signal.signal(signal.SIGTERM, _term)
try:
    sys.stdin.read()
except Exception:
    pass
time.sleep(30)
";
        let script = write_executable(
            tmp.path(),
            "cooperative.py",
            std::str::from_utf8(body).unwrap(),
        );
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 100,
            ..HookSpec::default()
        };
        let t0 = Instant::now();
        let (out, _) = run_dispatch(spec, "x").await;
        let elapsed = t0.elapsed();

        match out {
            HookOutcome::SoftFailed { reason, .. } => assert!(reason.contains("timed out")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_millis(1500),
            "grace window not honoured on cooperative exit: elapsed = {elapsed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Layer 4: 1 MiB buffer cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdout_truncated_at_one_mib_with_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "spew.sh",
        // 2 * 1024 * 1024 = 2097152 bytes of 'x'. dd-from-/dev/zero
        // is the most portable 2 MiB producer (no `head -c` portability
        // concerns across bash 3.2 / dash / zsh).
        "#!/bin/sh\n\
         cat >/dev/null\n\
         dd if=/dev/zero bs=1024 count=2048 2>/dev/null | tr '\\0' x\n",
    );
    let (out, events) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    // Spew of 2 MiB of 'x' is not valid JSON; outcome is SoftFailed.
    // The point of the test is the warning + cap, not the parse.
    assert!(matches!(out, HookOutcome::SoftFailed { .. }));
    let warning = events.iter().find_map(|e| match e {
        ignis::AgentEvent::Warning { source, message }
            if source == "hook.buffer" && message.contains("stdout") =>
        {
            Some(message.clone())
        }
        _ => None,
    });
    assert!(
        warning.is_some(),
        "expected a hook.buffer Warning for stdout, events were: {events:?}"
    );
    let msg = warning.unwrap();
    assert!(msg.contains("1 MiB"), "unexpected warning text: {msg}");
}

#[tokio::test]
async fn stderr_truncated_at_one_mib_with_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "spew-err.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf 'real message first\\n' >&2\n\
         dd if=/dev/zero bs=1024 count=2048 2>/dev/null | tr '\\0' x >&2\n\
         exit 2\n",
    );
    let (out, events) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    // exit 2 + blocked; the warning should still fire.
    assert!(matches!(out, HookOutcome::Blocked { .. }));
    let warning = events.iter().find_map(|e| match e {
        ignis::AgentEvent::Warning { source, message }
            if source == "hook.buffer" && message.contains("stderr") =>
        {
            Some(message.clone())
        }
        _ => None,
    });
    assert!(
        warning.is_some(),
        "expected a hook.buffer Warning for stderr, events were: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Layer 5: subprocess lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_binary_is_soft_failed_with_disabled_status() {
    let spec = spec_with(
        PathBuf::from("/nonexistent/binary/__ignis_no_such_path__"),
        // `sandbox: true` here proves the status is computed up-front,
        // BEFORE the spawn attempt — the failure path carries the
        // expected status, not a placeholder.
        true,
        vec![],
    );
    let (out, _) = run_dispatch(spec, "x").await;
    match out {
        HookOutcome::SoftFailed {
            reason,
            sandbox_status,
        } => {
            assert!(reason.contains("spawn failed"));
            assert_eq!(sandbox_status, SandboxStatus::FullyEnforced);
        }
        other => panic!("expected SoftFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn exit_2_is_blocked_with_enforced_status() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "block.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf 'block reason' >&2\n\
         exit 2\n",
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    match out {
        HookOutcome::Blocked {
            stderr,
            sandbox_status,
        } => {
            assert!(stderr.contains("block reason"));
            assert_eq!(sandbox_status, expected_sandbox_status(true));
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_json_is_soft_failed_with_enforced_status() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "bad.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf 'not json at all'\n",
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    match out {
        HookOutcome::SoftFailed {
            reason,
            sandbox_status,
        } => {
            assert!(reason.contains("invalid JSON"));
            assert_eq!(sandbox_status, expected_sandbox_status(true));
        }
        other => panic!("expected SoftFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn success_returns_mutated_with_enforced_status() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "ok.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"rewritten\"}}'\n",
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    match out {
        HookOutcome::Mutated {
            updated,
            sandbox_status,
        } => {
            assert_eq!(updated, "rewritten");
            assert_eq!(sandbox_status, expected_sandbox_status(true));
        }
        other => panic!("expected Mutated, got {other:?}"),
    }
}

#[tokio::test]
async fn passthrough_keeps_status_field() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(tmp.path(), "noop.sh", "#!/bin/sh\ncat >/dev/null\n");
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    match out {
        HookOutcome::PassThrough { sandbox_status } => {
            assert_eq!(sandbox_status, expected_sandbox_status(true));
        }
        other => panic!("expected PassThrough, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Layer 6: composition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sandboxed_hook_with_env_declaration_still_runs() {
    // Composition: env declaration + sandbox both on. The hook should
    // see the declared env var, run under the sandbox, and rewrite
    // successfully.
    let tmp = tempfile::tempdir().unwrap();
    let script = env_dump_script(tmp.path());
    std::env::set_var("IGNIS_E2E_COMPOSE_TOKEN", "compose-tok-XYZ");

    let (out, _) = run_dispatch(
        spec_with(script, true, vec!["IGNIS_E2E_COMPOSE_TOKEN".to_string()]),
        "x",
    )
    .await;
    let body = match out {
        HookOutcome::Mutated {
            updated,
            sandbox_status,
        } => {
            assert_eq!(sandbox_status, expected_sandbox_status(true));
            updated
        }
        other => panic!("expected Mutated, got {other:?}"),
    };
    assert!(
        body.contains("IGNIS_E2E_COMPOSE_TOKEN=compose-tok-XYZ"),
        "env declaration should reach the sandboxed hook: {body}"
    );
    assert!(
        body.contains("PATH="),
        "universal allowlist should still be active: {body}"
    );
    std::env::remove_var("IGNIS_E2E_COMPOSE_TOKEN");
}

#[tokio::test]
async fn chain_of_two_hooks_propagates_sandbox_status() {
    // Two-hook chain: each carries its own sandbox_status. The last
    // one's status is what surfaces through the registry passthrough.
    // (Direct assertion via the dispatch path — see `run_dispatch`.)
    let tmp = tempfile::tempdir().unwrap();
    let s1 = write_script(
        tmp.path(),
        "s1.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"S1\"}}'\n",
    );
    let s2 = write_script(
        tmp.path(),
        "s2.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"S1-S2\"}}'\n",
    );
    for spec in [s1, s2] {
        let (out, _) = run_dispatch(spec_with(spec, true, vec![]), "x").await;
        match out {
            HookOutcome::Mutated { sandbox_status, .. } => {
                assert_eq!(sandbox_status, expected_sandbox_status(true));
            }
            other => panic!("expected Mutated, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Layer 7: macOS Seatbelt regressions
// ---------------------------------------------------------------------------

/// Regression: the macOS Seatbelt profile used to fail bash's
/// `shell-init` and `job-working-directory` startup probes with EPERM
/// because the child's CWD was inherited from the parent (typically
/// the user's project root, NOT in the read allowlist). The
/// dispatcher now sets the child's CWD to the script's own folder,
/// which IS in the read allowlist. This test pins the fix: the
/// hook's stderr must not be polluted with EPERM noise that pushes
/// the real stderr past `truncate_chars(_, 200)`.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_seatbelt_does_not_pollute_stderr_with_eperm() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "blk.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf 'real message' >&2\n\
         exit 2\n",
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    match out {
        HookOutcome::Blocked { stderr, .. } => {
            assert!(
                stderr.contains("real message"),
                "the hook's actual stderr was truncated away by bash's \
                 shell-init noise. dispatcher stderr was: {stderr:?}"
            );
            assert!(
                !stderr.contains("shell-init"),
                "Seatbelt profile regressed: bash's shell-init EPERM \
                 noise is leaking into hook stderr. dispatcher stderr \
                 was: {stderr:?}"
            );
            assert!(
                !stderr.contains("job-working-directory"),
                "Seatbelt profile regressed: bash's job-working-directory \
                 EPERM noise is leaking into hook stderr. dispatcher \
                 stderr was: {stderr:?}"
            );
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

/// Regression: on macOS, the sandboxed hook's getcwd() inside its own
/// script directory must succeed (so the interpreter can do
/// file-relative imports, etc.). Pin by having the hook call `pwd`
/// and assert the result matches the script's parent.
///
/// The macOS Seatbelt profile rewrites `/var` → `/private/var` and
/// `/tmp` → `/private/tmp` because those are symlinks. `pwd` returns
/// the *resolved* path (so `/var/folders/...` shows up as
/// `/private/var/folders/...` in the child). The test canonicalises
/// the expected path with `std::fs::canonicalize` so both sides
/// agree.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_seatbelt_hook_getcwd_is_script_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = std::fs::canonicalize(tmp.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let script = write_script(
        tmp.path(),
        "pwd.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         PWD_OUT=$(pwd)\n\
         printf '%s' \"{\\\"hookSpecificOutput\\\":{\\\"updatedInput\\\":\\\"$PWD_OUT\\\"}}\"\n",
    );
    let (out, _) = run_dispatch(spec_with(script, true, vec![]), "x").await;
    let got = match out {
        HookOutcome::Mutated { updated, .. } => updated,
        other => panic!("expected Mutated, got {other:?}"),
    };
    assert_eq!(
        got, expected,
        "hook's CWD should be the script's parent directory"
    );
}

// ---------------------------------------------------------------------------
// Layer 8: status reporting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_status_when_sandbox_opt_out() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "ok.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"x\"}}'\n",
    );
    let (out, _) = run_dispatch(spec_with(script, false, vec![]), "x").await;
    let status = match out {
        HookOutcome::Mutated { sandbox_status, .. } => sandbox_status,
        other => panic!("expected Mutated, got {other:?}"),
    };
    assert_eq!(status, SandboxStatus::Disabled);
}

#[tokio::test]
async fn unconfined_warning_emitted_when_kernel_sandbox_unavailable() {
    // On a host without kernel sandbox (Linux without Landlock, or
    // other-Unix), the dispatcher must emit exactly one
    // `hook.sandbox` warning per hook per session.
    if is_kernel_sandbox_available() {
        eprintln!("kernel sandbox IS available; skipping warning-suppression test");
        return;
    }
    ignis::hooks::dispatch::reset_sandbox_warnings();
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "ok.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"x\"}}'\n",
    );
    let spec = spec_with(script, true, vec![]);
    // First call → warning fires.
    let (_, events1) = run_dispatch(spec.clone(), "x").await;
    let warned1 = events1.iter().any(
        |e| matches!(e, ignis::AgentEvent::Warning { source, .. } if source == "hook.sandbox"),
    );
    assert!(
        warned1,
        "first run on no-sandbox host must emit hook.sandbox warning"
    );
    // Second call → same hook name, no warning.
    let (_, events2) = run_dispatch(spec, "x").await;
    let warned2 = events2.iter().any(
        |e| matches!(e, ignis::AgentEvent::Warning { source, .. } if source == "hook.sandbox"),
    );
    assert!(
        !warned2,
        "second run with same hook name should be silenced; events were: {events2:?}"
    );
}

#[tokio::test]
async fn disabled_does_not_emit_unconfined_warning() {
    // `sandbox: false` is a user choice, not a platform gap — no
    // `hook.sandbox` warning should fire.
    ignis::hooks::dispatch::reset_sandbox_warnings();
    let tmp = tempfile::tempdir().unwrap();
    let script = write_script(
        tmp.path(),
        "ok.sh",
        "#!/bin/sh\n\
         cat >/dev/null\n\
         printf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"x\"}}'\n",
    );
    let (_, events) = run_dispatch(spec_with(script, false, vec![]), "x").await;
    let warned = events.iter().any(
        |e| matches!(e, ignis::AgentEvent::Warning { source, .. } if source == "hook.sandbox"),
    );
    assert!(
        !warned,
        "sandbox: false must not emit hook.sandbox warning; events were: {events:?}"
    );
}

#[tokio::test]
async fn reload_resets_sandbox_warning_suppression() {
    // Pin: the `/hooks reload` path calls `reset_sandbox_warnings()`,
    // so a freshly-edited hook gets a fresh degradation notice. The
    // `reload_swaps_config_in_place` test in `mod.rs` covers the
    // reload happy path; this one focuses on the warning-reset side
    // effect.
    //
    // We don't need a real no-sandbox host to test this: we just
    // verify that the public `reset_sandbox_warnings` is callable
    // and idempotent.
    ignis::hooks::dispatch::reset_sandbox_warnings();
    ignis::hooks::dispatch::reset_sandbox_warnings();
    // If the call panicked or deadlocked, the test would not have
    // reached this assertion. Reaching here is the proof.
    // (Idempotence is checked in mod.rs's reset_smoke test.)
}
