//! Spawn one extension subprocess, send the JSON envelope on stdin, read
//! stdout, triage the exit code. Designed so a runaway, hung, or crashing
//! extension can never kill the agent loop — every failure path returns an
//! `ExtensionOutcome` the caller can turn into "use the original value +
//! emit Warning".
//!
//! Process model:
//!   * env: `Command::env_clear()` + an explicit allowlist (`PATH HOME USER
//!     LANG LC_ALL TZ` always, plus whatever names the extension declared in
//!     `spec.env`). Closes the v1 credential-exfil gap.
//!   * sandbox: when `spec.sandbox` is true, a `pre_exec` closure installs
//!     the default filesystem ruleset (Linux Landlock / macOS Seatbelt)
//!     between fork and execve so the child can only read its own folder +
//!     lib paths + TLS roots and can only write `/tmp`. See [`super::sandbox`].
//!   * spawn via `tokio::process::Command` with piped stdin/stdout/stderr;
//!   * inside ONE `tokio::time::timeout`: write the envelope, drain stdout
//!     and stderr through a 1 MiB-per-stream cap, wait for the child;
//!   * on timeout, SIGTERM → 1 s grace → SIGKILL. `kill_on_drop` remains the
//!     safety net for panic paths.

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::config::ExtensionSpec;
use super::protocol::{ExtensionEvent, ExtensionInput, ExtensionOutput};
use super::sandbox as ext_sandbox;
use super::EventSender;
use crate::sandbox::{self, SandboxStatus};
use crate::AgentEvent;

/// Universal env-var allowlist. Every extension always sees these names from
/// ignis's own environment (when set). The per-extension `env: [...]` list
/// adds to this — it doesn't replace it.
///
/// These six are the minimum a normal interpreter (`python3`, `bash`,
/// `ruby`) needs to start: `PATH` to find the binary, `HOME` to find dot-
/// files, `USER`/`LANG`/`LC_ALL`/`TZ` so any locale-sensitive code behaves
/// as the user expects.
const UNIVERSAL_ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TZ"];

/// 1 MiB per stream. An extension that emits more than this is almost
/// certainly runaway; we truncate and surface a `Warning` event. Same cap on
/// stdout and stderr so log capture is bounded too.
const STREAM_BUFFER_CAP: usize = 1024 * 1024;

/// Extensions whose `SandboxStatus::NotEnforced` warning has already been
/// emitted once this session. Keyed by `display_name()`. Reset by
/// `/extensions reload` via [`reset_sandbox_warnings`] so editing an
/// extension re-arms the notice.
static SANDBOX_WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Clear the once-per-session "sandbox not enforced" suppression set. Called
/// from `ExtensionRegistry::reload` so a freshly-edited extension gets a
/// fresh notice instead of being silently swallowed.
pub fn reset_sandbox_warnings() {
    if let Ok(mut guard) = SANDBOX_WARNED.lock() {
        if let Some(set) = guard.as_mut() {
            set.clear();
        }
    }
}

/// Returns `true` the first time a given extension name's degradation warning
/// fires this session, `false` afterwards.
fn should_emit_sandbox_warning(name: &str) -> bool {
    let mut guard = match SANDBOX_WARNED.lock() {
        Ok(g) => g,
        Err(_) => return false, // poisoned — fail quiet rather than spam
    };
    let set = guard.get_or_insert_with(HashSet::new);
    set.insert(name.to_string())
}

/// Compute the sandbox status to report to telemetry from the parent. We do
/// NOT actually install the sandbox here — that happens in the child's
/// `pre_exec`. This is just for span attributes / dashboards and for tests
/// that want to assert what the child was subjected to.
///
/// On Linux, the kernel-level ABI is probed once per session via
/// [`crate::sandbox::is_kernel_sandbox_available`] and cached. macOS Seatbelt
/// ships unconditionally; we trust the documented ABI rather than probe.
pub fn sandbox_status_for_telemetry(spec: &ExtensionSpec) -> SandboxStatus {
    if !spec.sandbox {
        return SandboxStatus::Disabled;
    }
    if crate::sandbox::is_kernel_sandbox_available() {
        SandboxStatus::FullyEnforced
    } else if cfg!(target_os = "linux") {
        SandboxStatus::NotEnforced
    } else {
        SandboxStatus::PlatformUnsupported
    }
}

async fn emit_buffer_warning(tx: Option<&EventSender>, name: &str, stream: &str) {
    if let Some(tx) = tx {
        let _ = tx
            .send(AgentEvent::Warning {
                source: "extension.buffer".to_string(),
                message: format!("{name}: {stream} truncated at 1 MiB"),
            })
            .await;
    }
    tracing::warn!(
        extension = name,
        stream,
        "extension output truncated at 1 MiB cap"
    );
}

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

/// Run one extension and return its outcome plus the [`SandboxStatus`] the
/// child was subjected to. Never returns an `Err` — every failure mode maps
/// to [`ExtensionOutcome::SoftFailed`] (or [`ExtensionOutcome::Blocked`]).
///
/// The payload's variant (and not a separate `event` arg) decides the
/// envelope shape, the response-field this dispatch reads (`updated_input`
/// vs `updated_output` vs `updated_system_prompt` vs `additional_context`),
/// and whether `decision: "block"` short-circuits or is inverted to
/// [`ExtensionOutcome::KeepLooping`] (the `Stop` case).
///
/// `tx` is the live `AgentEvent::Warning` channel. When present, the
/// dispatcher emits a `Warning` for two conditions that don't change the
/// outcome: stdout/stderr truncated at `STREAM_BUFFER_CAP`, and the sandbox
/// being unavailable on this kernel/platform (at most once per extension
/// name per session).
pub async fn run_hook(
    spec: &ExtensionSpec,
    payload: ExtensionPayload<'_>,
    ctx: &DispatchContext<'_>,
    tx: Option<&EventSender>,
) -> (ExtensionOutcome, SandboxStatus) {
    let started = std::time::Instant::now();
    let event = payload.event();
    let cmd_name = spec.display_name();
    // Compute the sandbox status once at the top so every outcome we return
    // can carry the same value. This is a *telemetry* view of what the kernel
    // *will* do, not a guarantee it did — the actual install happens in the
    // child's `pre_exec` below.
    let sandbox_status = sandbox_status_for_telemetry(spec);
    let span = tracing::info_span!(
        "ignis.extension",
        event = event.as_str(),
        command = %cmd_name,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
        sandbox.status = sandbox_status.as_str(),
    );
    let _enter = span.enter();

    let envelope = payload.build_envelope(ctx);
    let stdin_bytes = match serde_json::to_vec(&envelope) {
        Ok(b) => b,
        Err(e) => {
            return (
                record(
                    &span,
                    started,
                    ExtensionOutcome::SoftFailed {
                        reason: format!("envelope encode failed: {e}"),
                    },
                ),
                sandbox_status,
            );
        }
    };

    // === Env-var allowlist (all platforms) =================================
    // env_clear() first, then re-add only the explicit allowlist plus any
    // extension-declared names. Closes the v1 credential-exfil gap.
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

    // === Filesystem sandbox (Linux Landlock / macOS Seatbelt) ==============
    // The closure runs in the forked child between `fork` and `execve` — the
    // only safe seam for the self-restrict semantics. On unsupported
    // platforms `apply` is a no-op stub returning PlatformUnsupported.
    //
    // Extension folder: the directory containing the program. `None` for a
    // bare binary name (e.g. `python3 hook.py` resolved by PATH) — the read
    // allowlist then omits the program directory rather than falling back to
    // `/` (which would silently disable read confinement).
    let ext_folder: Option<std::path::PathBuf> = spec.program.parent().map(|p| p.to_path_buf());
    let want_sandbox = spec.sandbox;
    // Set the child's CWD to the extension's own directory: (1) on macOS,
    // bash's `shell-init`/`job-working-directory` startup probes call
    // `getcwd()` (which under Seatbelt `stat()`s the path) — with the
    // parent's CWD inherited (the user's project root, not in the read
    // allowlist) every bash extension soft-fails with EPERM stderr noise;
    // setting CWD to the extension's folder (which IS readable) fixes it.
    // (2) predictable relative paths for sibling resolution. Falls back to
    // `/` (a readable directory) for a bare PATH-resolved program.
    let child_cwd: &std::path::Path = ext_folder
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("/"));
    cmd.current_dir(child_cwd);
    #[cfg(unix)]
    {
        if want_sandbox {
            // Build the policy in the PARENT (allocations live here). The
            // `pre_exec` closure is the child seam between fork and execve —
            // heap allocation there is unsafe. `SandboxPolicy::apply` is
            // designed to be allocation-free.
            let policy = sandbox::SandboxPolicy::new(
                &ext_sandbox::default_read_paths(ext_folder.as_deref()),
                &ext_sandbox::default_write_paths(),
            );
            // SAFETY: the closure runs in the forked child before execve. It
            // must be async-signal-safe — no allocation that can panic, no
            // global locks, no tracing. `policy.apply()` only performs
            // syscalls / Apple's `sandbox_init` and uses
            // `io::Error::from_raw_os_error` (no boxing) on the error path.
            unsafe {
                cmd.pre_exec(move || policy.apply().map(|_| ()));
            }
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                record(
                    &span,
                    started,
                    ExtensionOutcome::SoftFailed {
                        reason: format!("spawn failed: {e}"),
                    },
                ),
                sandbox_status,
            );
        }
    };

    // Warn (once per session) on extensions that ran without confinement.
    let unconfined_reason = match sandbox_status {
        SandboxStatus::NotEnforced => Some("kernel sandbox unavailable on this kernel"),
        SandboxStatus::PlatformUnsupported => Some("sandboxing unavailable on this platform"),
        _ => None,
    };
    if let Some(reason) = unconfined_reason {
        if should_emit_sandbox_warning(&cmd_name) {
            if let Some(tx) = tx {
                let _ = tx
                    .send(AgentEvent::Warning {
                        source: "extension.sandbox".to_string(),
                        message: format!("{cmd_name}: {reason}; extension runs unconfined"),
                    })
                    .await;
            }
            tracing::warn!(extension = %cmd_name, reason = %reason, "extension unconfined");
        }
    }

    // === Bounded stdout/stderr =============================================
    // Join the wait future with two capped drain futures; the stdin write
    // runs as an independent task so it can't block the join. After the cap
    // we keep draining (discarding) so the child can finish writing instead
    // of blocking on a full pipe.
    let timeout = Duration::from_millis(spec.timeout_ms);
    let stdin_handle = child.stdin.take();
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let stdin_task = tokio::spawn(async move {
        if let Some(mut s) = stdin_handle {
            if let Err(e) = s.write_all(&stdin_bytes).await {
                tracing::debug!(error = %e, "extension stdin write failed (child may have exited)");
            }
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
            return (
                record(
                    &span,
                    started,
                    ExtensionOutcome::SoftFailed {
                        reason: format!("wait failed: {e}"),
                    },
                ),
                sandbox_status,
            );
        }
        Err(_) => {
            // === SIGTERM grace ============================================
            // `child` is still owned here (the `interaction` future only
            // borrowed it via `child.wait()`). Send SIGTERM, wait up to 1 s
            // for clean exit, then SIGKILL. tokio's `start_kill` maps to
            // SIGKILL, so we deliver SIGTERM via `libc::kill` directly.
            #[cfg(unix)]
            {
                if let Some(pid) = child.id() {
                    // SAFETY: libc::kill is async-signal-safe; `pid` came
                    // from a Child we still own (no reuse race).
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }
            let grace = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            if grace.is_err() {
                let _ = child.kill().await;
            } else {
                tracing::debug!("extension exited within SIGTERM grace window");
            }
            stdin_task.abort();
            return (
                record(
                    &span,
                    started,
                    ExtensionOutcome::SoftFailed {
                        reason: format!("timed out after {}ms", spec.timeout_ms),
                    },
                ),
                sandbox_status,
            );
        }
    };

    // Reap the stdin writer — almost certainly finished, but ignoring the
    // JoinHandle would leave it half-detached.
    let _ = stdin_task.await;

    if stdout_bytes.len() >= STREAM_BUFFER_CAP {
        emit_buffer_warning(tx, &cmd_name, "stdout").await;
    }
    if stderr_bytes.len() >= STREAM_BUFFER_CAP {
        emit_buffer_warning(tx, &cmd_name, "stderr").await;
    }
    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

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
        return (record(&span, started, outcome), sandbox_status);
    }
    if !status.success() {
        let reason = match status.code() {
            Some(code) => format!("exit {code}: {}", trim_stderr(&stderr)),
            None => format!("terminated by signal: {}", trim_stderr(&stderr)),
        };
        return (
            record(&span, started, ExtensionOutcome::SoftFailed { reason }),
            sandbox_status,
        );
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return (
            record(&span, started, ExtensionOutcome::PassThrough),
            sandbox_status,
        );
    }
    let parsed: ExtensionOutput = match serde_json::from_str(trimmed) {
        Ok(p) => p,
        Err(e) => {
            return (
                record(
                    &span,
                    started,
                    ExtensionOutcome::SoftFailed {
                        reason: format!("invalid JSON on stdout: {e}"),
                    },
                ),
                sandbox_status,
            );
        }
    };

    (
        record(&span, started, classify_response(event, parsed, stderr)),
        sandbox_status,
    )
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
            timeout_ms: 5_000,
            ..ExtensionSpec::default()
        };
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "original" },
            &ctx(),
            None,
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
            timeout_ms: 5_000,
            ..ExtensionSpec::default()
        };
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "original" },
            &ctx(),
            None,
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
            timeout_ms: 5_000,
            ..ExtensionSpec::default()
        };
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            None,
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
            timeout_ms: 5_000,
            ..ExtensionSpec::default()
        };
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            None,
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
            timeout_ms: 1_000,
            ..ExtensionSpec::default()
        };
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            None,
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
            timeout_ms: 200,
            ..ExtensionSpec::default()
        };
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            None,
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
            timeout_ms: 150,
            ..ExtensionSpec::default()
        };
        let big_payload = "x".repeat(256 * 1024); // > 64 KiB pipe buf
        let t0 = std::time::Instant::now();
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit {
                prompt: &big_payload,
            },
            &ctx(),
            None,
        )
        .await;
        let elapsed = t0.elapsed();
        match out {
            ExtensionOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        // Must return well within the long sleep (with slack for spawn).
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout did not fire promptly: elapsed = {elapsed:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    // -------- sandbox / env / SIGTERM-grace / buffer-cap layer tests --------

    /// Helper: an extension script that emits every name=value pair from its
    /// env to stdout, wrapped in the protocol JSON shape. Lets us assert the
    /// allowlist actually filters.
    fn env_dump_script(dir: &std::path::Path) -> PathBuf {
        write_script(
            dir,
            "env-dump.sh",
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
out=$(printf '%s' "$out" | tr '"' "'")
printf '%s' "{\"hookSpecificOutput\":{\"updatedInput\":\"$out\"}}"
"#,
        )
    }

    #[tokio::test]
    async fn env_filter_drops_secrets_unless_declared() {
        // With `env: []` the child sees PATH/HOME but NOT a secret env var
        // the parent had set; with `env: ["KEY"]` the child sees the secret.
        let tmp = crate::util::unique_temp_dir("ignis-hook-envfilter");
        let script = env_dump_script(&tmp);

        std::env::set_var("IGNIS_HOOK_TEST_SECRET", "leaked-credential-XYZ");

        // 1. No declaration → child must NOT see the secret.
        let spec_no_env = ExtensionSpec {
            program: script.clone(),
            timeout_ms: 5_000,
            sandbox: false,
            ..ExtensionSpec::default()
        };
        let (out, _sb) = run_hook(
            &spec_no_env,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            None,
        )
        .await;
        let body = match out {
            ExtensionOutcome::Mutated(updated) => updated,
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
        let spec_with_env = ExtensionSpec {
            program: script,
            timeout_ms: 5_000,
            env: vec!["IGNIS_HOOK_TEST_SECRET".to_string()],
            sandbox: false,
            ..ExtensionSpec::default()
        };
        let (out2, _sb2) = run_hook(
            &spec_with_env,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            None,
        )
        .await;
        let body2 = match out2 {
            ExtensionOutcome::Mutated(updated) => updated,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(
            body2.contains("IGNIS_HOOK_TEST_SECRET=leaked-credential-XYZ"),
            "explicit env declaration did not pass secret through: {body2}"
        );

        std::env::remove_var("IGNIS_HOOK_TEST_SECRET");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// SIGTERM grace: an extension that ignores SIGTERM and sleeps 30 s should
    /// be SIGKILL'd ~1 s after the configured timeout. **Linux only** — macOS
    /// resets SIGTERM to `SIG_DFL` on `exec` for children without a
    /// controlling terminal, so a child's `SIG_IGN` is overridden. The
    /// cooperative test below covers the primary grace-window use on all Unix.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sigterm_grace_kills_uncooperative_hook_after_one_second() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-sigterm");
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
        let script = write_script(&tmp, "ignore-term.py", std::str::from_utf8(body).unwrap());
        let spec = ExtensionSpec {
            program: script,
            timeout_ms: 100,
            ..ExtensionSpec::default()
        };
        let t0 = std::time::Instant::now();
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            None,
        )
        .await;
        let elapsed = t0.elapsed();

        match out {
            ExtensionOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
            other => panic!("expected SoftFailed, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_millis(1050),
            "did not honour grace window: elapsed = {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "SIGKILL did not land promptly: elapsed = {elapsed:?}"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// SIGTERM grace — cooperative hook: a SIGTERM handler that exits cleanly
    /// should exit before the 1 s grace elapses. Runs on all Unix targets.
    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_grace_with_cooperative_hook_exits_promptly() {
        // Skipped on macOS: the system / Homebrew Python stdlib is NOT in the
        // Seatbelt read allowlist, so a python hook can't start under the
        // default sandbox. macOS kernel confinement is covered by
        // `hook_sandbox.rs`.
        #[cfg(target_os = "macos")]
        {
            eprintln!(
                "macOS Python not in Seatbelt read allowlist; skipping cooperative SIGTERM test"
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            let tmp = crate::util::unique_temp_dir("ignis-hook-sigterm-coop");
            let body = b"\
#!/usr/bin/env python3
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
            let script = write_script(&tmp, "cooperative.py", std::str::from_utf8(body).unwrap());
            let spec = ExtensionSpec {
                program: script,
                timeout_ms: 100,
                ..ExtensionSpec::default()
            };
            let t0 = std::time::Instant::now();
            let (out, _sb) = run_hook(
                &spec,
                ExtensionPayload::UserPromptSubmit { prompt: "x" },
                &ctx(),
                None,
            )
            .await;
            let elapsed = t0.elapsed();

            match out {
                ExtensionOutcome::SoftFailed { reason } => assert!(reason.contains("timed out")),
                other => panic!("expected SoftFailed, got {other:?}"),
            }
            assert!(
                elapsed >= Duration::from_millis(100),
                "outer timeout did not fire: elapsed = {elapsed:?}"
            );
            assert!(
                elapsed < Duration::from_millis(1500),
                "grace window was not honoured on cooperative exit: elapsed = {elapsed:?}"
            );

            std::fs::remove_dir_all(&tmp).ok();
        }
    }

    /// Buffer cap: an extension that emits 2 MiB on stdout should be truncated
    /// at 1 MiB and a `extension.buffer` Warning event should land on tx.
    #[tokio::test]
    async fn stdout_truncated_at_one_mib_and_warning_emitted() {
        let tmp = crate::util::unique_temp_dir("ignis-hook-bufcap");
        let script = write_script(
            &tmp,
            "spew.sh",
            "#!/bin/sh\ncat >/dev/null\nhead -c 2097152 /dev/zero | tr '\\0' x\n",
        );
        let spec = ExtensionSpec {
            program: script,
            timeout_ms: 10_000,
            ..ExtensionSpec::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (out, _sb) = run_hook(
            &spec,
            ExtensionPayload::UserPromptSubmit { prompt: "x" },
            &ctx(),
            Some(&tx),
        )
        .await;
        // 2 MiB of 'x' is not valid JSON; outcome is SoftFailed. The point of
        // the test is the warning + cap, not the parse.
        matches!(out, ExtensionOutcome::SoftFailed { .. });
        drop(tx);

        let mut got_buffer_warning = false;
        let mut buffer_msg = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Warning { source, message } = ev {
                if source == "extension.buffer" {
                    got_buffer_warning = true;
                    buffer_msg = message;
                }
            }
        }
        assert!(
            got_buffer_warning,
            "expected an extension.buffer Warning, none arrived"
        );
        assert!(
            buffer_msg.contains("stdout truncated at 1 MiB"),
            "unexpected warning text: {buffer_msg}"
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
