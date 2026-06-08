//! Spawn one hook subprocess, send the JSON envelope on stdin, read stdout,
//! triage the exit code. Designed so a runaway, hung, or crashing hook can
//! never kill the agent loop — every failure path returns a `ExtensionOutcome`
//! the caller can turn into "use the original value + emit Warning".
//!
//! Process model:
//!   * spawn via `tokio::process::Command` with piped stdin/stdout/stderr;
//!   * inside ONE `tokio::time::timeout`: write the envelope, close stdin,
//!     wait for the child;
//!   * on timeout, `kill_on_drop` fires SIGKILL when the future is dropped.
//!     v2 will add a SIGTERM grace window before SIGKILL.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::config::ExtensionSpec;
use super::protocol::{ExtensionEvent, ExtensionInput, ExtensionOutput};

/// Outcome of a single hook invocation. None of these are errors at the
/// caller's level — `ExtensionRegistry` decides whether each maps to "keep
/// running the chain", "stop with original value", etc.
///
/// `Eq` is not derived: [`ExtensionOutcome::MutatedJson`] holds a
/// `serde_json::Value`, which is `PartialEq` but not `Eq` because
/// `Value::Number` can wrap an `f64`.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionOutcome {
    /// Exit 0, no rewrite or context injection — caller passes the
    /// original payload through.
    PassThrough,
    /// Text rewrite for `UserPromptSubmit`, `AssistantMessageRender`, or
    /// `SystemPromptCompose` (`updatedInput` for the first, `updatedOutput`
    /// for the second, `updatedSystemPrompt` for the third).
    Mutated(String),
    /// Object rewrite of `tool_input` for `PreToolUse` (`updatedInput`
    /// shaped as a JSON object).
    MutatedJson(serde_json::Value),
    /// `additionalContext` from the hook response — to be injected as a
    /// system reminder before the next LLM call. Used by `SessionStart`,
    /// `UserPromptSubmit`, `PostToolUse`, `PreCompact`, `PostCompact`,
    /// `Stop` (when not the inverted block — see [`Self::KeepLooping`]).
    InjectContext(String),
    /// Hook explicitly blocked the chain (`exit 2` or
    /// `decision: "block"`). For `Stop` this is inverted to
    /// [`Self::KeepLooping`] — see the dispatch path below.
    /// `reason` is the structured block reason from
    /// `ExtensionOutput.reason`; `stderr` is the raw subprocess stderr (used
    /// for v1's exit-2 path where `reason` is empty).
    Blocked {
        stderr: String,
        reason: Option<String>,
    },
    /// `Stop` event's "decision:'block' = keep looping" inversion. The
    /// `reason` is injected as a system reminder framed
    /// `"<hook> stopped continuation: <reason>"`.
    KeepLooping { reason: String },
    /// Anything else (non-zero exit, malformed JSON, missing binary,
    /// timeout). Caller uses the original payload and surfaces a
    /// `Warning`.
    SoftFailed { reason: String },
}

/// Context carried into each dispatch call so the envelope's `session_id`
/// and `cwd` line up with the running session.
#[derive(Debug, Clone)]
pub struct DispatchContext<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
}

/// Typed per-event input handed to [`run_hook`]. Each variant carries
/// exactly the fields its event's envelope populates — this is what
/// keeps `run_hook` from needing nine separate signatures while still
/// guaranteeing at the type level that (say) `PreToolUse` receives a
/// `tool_input` and `PostCompact` receives a `summary`.
///
/// `event()` recovers the discriminator; `into_envelope()` builds the
/// outgoing [`ExtensionInput`].
#[derive(Debug, Clone)]
pub enum ExtensionPayload<'a> {
    UserPromptSubmit {
        prompt: &'a str,
    },
    AssistantMessageRender {
        content: &'a str,
    },
    SystemPromptCompose {
        system_prompt: &'a str,
        model: &'a str,
    },
    PreToolUse {
        tool_name: &'a str,
        tool_input: &'a serde_json::Value,
    },
    PostToolUse {
        tool_name: &'a str,
        tool_input: &'a serde_json::Value,
        tool_response: &'a serde_json::Value,
    },
    PreCompact {
        trigger: &'a str,
        transcript_path: &'a str,
    },
    PostCompact {
        trigger: &'a str,
        summary: &'a str,
    },
    SessionStart {
        source: &'a str,
    },
    Stop {
        transcript_path: &'a str,
    },
}

impl<'a> ExtensionPayload<'a> {
    /// The [`ExtensionEvent`] this payload variant corresponds to. The
    /// discriminator decides the wire-shape and outcome mapping in
    /// [`run_hook`].
    pub fn event(&self) -> ExtensionEvent {
        match self {
            ExtensionPayload::UserPromptSubmit { .. } => ExtensionEvent::UserPromptSubmit,
            ExtensionPayload::AssistantMessageRender { .. } => {
                ExtensionEvent::AssistantMessageRender
            }
            ExtensionPayload::SystemPromptCompose { .. } => ExtensionEvent::SystemPromptCompose,
            ExtensionPayload::PreToolUse { .. } => ExtensionEvent::PreToolUse,
            ExtensionPayload::PostToolUse { .. } => ExtensionEvent::PostToolUse,
            ExtensionPayload::PreCompact { .. } => ExtensionEvent::PreCompact,
            ExtensionPayload::PostCompact { .. } => ExtensionEvent::PostCompact,
            ExtensionPayload::SessionStart { .. } => ExtensionEvent::SessionStart,
            ExtensionPayload::Stop { .. } => ExtensionEvent::Stop,
        }
    }

    /// The `tool_name` carried in the payload, if any. Used by the
    /// registry to evaluate a hook's `matcher` regex *before* spawning
    /// the subprocess — a non-matching hook is skipped without paying
    /// the spawn cost.
    pub fn tool_name(&self) -> Option<&'a str> {
        match self {
            ExtensionPayload::PreToolUse { tool_name, .. }
            | ExtensionPayload::PostToolUse { tool_name, .. } => Some(tool_name),
            _ => None,
        }
    }

    fn build_envelope(&self, ctx: &DispatchContext<'_>) -> ExtensionInput {
        let mut env = ExtensionInput {
            hook_event_name: self.event().as_str().to_string(),
            session_id: ctx.session_id.to_string(),
            cwd: ctx.cwd.to_string(),
            ..Default::default()
        };
        match self {
            ExtensionPayload::UserPromptSubmit { prompt } => {
                env.prompt = Some((*prompt).to_string());
            }
            ExtensionPayload::AssistantMessageRender { content } => {
                env.content = Some((*content).to_string());
            }
            ExtensionPayload::SystemPromptCompose {
                system_prompt,
                model,
            } => {
                env.system_prompt = Some((*system_prompt).to_string());
                env.model = Some((*model).to_string());
            }
            ExtensionPayload::PreToolUse {
                tool_name,
                tool_input,
            } => {
                env.tool_name = Some((*tool_name).to_string());
                env.tool_input = Some((*tool_input).clone());
            }
            ExtensionPayload::PostToolUse {
                tool_name,
                tool_input,
                tool_response,
            } => {
                env.tool_name = Some((*tool_name).to_string());
                env.tool_input = Some((*tool_input).clone());
                env.tool_response = Some((*tool_response).clone());
            }
            ExtensionPayload::PreCompact {
                trigger,
                transcript_path,
            } => {
                env.trigger = Some((*trigger).to_string());
                env.transcript_path = Some((*transcript_path).to_string());
            }
            ExtensionPayload::PostCompact { trigger, summary } => {
                env.trigger = Some((*trigger).to_string());
                env.summary = Some((*summary).to_string());
            }
            ExtensionPayload::SessionStart { source } => {
                env.source = Some((*source).to_string());
            }
            ExtensionPayload::Stop { transcript_path } => {
                env.transcript_path = Some((*transcript_path).to_string());
            }
        }
        env
    }
}

/// Run one hook and return the outcome. Never returns an `Err` — every
/// failure mode maps to [`ExtensionOutcome::SoftFailed`] (or [`ExtensionOutcome::Blocked`]).
///
/// The payload's variant (and not a separate `event` arg) decides the
/// envelope shape, the response-field this dispatch reads (`updated_input`
/// vs `updated_output` vs `updated_system_prompt` vs `additional_context`),
/// and whether `decision: "block"` short-circuits or is inverted to
/// [`ExtensionOutcome::KeepLooping`] (the `Stop` case).
pub async fn run_hook(
    spec: &ExtensionSpec,
    payload: ExtensionPayload<'_>,
    ctx: &DispatchContext<'_>,
) -> ExtensionOutcome {
    let started = std::time::Instant::now();
    let event = payload.event();
    let cmd_name = spec.display_name();
    let span = tracing::info_span!(
        "ignis.extension",
        event = event.as_str(),
        command = %cmd_name,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    let _enter = span.enter();

    let envelope = payload.build_envelope(ctx);
    let stdin_bytes = match serde_json::to_vec(&envelope) {
        Ok(b) => b,
        Err(e) => {
            return record(
                &span,
                started,
                ExtensionOutcome::SoftFailed {
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
                ExtensionOutcome::SoftFailed {
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
                ExtensionOutcome::SoftFailed {
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
                ExtensionOutcome::SoftFailed {
                    reason: format!("timed out after {}ms", spec.timeout_ms),
                },
            );
        }
    };

    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if status.code() == Some(2) {
        // Exit-2 blocking. For `Stop` the inversion ("block = keep
        // looping") flips this into `KeepLooping`; stderr stands in for
        // the reason text when no JSON came back.
        let outcome = if matches!(event, ExtensionEvent::Stop) {
            ExtensionOutcome::KeepLooping {
                reason: trim_stderr(&stderr),
            }
        } else {
            ExtensionOutcome::Blocked {
                stderr,
                reason: None,
            }
        };
        return record(&span, started, outcome);
    }
    if !status.success() {
        let reason = match status.code() {
            Some(code) => format!("exit {code}: {}", trim_stderr(&stderr)),
            None => format!("terminated by signal: {}", trim_stderr(&stderr)),
        };
        return record(&span, started, ExtensionOutcome::SoftFailed { reason });
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return record(&span, started, ExtensionOutcome::PassThrough);
    }
    let parsed: ExtensionOutput = match serde_json::from_str(trimmed) {
        Ok(p) => p,
        Err(e) => {
            return record(
                &span,
                started,
                ExtensionOutcome::SoftFailed {
                    reason: format!("invalid JSON on stdout: {e}"),
                },
            );
        }
    };

    record(&span, started, classify_response(event, parsed, stderr))
}

/// Translate a parsed [`ExtensionOutput`] into a per-event [`ExtensionOutcome`].
///
/// Precedence per event:
/// * `continue: false` → [`ExtensionOutcome::Blocked`] (or `KeepLooping` for
///   `Stop`) — the hardest stop.
/// * `decision: "block"` → same as above.
/// * `updated_*` field for the event → [`ExtensionOutcome::Mutated`] or
///   [`ExtensionOutcome::MutatedJson`].
/// * `additional_context` → [`ExtensionOutcome::InjectContext`].
/// * otherwise → [`ExtensionOutcome::PassThrough`].
///
/// The text-rewrite vs context-injection axes are exclusive in the
/// outcome — a hook that returns both gets the rewrite, and the
/// `additional_context` is logged as ignored. Splitting them into
/// two simultaneous outcomes is left for v3 once a real use case
/// shows up.
fn classify_response(
    event: ExtensionEvent,
    parsed: ExtensionOutput,
    stderr: String,
) -> ExtensionOutcome {
    let reason = parsed.reason.clone();
    let blocked = parsed.r#continue == Some(false) || parsed.decision.as_deref() == Some("block");
    if blocked {
        return if matches!(event, ExtensionEvent::Stop) {
            ExtensionOutcome::KeepLooping {
                reason: reason.unwrap_or_else(|| trim_stderr(&stderr)),
            }
        } else {
            ExtensionOutcome::Blocked { stderr, reason }
        };
    }

    let Some(spec) = parsed.hook_specific_output else {
        return ExtensionOutcome::PassThrough;
    };

    // Per-event rewrite extraction.
    let rewrite_text: Option<String> = match event {
        ExtensionEvent::UserPromptSubmit => spec
            .updated_input
            .as_ref()
            .and_then(|v| v.as_str())
            .map(String::from),
        ExtensionEvent::AssistantMessageRender => spec.updated_output.clone(),
        ExtensionEvent::SystemPromptCompose => spec.updated_system_prompt.clone(),
        _ => None,
    };
    if let Some(t) = rewrite_text {
        if spec.additional_context.is_some() {
            tracing::debug!(
                event = event.as_str(),
                "hook returned both rewrite and additional_context; using rewrite"
            );
        }
        return ExtensionOutcome::Mutated(t);
    }

    // PreToolUse: object rewrite of `tool_input`.
    if matches!(event, ExtensionEvent::PreToolUse) {
        if let Some(v) = spec.updated_input {
            if v.is_object() {
                return ExtensionOutcome::MutatedJson(v);
            }
            tracing::debug!(
                event = "PreToolUse",
                "updated_input was not a JSON object; ignoring"
            );
        }
    }

    if let Some(ctx) = spec.additional_context {
        return ExtensionOutcome::InjectContext(ctx);
    }
    ExtensionOutcome::PassThrough
}

fn record(
    span: &tracing::Span,
    started: std::time::Instant,
    outcome: ExtensionOutcome,
) -> ExtensionOutcome {
    span.record("duration_ms", started.elapsed().as_millis() as u64);
    let label = match &outcome {
        ExtensionOutcome::Mutated(_) => "mutated",
        ExtensionOutcome::MutatedJson(_) => "mutated_json",
        ExtensionOutcome::InjectContext(_) => "inject_context",
        ExtensionOutcome::PassThrough => "pass_through",
        ExtensionOutcome::Blocked { .. } => "blocked",
        ExtensionOutcome::KeepLooping { .. } => "keep_looping",
        ExtensionOutcome::SoftFailed { .. } => "failed",
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
        let spec = ExtensionSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            matcher: None,
        };
        let out = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "original" },
            &ctx(),
        )
        .await;
        assert_eq!(out, ExtensionOutcome::Mutated("rewritten".to_string()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn exit_zero_empty_stdout_is_passthrough() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-passthrough");
        let script = write_script(&tmp, "noop.sh", "#!/bin/sh\ncat >/dev/null\n");
        let spec = ExtensionSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            matcher: None,
        };
        let out = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "original" },
            &ctx(),
        )
        .await;
        assert_eq!(out, ExtensionOutcome::PassThrough);
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
        let spec = ExtensionSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            matcher: None,
        };
        let out = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
        )
        .await;
        match out {
            ExtensionOutcome::Blocked { stderr, .. } => assert!(stderr.contains("nope")),
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
        let spec = ExtensionSpec {
            program: script,
            args: vec![],
            timeout_ms: 5_000,
            matcher: None,
        };
        let out = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
        )
        .await;
        match out {
            ExtensionOutcome::SoftFailed { reason } => assert!(reason.contains("invalid JSON")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn missing_binary_is_soft_failed() {
        let spec = ExtensionSpec {
            program: PathBuf::from("/nonexistent/binary/__ignis_no_such_path__"),
            args: vec![],
            timeout_ms: 1_000,
            matcher: None,
        };
        let out = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
        )
        .await;
        matches!(out, ExtensionOutcome::SoftFailed { .. });
    }

    #[tokio::test]
    async fn timeout_is_soft_failed() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-disp-timeout");
        let script = write_script(&tmp, "slow.sh", "#!/bin/sh\ncat >/dev/null\nsleep 5\n");
        let spec = ExtensionSpec {
            program: script,
            args: vec![],
            timeout_ms: 200,
            matcher: None,
        };
        let out = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
        )
        .await;
        match out {
            ExtensionOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
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
        let spec = ExtensionSpec {
            program: script,
            args: vec![],
            timeout_ms: 150,
            matcher: None,
        };
        let big_payload = "x".repeat(256 * 1024); // > 64 KiB pipe buf
        let t0 = std::time::Instant::now();
        let out = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit {
                prompt: &big_payload,
            },
            &ctx(),
        )
        .await;
        let elapsed = t0.elapsed();
        match out {
            ExtensionOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        // Must return well within the long sleep (with slack for spawn).
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "timeout did not fire promptly: elapsed = {elapsed:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // -------- classify_response unit tests (no subprocess) --------
    // These exercise the per-event JSON → outcome mapping in
    // isolation, so behavioural drift on a single field type is caught
    // without spawning Python.

    fn parse(raw: &str) -> ExtensionOutput {
        serde_json::from_str(raw).expect("test JSON")
    }

    #[test]
    fn pre_tool_use_object_updated_input_becomes_mutated_json() {
        let raw = r#"{
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": { "command": "echo safe" }
            }
        }"#;
        let outcome = classify_response(ExtensionEvent::PreToolUse, parse(raw), String::new());
        match outcome {
            ExtensionOutcome::MutatedJson(v) => {
                assert_eq!(v["command"], "echo safe");
            }
            other => panic!("expected MutatedJson, got {other:?}"),
        }
    }

    #[test]
    fn pre_tool_use_string_updated_input_falls_through_to_passthrough() {
        // PreToolUse rewrites are object-only. A hook that misuses the
        // field (writes a string) must not crash the loop — it falls
        // through to PassThrough so the original tool_input is used.
        let raw = r#"{
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": "not an object"
            }
        }"#;
        let outcome = classify_response(ExtensionEvent::PreToolUse, parse(raw), String::new());
        assert_eq!(outcome, ExtensionOutcome::PassThrough);
    }

    #[test]
    fn post_tool_use_additional_context_becomes_inject_context() {
        let raw = r#"{
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": "test suite failed"
            }
        }"#;
        let outcome = classify_response(ExtensionEvent::PostToolUse, parse(raw), String::new());
        assert_eq!(
            outcome,
            ExtensionOutcome::InjectContext("test suite failed".to_string())
        );
    }

    #[test]
    fn stop_decision_block_becomes_keep_looping_with_reason() {
        // Pins the "decision: block = keep looping" inversion for Stop —
        // a subtle semantic that, if it ever silently reverts, would
        // turn user stop-condition guardrails into hard turn-terminators.
        let raw = r#"{
            "decision": "block",
            "reason": "tests are still failing"
        }"#;
        let outcome = classify_response(ExtensionEvent::Stop, parse(raw), String::new());
        assert_eq!(
            outcome,
            ExtensionOutcome::KeepLooping {
                reason: "tests are still failing".to_string(),
            }
        );
    }

    #[test]
    fn stop_decision_block_without_reason_falls_back_to_stderr() {
        let outcome = classify_response(
            ExtensionEvent::Stop,
            parse(r#"{ "decision": "block" }"#),
            "stderr-only reason".to_string(),
        );
        assert_eq!(
            outcome,
            ExtensionOutcome::KeepLooping {
                reason: "stderr-only reason".to_string(),
            }
        );
    }

    #[test]
    fn pre_tool_use_decision_block_with_reason_is_structured_block() {
        // For non-Stop events, decision:block stays a Blocked outcome,
        // and the structured reason field is preferred over stderr.
        let raw = r#"{
            "decision": "block",
            "reason": "rm -rf is destructive"
        }"#;
        let outcome = classify_response(
            ExtensionEvent::PreToolUse,
            parse(raw),
            "fallback stderr".to_string(),
        );
        match outcome {
            ExtensionOutcome::Blocked { reason, stderr } => {
                assert_eq!(reason.as_deref(), Some("rm -rf is destructive"));
                assert_eq!(stderr, "fallback stderr");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn system_prompt_compose_rewrite_via_updated_system_prompt() {
        let raw = r#"{
            "hookSpecificOutput": {
                "hookEventName": "SystemPromptCompose",
                "updatedSystemPrompt": "rewritten system prompt"
            }
        }"#;
        let outcome = classify_response(
            ExtensionEvent::SystemPromptCompose,
            parse(raw),
            String::new(),
        );
        assert_eq!(
            outcome,
            ExtensionOutcome::Mutated("rewritten system prompt".to_string())
        );
    }

    #[test]
    fn rewrite_takes_precedence_over_additional_context() {
        // A hook that returns both fields gets its rewrite; the
        // additional_context is logged as ignored. This pins the
        // documented precedence so future drift is intentional.
        let raw = r#"{
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "updatedInput": "rewritten",
                "additionalContext": "ignored for now"
            }
        }"#;
        let outcome =
            classify_response(ExtensionEvent::UserPromptSubmit, parse(raw), String::new());
        assert_eq!(outcome, ExtensionOutcome::Mutated("rewritten".to_string()));
    }
}
