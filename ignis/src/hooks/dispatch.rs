//! Spawn one hook subprocess, send the JSON envelope on stdin, read stdout,
//! triage the exit code. Designed so a runaway, hung, or crashing hook can
//! never kill the agent loop — every failure path returns a `HookOutcome`
//! the caller can turn into "use the original value + emit Warning".
//!
//! Process model:
//!   * env: `Command::env_clear()` + an explicit allowlist (`PATH HOME USER
//!     LANG LC_ALL TZ` always, plus whatever names the hook declared in
//!     `spec.env`). Closes the v1 credential-exfil gap.
//!   * sandbox: on Linux, when `spec.sandbox` is true, a `pre_exec` closure
//!     installs the default Landlock ruleset between fork and execve so the
//!     child can only read its own folder + lib paths + TLS roots and can
//!     only write `$TMPDIR`. See `super::sandbox`.
//!   * spawn via `tokio::process::Command` with piped stdin/stdout/stderr;
//!   * inside ONE `tokio::time::timeout`: write the envelope, drain stdout
//!     and stderr through a 1 MiB-per-stream cap, wait for the child;
//!   * on timeout, SIGTERM → 1 s grace → SIGKILL. `kill_on_drop` remains
//!     the safety net for panic paths.

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::config::HookSpec;
use super::protocol::{HookEvent, HookOutput};
use super::sandbox::{self, SandboxStatus};
use super::EventSender;
use crate::AgentEvent;

/// Universal env-var allowlist. Every hook always sees these names from
/// ignis's own environment (when set). The per-hook `env: [...]` list adds
/// to this — it doesn't replace it.
///
/// These six are the minimum a normal interpreter (`python3`, `bash`,
/// `ruby`) needs to start: `PATH` to find the binary, `HOME` to find dot-
/// files, `USER`/`LANG`/`LC_ALL`/`TZ` so any locale-sensitive code (date
/// formatting, error messages) behaves as the user expects.
const UNIVERSAL_ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TZ"];

/// 1 MiB per stream. A hook that emits more than this is almost certainly
/// runaway; we truncate and surface a `Warning` event. Same cap on stdout
/// and stderr so log capture is bounded too.
const STREAM_BUFFER_CAP: usize = 1024 * 1024;

/// Hooks whose `SandboxStatus::NotEnforced` warning has already been emitted
/// once this session. Keyed by `display_name()`. Reset by `/hooks reload`
/// via [`reset_sandbox_warnings`] so editing a hook re-arms the notice.
static SANDBOX_WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Clear the once-per-session "Landlock not enforced" suppression set. Called
/// from `HookRegistry::reload` so a freshly-edited hook gets a fresh notice
/// instead of being silently swallowed.
pub fn reset_sandbox_warnings() {
    if let Ok(mut guard) = SANDBOX_WARNED.lock() {
        if let Some(set) = guard.as_mut() {
            set.clear();
        }
    }
}

/// Returns `true` the first time a given hook name's degradation warning
/// fires this session, `false` afterwards.
fn should_emit_sandbox_warning(name: &str) -> bool {
    let mut guard = match SANDBOX_WARNED.lock() {
        Ok(g) => g,
        Err(_) => return false, // poisoned — fail quiet rather than spam
    };
    let set = guard.get_or_insert_with(HashSet::new);
    set.insert(name.to_string())
}

/// Outcome of a single hook invocation. None of these are errors at the
/// caller's level — `HookRegistry` decides whether each maps to "keep
/// running the chain", "stop with original value", etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Exit 0, parsed JSON, hook had a rewrite. Carry it forward.
    Mutated(String),
    /// Exit 0, but the hook did not produce a rewrite (or said
    /// `"continue": true` without `updatedInput`/`updatedOutput`). Caller
    /// passes through the original text and moves on.
    PassThrough,
    /// Exit 2 — hook explicitly blocked the chain. Stderr is shown to the
    /// user when this is honoured (only `UserPromptSubmit`).
    Blocked { stderr: String },
    /// Anything else (non-zero exit, malformed JSON, missing binary,
    /// timeout). Caller uses the original text and surfaces a Warning.
    SoftFailed { reason: String },
}

/// Context carried into each dispatch call so the envelope's `session_id`
/// and `cwd` line up with the running session.
#[derive(Debug, Clone)]
pub struct DispatchContext<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
}

#[derive(Serialize)]
struct WireEnvelope<'a> {
    hook_event_name: &'static str,
    session_id: &'a str,
    cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
}

/// Run one hook and return the outcome. Never returns an Err — every
/// failure mode maps to `HookOutcome::SoftFailed` (or `Blocked`).
///
/// `tx` is the live `AgentEvent::Warning` channel. When present, the
/// dispatcher emits a `Warning` for two v2 conditions that don't change
/// the outcome:
///   * stdout/stderr truncated at `STREAM_BUFFER_CAP`;
///   * Landlock unavailable on Linux (`SandboxStatus::NotEnforced`) — at
///     most once per hook name per session.
pub async fn run_hook(
    spec: &HookSpec,
    event: HookEvent,
    payload: &str,
    ctx: &DispatchContext<'_>,
    tx: Option<&EventSender>,
) -> HookOutcome {
    let started = std::time::Instant::now();
    let cmd_name = spec.display_name();
    let span = tracing::info_span!(
        "ignis.hook",
        event = event.as_str(),
        command = %cmd_name,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        sandbox.status = tracing::field::Empty,
    );
    let _enter = span.enter();

    let envelope = WireEnvelope {
        hook_event_name: event.as_str(),
        session_id: ctx.session_id,
        cwd: ctx.cwd,
        prompt: match event {
            HookEvent::UserPromptSubmit => Some(payload),
            HookEvent::AssistantMessageRender => None,
        },
        content: match event {
            HookEvent::AssistantMessageRender => Some(payload),
            HookEvent::UserPromptSubmit => None,
        },
    };
    let stdin_bytes = match serde_json::to_vec(&envelope) {
        Ok(b) => b,
        Err(e) => {
            return record(
                &span,
                started,
                HookOutcome::SoftFailed {
                    reason: format!("envelope encode failed: {e}"),
                },
            );
        }
    };

    // === Env-var allowlist (all platforms, v2 layer 1) =====================
    // env_clear() first, then re-add only the explicit allowlist plus any
    // hook-declared names. Skips empty values silently (env vars set to ""
    // are legal but rarely useful and surprise more than they help).
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for &name in UNIVERSAL_ENV_ALLOWLIST {
        if let Some(val) = std::env::var_os(name) {
            cmd.env(name, val);
        }
    }
    for name in &spec.env {
        if let Some(val) = std::env::var_os(name) {
            cmd.env(name, val);
        }
    }

    // === Linux Landlock filesystem sandbox (v2 layer 2) ====================
    // The closure runs in the forked child between `fork` and `execve` — the
    // only safe seam for Landlock's self-restrict semantics. On non-Linux,
    // `apply` is a no-op stub returning PlatformUnsupported (which we only
    // log once per session, below).
    //
    // Hook folder: the directory containing the program. Falls back to "/"
    // if the path has no parent (e.g. a bare binary name resolved by PATH);
    // "/" is harmless because Landlock only adds rules for paths that exist.
    let hook_folder: std::path::PathBuf = spec
        .program
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let want_sandbox = spec.sandbox;
    #[cfg(unix)]
    {
        // Note: `tokio::process::Command::pre_exec` is the tokio-owned method
        // (gated on cfg(unix)), not the std `CommandExt` trait — no import
        // needed here.
        if want_sandbox {
            let folder_for_closure = hook_folder.clone();
            // SAFETY: the closure runs in the forked child before execve. It
            // must be async-signal-safe — no allocation that can panic, no
            // global locks, no tracing. `sandbox::apply` is documented to
            // hold to that contract; we ignore its returned status here
            // because there's no back-channel to the parent. The parent
            // re-runs the rule build under `is_test()`-style checks via the
            // unit test in sandbox.rs and the integration test in
            // tests/hook_sandbox.rs to confirm the kernel cooperated.
            unsafe {
                cmd.pre_exec(move || {
                    // Map our SandboxStatus into io::Result success/failure
                    // categories. NotEnforced is NOT a hard failure (the
                    // kernel just doesn't support Landlock); the hook still
                    // runs unconfined and the parent's startup-warning path
                    // handles the user-visible degradation notice. Hard
                    // errors (e.g. EPERM mid-rule) DO fail the exec so the
                    // hook is not silently unconfined when it shouldn't be.
                    match sandbox::apply(&folder_for_closure) {
                        Ok(_) => Ok(()),
                        Err(e) => Err(e),
                    }
                });
            }
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return record(
                &span,
                started,
                HookOutcome::SoftFailed {
                    reason: format!("spawn failed: {e}"),
                },
            );
        }
    };

    // Record sandbox status for telemetry. We re-evaluate the kernel ABI
    // level in the parent (cheap, no restriction applied) so dashboards see
    // whether the child got real confinement.
    let sandbox_status = sandbox_status_for_telemetry(spec);
    span.record("sandbox.status", sandbox_status.as_str());
    if sandbox_status == SandboxStatus::NotEnforced && should_emit_sandbox_warning(&cmd_name) {
        if let Some(tx) = tx {
            let _ = tx
                .send(AgentEvent::Warning {
                    source: "hook.sandbox".to_string(),
                    message: format!(
                        "{}: Landlock unavailable on this kernel; hook runs unconfined",
                        cmd_name
                    ),
                })
                .await;
        }
        tracing::warn!(hook = %cmd_name, "Landlock unavailable; hook unconfined");
    }

    // === Bounded stdout/stderr (v2 layer 4) ================================
    // Mirrors tokio's own `wait_with_output` internal pattern (join the wait
    // future with two drain futures) but adds a per-stream cap. The stdin
    // write runs concurrently as a spawned task so it can't block the join.
    //
    // `pipe.take(LIMIT).read_to_end(...)` reads at most LIMIT bytes — when
    // the returned buf length == LIMIT, the stream was either exactly LIMIT
    // bytes long or got truncated. We treat "==" as "truncated"; the
    // false-positive cost is just one extra `Warning` event.
    let timeout = Duration::from_millis(spec.timeout_ms);
    let stdin_handle = child.stdin.take();
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    // Drive stdin write as an independent task so it can complete while
    // wait/stdout/stderr are draining — avoids the n=4 `join!` ordering
    // races we hit when stdin and wait shared a `child` borrow.
    let stdin_task = tokio::spawn(async move {
        if let Some(mut s) = stdin_handle {
            if let Err(e) = s.write_all(&stdin_bytes).await {
                tracing::debug!(error = %e, "hook stdin write failed (child may have exited)");
            }
            // Drop closes the pipe so the child sees EOF.
            drop(s);
        }
    });

    let interaction = async {
        let stdout_fut = async {
            let mut buf: Vec<u8> = Vec::new();
            if let Some(pipe) = stdout_pipe.as_mut() {
                let _ = pipe
                    .take(STREAM_BUFFER_CAP as u64)
                    .read_to_end(&mut buf)
                    .await;
                // After cap, keep draining so the child can finish writing
                // (otherwise its pipe blocks and it never exits).
                let mut scratch = [0u8; 16 * 1024];
                loop {
                    match pipe.read(&mut scratch).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            }
            buf
        };
        let stderr_fut = async {
            let mut buf: Vec<u8> = Vec::new();
            if let Some(pipe) = stderr_pipe.as_mut() {
                let _ = pipe
                    .take(STREAM_BUFFER_CAP as u64)
                    .read_to_end(&mut buf)
                    .await;
                let mut scratch = [0u8; 16 * 1024];
                loop {
                    match pipe.read(&mut scratch).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            }
            buf
        };
        let wait_fut = child.wait();
        let (status_res, stdout_bytes, stderr_bytes) =
            tokio::join!(wait_fut, stdout_fut, stderr_fut);
        let status = status_res?;
        Ok::<(std::process::ExitStatus, Vec<u8>, Vec<u8>), std::io::Error>((
            status,
            stdout_bytes,
            stderr_bytes,
        ))
    };

    let wait = tokio::time::timeout(timeout, interaction).await;

    let (status, stdout_bytes, stderr_bytes) = match wait {
        Ok(Ok(triple)) => triple,
        Ok(Err(e)) => {
            return record(
                &span,
                started,
                HookOutcome::SoftFailed {
                    reason: format!("wait failed: {e}"),
                },
            );
        }
        Err(_) => {
            // === SIGTERM grace (v2 layer 3) ================================
            // `child` is still owned by this scope — the `interaction` future
            // only borrowed it via `child.wait()`. On timeout, send SIGTERM,
            // wait up to 1 s for clean exit, then SIGKILL if the child is
            // still alive.
            //
            // tokio's `Child::start_kill()` actually maps to std's
            // `Child::kill()` which sends SIGKILL — so we use libc::kill
            // directly to deliver SIGTERM. On non-Unix builds we have no
            // SIGTERM-equivalent that we can address by PID; the
            // `kill_on_drop` flag + `child.kill().await` below covers it,
            // but the grace window degenerates to "no grace" which v2
            // accepts.
            #[cfg(unix)]
            {
                if let Some(pid) = child.id() {
                    // SAFETY: libc::kill is async-signal-safe; `pid` came
                    // from a Child we still own (no reuse race), and
                    // SIGTERM is well-defined.
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }
            let grace = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            if grace.is_err() {
                // Refused to exit within the grace; escalate to SIGKILL.
                let _ = child.kill().await;
            } else {
                tracing::debug!("hook exited within SIGTERM grace window");
            }
            // Stdin task is harmless — it'll error or finish on its own once
            // the child's pipe closes — but abort so it doesn't outlive the
            // call and leak a handle.
            stdin_task.abort();
            return record(
                &span,
                started,
                HookOutcome::SoftFailed {
                    reason: format!("timed out after {}ms", spec.timeout_ms),
                },
            );
        }
    };

    // Reap the stdin writer — it almost certainly finished long ago since
    // the child has exited, but ignoring the JoinHandle would leave the
    // task half-detached and surface as a warning in some test harnesses.
    let _ = stdin_task.await;

    let stdout_truncated = stdout_bytes.len() >= STREAM_BUFFER_CAP;
    let stderr_truncated = stderr_bytes.len() >= STREAM_BUFFER_CAP;
    if stdout_truncated {
        emit_buffer_warning(tx, &cmd_name, "stdout").await;
    }
    if stderr_truncated {
        emit_buffer_warning(tx, &cmd_name, "stderr").await;
    }
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    if status.code() == Some(2) {
        return record(&span, started, HookOutcome::Blocked { stderr });
    }
    if !status.success() {
        let reason = match status.code() {
            Some(code) => format!("exit {code}: {}", trim_stderr(&stderr)),
            None => format!("terminated by signal: {}", trim_stderr(&stderr)),
        };
        return record(&span, started, HookOutcome::SoftFailed { reason });
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return record(&span, started, HookOutcome::PassThrough);
    }
    let parsed: HookOutput = match serde_json::from_str(trimmed) {
        Ok(p) => p,
        Err(e) => {
            return record(
                &span,
                started,
                HookOutcome::SoftFailed {
                    reason: format!("invalid JSON on stdout: {e}"),
                },
            );
        }
    };

    if parsed.r#continue == Some(false) {
        return record(&span, started, HookOutcome::Blocked { stderr });
    }

    let rewrite = parsed.hook_specific_output.and_then(|s| match event {
        HookEvent::UserPromptSubmit => s.updated_input,
        HookEvent::AssistantMessageRender => s.updated_output,
    });
    match rewrite {
        Some(updated) => record(&span, started, HookOutcome::Mutated(updated)),
        None => record(&span, started, HookOutcome::PassThrough),
    }
}

/// Compute the sandbox status to report to telemetry from the parent. We do
/// NOT actually install Landlock here — that happens in the child's
/// `pre_exec`. This is just for span attributes / dashboards.
///
/// On Linux, the kernel-level ABI is probed once per session via a raw
/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
/// syscall. The result is cached: kernels either support Landlock or not,
/// it doesn't change at runtime.
fn sandbox_status_for_telemetry(spec: &HookSpec) -> SandboxStatus {
    if !spec.sandbox {
        return SandboxStatus::Disabled;
    }
    #[cfg(target_os = "linux")]
    {
        use std::sync::OnceLock;
        static CACHED: OnceLock<SandboxStatus> = OnceLock::new();
        *CACHED.get_or_init(probe_landlock_kernel)
    }
    #[cfg(not(target_os = "linux"))]
    {
        SandboxStatus::PlatformUnsupported
    }
}

/// Probe the kernel's Landlock ABI version once per session. Returns
/// `FullyEnforced` on any kernel that recognises the syscall (V1+),
/// `NotEnforced` when the syscall returns -1 (ENOSYS / EOPNOTSUPP). The
/// parent's status is what the child *will* see, modulo race conditions
/// that don't exist for a CPU-feature LSM.
#[cfg(target_os = "linux")]
fn probe_landlock_kernel() -> SandboxStatus {
    // Raw syscall: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`.
    // Returns the supported ABI version (>= 1) on success, or -1 with errno
    // ENOSYS / EOPNOTSUPP when Landlock is not available.
    //
    // SAFETY: passing NULL + size 0 + flags = 1 (LANDLOCK_CREATE_RULESET_VERSION).
    // That argument tuple is documented to never mutate userspace; it only
    // reports the supported ABI as the return value.
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if ret >= 1 {
        SandboxStatus::FullyEnforced
    } else {
        SandboxStatus::NotEnforced
    }
}

async fn emit_buffer_warning(tx: Option<&EventSender>, hook: &str, stream: &str) {
    if let Some(tx) = tx {
        let _ = tx
            .send(AgentEvent::Warning {
                source: "hook.buffer".to_string(),
                message: format!("{hook}: {stream} truncated at 1 MiB"),
            })
            .await;
    }
    tracing::warn!(hook, stream, "hook output truncated at 1 MiB cap");
}

fn record(span: &tracing::Span, started: std::time::Instant, outcome: HookOutcome) -> HookOutcome {
    span.record("duration_ms", started.elapsed().as_millis() as u64);
    let label = match &outcome {
        HookOutcome::Mutated(_) => "mutated",
        HookOutcome::PassThrough => "pass_through",
        HookOutcome::Blocked { .. } => "blocked",
        HookOutcome::SoftFailed { .. } => "failed",
    };
    span.record("outcome", label);
    outcome
}

fn trim_stderr(stderr: &str) -> String {
    let one_line = stderr.replace('\n', " ");
    super::truncate_chars(&one_line, 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> DispatchContext<'static> {
        DispatchContext {
            session_id: "sess",
            cwd: "/tmp",
        }
    }

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        // chmod +x
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        p
    }

    #[tokio::test]
    async fn success_returns_mutated() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-success");
        let script = write_script(
            &tmp,
            "ok.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"rewritten\"}}'\n",
        );
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            ..HookSpec::default()
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "original", &ctx(), None).await;
        assert_eq!(out, HookOutcome::Mutated("rewritten".to_string()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn exit_zero_empty_stdout_is_passthrough() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-passthrough");
        let script = write_script(&tmp, "noop.sh", "#!/bin/sh\ncat >/dev/null\n");
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            ..HookSpec::default()
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "original", &ctx(), None).await;
        assert_eq!(out, HookOutcome::PassThrough);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn exit_two_is_blocked() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-block");
        let script = write_script(
            &tmp,
            "block.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf 'nope' >&2\nexit 2\n",
        );
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            ..HookSpec::default()
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx(), None).await;
        match out {
            HookOutcome::Blocked { stderr } => assert!(stderr.contains("nope")),
            other => panic!("expected Blocked, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn malformed_json_is_soft_failed() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-badjson");
        let script = write_script(
            &tmp,
            "bad.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf 'not json at all'\n",
        );
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            ..HookSpec::default()
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx(), None).await;
        match out {
            HookOutcome::SoftFailed { reason } => assert!(reason.contains("invalid JSON")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn missing_binary_is_soft_failed() {
        let spec = HookSpec {
            program: PathBuf::from("/nonexistent/binary/__ignis_no_such_path__"),
            args: vec![],
            timeout_ms: 1_000,
            ..HookSpec::default()
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx(), None).await;
        matches!(out, HookOutcome::SoftFailed { .. });
    }

    #[tokio::test]
    async fn timeout_is_soft_failed() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-timeout");
        let script = write_script(&tmp, "slow.sh", "#!/bin/sh\ncat >/dev/null\nsleep 5\n");
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 200,
            ..HookSpec::default()
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx(), None).await;
        match out {
            HookOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn hook_that_never_reads_stdin_still_times_out() {
        // Regression: previously the dispatcher wrote stdin → blocked-on-PIPE
        // forever if the child never drained it, and only started the
        // timeout AFTER the write. The fix wraps write + wait in one
        // timeout, so this test pins the behaviour.
        //
        // We send an over-sized payload to defeat any pipe-buffer slack and
        // give the script no chance to drain it.
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-stdin-deadlock");
        let script = write_script(
            &tmp,
            "sleep-no-read.sh",
            // No `cat >/dev/null` — child never reads stdin. sleep is long
            // enough that the timeout dominates; pipe write would otherwise
            // block until the child exited.
            "#!/bin/sh\nsleep 30\n",
        );
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 150,
            ..HookSpec::default()
        };
        let big_payload = "x".repeat(256 * 1024); // > 64 KiB pipe buf
        let t0 = std::time::Instant::now();
        let out = run_hook(
            &spec,
            HookEvent::UserPromptSubmit,
            &big_payload,
            &ctx(),
            None,
        )
        .await;
        let elapsed = t0.elapsed();
        match out {
            HookOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        // Must return well within the long sleep (with slack for spawn).
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "timeout did not fire promptly: elapsed = {elapsed:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---------------------------------------------------------------------
    // v2 layer tests
    // ---------------------------------------------------------------------

    /// Helper: a hook script that emits every name=value pair from its env to
    /// stdout, wrapped in the hook protocol JSON shape. Lets us assert the
    /// allowlist actually filters.
    fn env_dump_script(dir: &std::path::Path) -> PathBuf {
        write_script(
            dir,
            "env-dump.sh",
            // Each line is a name=value pair. We trim quotes/special chars
            // by constructing a JSON value via printf and embedding it in
            // updatedInput. Dash's printf is POSIX, so this is portable.
            r#"#!/bin/sh
cat >/dev/null
out=""
while IFS= read -r line; do
    [ -z "$line" ] && continue
    case "$line" in
        BASH_*|_*|SHLVL=*|PWD=*|OLDPWD=*) continue ;;
    esac
    out="$out$line;"
done <<EOF
$(env)
EOF
# JSON-escape minimally: replace " with ' so we don't need a real encoder.
out=$(printf '%s' "$out" | tr '"' "'")
printf '%s' "{\"hookSpecificOutput\":{\"updatedInput\":\"$out\"}}"
"#,
        )
    }

    #[tokio::test]
    async fn env_filter_drops_secrets_unless_declared() {
        // R3 success criterion: with `env: []` the child sees PATH/HOME but
        // NOT a secret env var the parent had set; with `env: ["KEY"]` the
        // child sees the secret. Setting parent-process env vars is safe
        // here because each test owns its own subprocess.
        let tmp = crate::util::unique_temp_dir("ignis-hook-envfilter");
        let script = env_dump_script(&tmp);

        // Parent sets a fake secret. We don't unset it before assert; the
        // test only inspects the child's view via the JSON it returns.
        std::env::set_var("IGNIS_HOOK_TEST_SECRET", "leaked-credential-XYZ");

        // 1. No declaration → child must NOT see the secret.
        let spec_no_env = HookSpec {
            program: script.clone(),
            args: vec![],
            timeout_ms: 5_000,
            env: vec![],
            sandbox: false,
        };
        let out = run_hook(&spec_no_env, HookEvent::UserPromptSubmit, "x", &ctx(), None).await;
        let body = match out {
            HookOutcome::Mutated(s) => s,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(
            body.contains("PATH="),
            "PATH should always pass through; got: {body}"
        );
        assert!(
            !body.contains("IGNIS_HOOK_TEST_SECRET"),
            "secret env var leaked despite empty allowlist: {body}"
        );

        // 2. Explicit declaration → child DOES see the secret.
        let spec_with_env = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            env: vec!["IGNIS_HOOK_TEST_SECRET".to_string()],
            sandbox: false,
        };
        let out2 = run_hook(
            &spec_with_env,
            HookEvent::UserPromptSubmit,
            "x",
            &ctx(),
            None,
        )
        .await;
        let body2 = match out2 {
            HookOutcome::Mutated(s) => s,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(
            body2.contains("IGNIS_HOOK_TEST_SECRET=leaked-credential-XYZ"),
            "explicit env declaration did not pass secret through: {body2}"
        );

        std::env::remove_var("IGNIS_HOOK_TEST_SECRET");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// SIGTERM grace: a hook that ignores SIGTERM and sleeps 30 s should be
    /// SIGKILL'd ~1 s after the configured timeout. Total wall time:
    /// `timeout + ~1 s`, well under the 30 s sleep.
    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_grace_kills_uncooperative_hook_after_one_second() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-sigterm");
        let script = write_script(
            &tmp,
            "ignore-term.sh",
            // `trap '' TERM` ignores SIGTERM. The shell still respects
            // SIGKILL, so after the 1 s grace we expect SIGKILL to land.
            // `cat >/dev/null &` lets the hook still read stdin in the
            // background so the dispatcher's write doesn't EPIPE.
            "#!/bin/sh\ntrap '' TERM\ncat >/dev/null &\nsleep 30\n",
        );
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 100,
            ..HookSpec::default()
        };
        let t0 = std::time::Instant::now();
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx(), None).await;
        let elapsed = t0.elapsed();

        match out {
            HookOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        // Lower bound: SIGTERM fires at ~100ms, then 1s grace, then SIGKILL.
        // Total >= 1.05s (a hair under 1.1s allows for SIGTERM-then-quick-
        // exit short-circuiting the grace; we measure the worst case).
        assert!(
            elapsed >= Duration::from_millis(1050),
            "did not honour grace window: elapsed = {elapsed:?}"
        );
        // Upper bound: must NOT wait the full 30 s sleep — SIGKILL kicks in.
        assert!(
            elapsed < Duration::from_secs(3),
            "SIGKILL did not land promptly: elapsed = {elapsed:?}"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Buffer cap: a hook that emits 2 MiB on stdout should be truncated at
    /// exactly 1 MiB and a `hook.buffer` Warning event should land on tx.
    #[tokio::test]
    async fn stdout_truncated_at_one_mib_and_warning_emitted() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-bufcap");
        // 2 MiB of 'x' via dd. We use head -c 2M from /dev/zero | tr because
        // some `dd` implementations on minimal images lack the iflag arg.
        // Final newline omitted so we control the byte count exactly.
        let script = write_script(
            &tmp,
            "spew.sh",
            // 2 * 1024 * 1024 = 2097152 bytes
            "#!/bin/sh\ncat >/dev/null\nhead -c 2097152 /dev/zero | tr '\\0' x\n",
        );
        let spec = HookSpec {
            program: script,
            args: vec![],
            timeout_ms: 10_000,
            ..HookSpec::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx(), Some(&tx)).await;
        // Spew of 2 MiB of 'x' is not valid JSON; outcome is SoftFailed.
        // The point of the test is the warning + cap, not the parse.
        matches!(out, HookOutcome::SoftFailed { .. });
        drop(tx);

        let mut got_buffer_warning = false;
        let mut buffer_msg = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Warning { source, message } = ev {
                if source == "hook.buffer" {
                    got_buffer_warning = true;
                    buffer_msg = message;
                }
            }
        }
        assert!(
            got_buffer_warning,
            "expected a hook.buffer Warning, none arrived"
        );
        assert!(
            buffer_msg.contains("stdout truncated at 1 MiB"),
            "unexpected warning text: {buffer_msg}"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
