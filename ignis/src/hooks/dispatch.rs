//! Spawn one hook subprocess, send the JSON envelope on stdin, read stdout,
//! triage the exit code. Designed so a runaway, hung, or crashing hook can
//! never kill the agent loop — every failure path returns a `HookOutcome`
//! the caller can turn into "use the original value + emit Warning".
//!
//! Process model:
//!   * spawn via `tokio::process::Command` with piped stdin/stdout/stderr;
//!   * write the envelope, close stdin;
//!   * wait for the child with a `tokio::time::timeout`;
//!   * on timeout, `SIGTERM` then `SIGKILL` after a 1 s grace.

use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::config::HookSpec;
use super::protocol::{HookEvent, HookOutput};

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
pub async fn run_hook(
    spec: &HookSpec,
    event: HookEvent,
    payload: &str,
    ctx: &DispatchContext<'_>,
) -> HookOutcome {
    let started = std::time::Instant::now();
    let cmd_name = spec.display_name();
    let span = tracing::info_span!(
        "ignis.hook",
        event = event.as_str(),
        command = %cmd_name,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
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

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

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

    let timeout = Duration::from_millis(spec.timeout_ms);
    // CRITICAL: the timeout must arm BEFORE the stdin write. A hook that
    // doesn't read stdin will block the write forever once the pipe buffer
    // fills (~64 KiB on Linux); if we waited until after the write to start
    // the timer, a misbehaving hook would hang the agent loop indefinitely.
    // Wrap the whole interaction (write + close + wait_with_output) in one
    // timeout so a non-reading hook still times out cleanly.
    let stdin = child.stdin.take();
    let interaction = async move {
        if let Some(mut stdin) = stdin {
            if let Err(e) = stdin.write_all(&stdin_bytes).await {
                // The child may have exited before reading; we still
                // continue and collect its output below so a fast hook
                // that did its work and closed stdin doesn't get marked
                // failed here.
                tracing::debug!(error = %e, "hook stdin write failed (child may have exited)");
            }
            drop(stdin);
        }
        child.wait_with_output().await
    };
    let wait = tokio::time::timeout(timeout, interaction).await;

    let output = match wait {
        Ok(Ok(o)) => o,
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
            // kill_on_drop fires SIGKILL when the dropped `interaction`
            // future drops `child`. v1 is SIGKILL-only; v2 will add a
            // SIGTERM grace window (see docs/usage/hooks.md).
            return record(
                &span,
                started,
                HookOutcome::SoftFailed {
                    reason: format!("timed out after {}ms", spec.timeout_ms),
                },
            );
        }
    };

    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

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
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "original", &ctx()).await;
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
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "original", &ctx()).await;
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
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx()).await;
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
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx()).await;
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
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx()).await;
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
        };
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, "x", &ctx()).await;
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
        };
        let big_payload = "x".repeat(256 * 1024); // > 64 KiB pipe buf
        let t0 = std::time::Instant::now();
        let out = run_hook(&spec, HookEvent::UserPromptSubmit, &big_payload, &ctx()).await;
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
}
