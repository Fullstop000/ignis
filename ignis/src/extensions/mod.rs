//! External subprocess hook protocol.
//!
//! ignis spawns one subprocess per declared hook per event. The hook reads
//! a JSON envelope from stdin and writes a JSON response on stdout; we
//! optionally use its `updatedInput` / `updatedOutput` to rewrite the
//! text. Two events ship in v1: `UserPromptSubmit` (mutates the prompt
//! before history.push) and `AssistantMessageRender` (mutates the
//! assistant's text before TUI render).
//!
//! Hooks **never** kill a turn: every failure mode (timeout, crash,
//! malformed JSON, missing binary) degrades to "use the original value +
//! emit a Warning event to the UI".
//!
//! **Security:** v1 ships with no sandbox. Hooks run with ignis's full
//! privileges. See `docs/usage/hooks.md` for the threat model.

pub mod config;
pub mod dispatch;
pub mod protocol;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::{Mutex, RwLock};

use crate::tools::tool::{ToolExtensions, ToolResult};
use crate::AgentEvent;

pub use config::{ExtensionSpec, ExtensionsConfig, DEFAULT_TIMEOUT_MS};
pub use dispatch::{DispatchContext, ExtensionOutcome};
pub use protocol::{ExtensionEvent, ExtensionInput, ExtensionOutput, ExtensionSpecificOutput};

/// Context the registry needs at every dispatch call. Borrowed strings so
/// callers don't have to allocate per turn.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionContext<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
}

/// Sender for `AgentEvent::Warning` lines. The registry owns no channel of
/// its own — every dispatch path takes the channel the caller already has.
pub type EventSender = mpsc::Sender<AgentEvent>;

/// The registered hook chains, loaded once at session start and swappable
/// via `/hooks reload`. The wrapper holds an `Arc<RwLock<…>>` so the swap
/// is cheap and reload doesn't tear down outstanding references.
///
/// `session` stores the per-session envelope context (`session_id`, `cwd`)
/// that subprocess hooks see in their JSON envelope. It is set once by
/// `Session::open` after the session id is known. Before it's set, hooks
/// fire with empty `session_id` and `cwd` of `/` — harmless but
/// undescriptive; the warning logged at first such call points the
/// operator at the missing wire-up. The lock is independent of `inner`
/// so a `/hooks reload` (which writes `inner`) cannot stall a tool call
/// that reads `session`.
#[derive(Debug, Default, Clone)]
pub struct ExtensionRegistry {
    inner: Arc<RwLock<ExtensionsConfig>>,
    session: Arc<RwLock<SessionEnvelopeContext>>,
    /// FIFO queue of `additionalContext` strings emitted by hooks that
    /// returned [`ExtensionOutcome::InjectContext`]. The agent loop drains
    /// this between tool batches and prepends each entry as a
    /// `<system-reminder>` block before the next LLM call. Queue is
    /// shared across the session (same posture as the config) so a
    /// hook firing inside one tool batch can deliver context to the
    /// next.
    pending_injections: Arc<Mutex<Vec<PendingInjection>>>,
}

/// One queued context injection. The `source` is the hook's display
/// name so the system reminder rendered to the model carries enough
/// provenance for a user reading the transcript to know which hook
/// fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInjection {
    /// The `additional_context` text the hook returned.
    pub text: String,
    /// `display_name()` of the hook that produced it (the file stem).
    pub source: String,
    /// The event class that produced it (e.g. `PostToolUse`). Lets the
    /// renderer label the reminder ("hook PostToolUse: ...").
    pub event: ExtensionEvent,
}

/// Per-session envelope context — what subprocess hooks see in their
/// JSON envelope. Mutated once at session start, read once per hook
/// dispatch. Owned `String`/`PathBuf` so the lifetime is independent of
/// any caller stack frame.
#[derive(Debug, Clone, Default)]
struct SessionEnvelopeContext {
    session_id: String,
    cwd: PathBuf,
}

/// Internal carrier used by `run_inject_only_event` so the per-event
/// builders can hand back both the payload (lifetime-bound to the
/// event-specific args) and an owned `display_name()` label for the
/// `PendingInjection` provenance. Closures can't easily return two
/// values with different lifetimes, so we bundle them here.
struct PayloadWithLabel<'a> {
    payload: dispatch::ExtensionPayload<'a>,
    label: String,
}

impl<'a> dispatch::ExtensionPayload<'a> {
    fn with_spec_label(self, label: String) -> PayloadWithLabel<'a> {
        PayloadWithLabel {
            payload: self,
            label,
        }
    }
}

impl ExtensionRegistry {
    /// Load `~/.ignis/hooks.json` into a fresh registry.
    pub fn from_config_dir(home: &Path) -> anyhow::Result<Self> {
        let cfg = ExtensionsConfig::from_home(home)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(cfg)),
            session: Arc::new(RwLock::new(SessionEnvelopeContext::default())),
            pending_injections: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Empty registry — useful in tests and when no home dir is available.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct directly from a parsed config (test helper, used by the
    /// integration test in `tests/hook_roundtrip.rs`).
    pub fn from_config(cfg: ExtensionsConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(cfg)),
            session: Arc::new(RwLock::new(SessionEnvelopeContext::default())),
            pending_injections: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Take everything queued by hooks that returned
    /// `additional_context`. Called by the agent loop after each tool
    /// batch to flush pending injections into the history as
    /// `<system-reminder>` blocks before the next LLM call. FIFO order
    /// is preserved so a `PostToolUse` hook in one batch reaches the
    /// next LLM call in the order it fired.
    pub async fn drain_injections(&self) -> Vec<PendingInjection> {
        let mut g = self.pending_injections.lock().await;
        std::mem::take(&mut *g)
    }

    /// Number of injections currently queued. Read-only — for tests
    /// and the `/hooks` listing's drift detection.
    pub async fn pending_injection_count(&self) -> usize {
        self.pending_injections.lock().await.len()
    }

    /// Internal: queue an injection. Used by the `ToolExtensions` impl when
    /// a `PostToolUse` (or, in commit 5, any inject-context event)
    /// returns `additional_context`.
    async fn queue_injection(&self, inj: PendingInjection) {
        self.pending_injections.lock().await.push(inj);
    }

    /// Set the session-level envelope context. Called once by
    /// `Session::open` (and any place that re-binds a registry to a
    /// new session) after the session id and cwd are known.
    pub async fn set_envelope_context(&self, session_id: String, cwd: PathBuf) {
        let mut guard = self.session.write().await;
        guard.session_id = session_id;
        guard.cwd = cwd;
    }

    /// Read the current envelope context as owned strings. Tool-hook
    /// dispatch paths use this to build the `DispatchContext` once per
    /// chain, holding the lock for the read only.
    async fn envelope_context(&self) -> (String, String) {
        let guard = self.session.read().await;
        (
            guard.session_id.clone(),
            guard.cwd.to_string_lossy().to_string(),
        )
    }

    /// Rebuild the registry from disk in place. Returns the new hook count
    /// for the `/hooks reload` confirmation line.
    pub async fn reload(&self, home: &Path) -> anyhow::Result<usize> {
        let cfg = ExtensionsConfig::from_home(home)?;
        let total = cfg.total_len();
        let mut guard = self.inner.write().await;
        *guard = cfg;
        Ok(total)
    }

    /// Run the `UserPromptSubmit` chain. Returns:
    ///
    /// - [`PromptExtensionResult::Continue`] with the (possibly rewritten) prompt
    ///   when every hook passed through or successfully rewrote, or when a
    ///   soft-failure short-circuited the chain at the last good value
    ///   (caller pushes the string to history and runs the agent).
    /// - [`PromptExtensionResult::Blocked`] when a hook returned exit 2 or
    ///   `continue: false`. The spec's iron rule: hooks cannot kill a
    ///   turn EXCEPT here — `UserPromptSubmit` is the one event where
    ///   blocking is meaningful. The caller MUST NOT push the prompt to
    ///   history and MUST NOT call the agent; the warning has already
    ///   been emitted to `tx`, so the user sees the block reason.
    pub async fn run_user_prompt_submit(
        &self,
        prompt: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) -> PromptExtensionResult {
        self.run_prompt_chain(prompt, ctx, tx).await
    }

    /// Run the `SystemPromptCompose` chain — fires once per LLM call,
    /// before serialization. Hooks may:
    /// * rewrite the prompt (`updatedSystemPrompt`) — threaded through
    ///   the remaining hooks and used for THIS call only;
    /// * inject `additionalContext` — queued for the same flush path
    ///   `PostToolUse` uses;
    /// * soft-fail (the base prompt is preserved, a `[warn]` is
    ///   emitted on `tx`).
    ///
    /// Returns the prompt to send to the provider. Empty hook chain →
    /// the base prompt unchanged, no allocation.
    pub async fn run_system_prompt_compose(
        &self,
        base: &str,
        model: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) -> String {
        let event = ExtensionEvent::SystemPromptCompose;
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return base.to_string();
        }
        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };
        let mut current = base.to_string();
        for spec in &specs {
            let payload = dispatch::ExtensionPayload::SystemPromptCompose {
                system_prompt: &current,
                model,
            };
            let outcome = dispatch::run_hook(spec, payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::Mutated(text) => current = text,
                ExtensionOutcome::InjectContext(text) => {
                    self.queue_injection(PendingInjection {
                        text,
                        source: spec.display_name(),
                        event,
                    })
                    .await;
                }
                ExtensionOutcome::Blocked { stderr, reason } => {
                    // SystemPromptCompose has no meaningful "block" —
                    // the call still needs SOME prompt. Degrade to a
                    // soft failure with the reason surfaced as a
                    // warning so the user sees the misconfiguration.
                    let why = reason.unwrap_or_else(|| trim_one_line(&stderr));
                    emit_warning(
                        tx,
                        event,
                        &format!(
                            "{} returned `decision: \"block\"` (ignored for SystemPromptCompose): {}",
                            spec.display_name(),
                            trim_one_line(&why)
                        ),
                    )
                    .await;
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
                ExtensionOutcome::MutatedJson(_) | ExtensionOutcome::KeepLooping { .. } => {
                    tracing::debug!(
                        hook = %spec.display_name(),
                        "SystemPromptCompose hook returned outcome that does not apply; ignoring"
                    );
                }
            }
        }
        current
    }

    /// Run the `SessionStart` chain — fires once when a session opens
    /// (whether new or resumed). Source carries `"new"` / `"resume"` /
    /// `"subagent"`. The only meaningful outcome is `additionalContext`
    /// (queued for the next-LLM-call drain) or pass-through; rewrite
    /// variants and `decision: "block"` are logged at debug and
    /// otherwise ignored — a session has to start.
    pub async fn run_session_start(
        &self,
        source: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) {
        self.run_inject_only_event(ExtensionEvent::SessionStart, ctx, tx, |spec| {
            dispatch::ExtensionPayload::SessionStart { source }.with_spec_label(spec.display_name())
        })
        .await;
    }

    /// Run the `PreCompact` chain — fires before context compaction.
    /// `trigger` is `"auto"` or `"manual"`. A hook returning
    /// `decision: "block"` aborts the compact; the returned bool is
    /// `true` when the caller MUST skip compaction. `additionalContext`
    /// is queued for the next-LLM-call drain.
    pub async fn run_pre_compact(
        &self,
        trigger: &str,
        transcript_path: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) -> bool {
        let event = ExtensionEvent::PreCompact;
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return false;
        }
        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };
        let mut abort = false;
        for spec in &specs {
            let payload = dispatch::ExtensionPayload::PreCompact {
                trigger,
                transcript_path,
            };
            let outcome = dispatch::run_hook(spec, payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::InjectContext(text) => {
                    self.queue_injection(PendingInjection {
                        text,
                        source: spec.display_name(),
                        event,
                    })
                    .await;
                }
                ExtensionOutcome::Blocked { stderr, reason } => {
                    let why = reason.unwrap_or_else(|| trim_one_line(&stderr));
                    emit_warning(
                        tx,
                        event,
                        &format!(
                            "aborted by {}: {}",
                            spec.display_name(),
                            trim_one_line(&why)
                        ),
                    )
                    .await;
                    abort = true;
                    break;
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
                other => {
                    tracing::debug!(
                        hook = %spec.display_name(),
                        outcome = ?other,
                        "PreCompact hook returned outcome that does not apply; ignoring"
                    );
                }
            }
        }
        abort
    }

    /// Run the `PostCompact` chain — fires after compaction succeeds.
    /// Sees the summary text; can inject `additionalContext` for the
    /// next LLM call. Block / rewrite outcomes don't apply (the
    /// summary is already final).
    pub async fn run_post_compact(
        &self,
        trigger: &str,
        summary: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) {
        self.run_inject_only_event(ExtensionEvent::PostCompact, ctx, tx, |spec| {
            dispatch::ExtensionPayload::PostCompact { trigger, summary }
                .with_spec_label(spec.display_name())
        })
        .await;
    }

    /// Run the `Stop` chain — fires on the clean-exit branch of the
    /// agent loop (NOT on `emit_fatal`). The CC inversion applies:
    /// `decision: "block"` becomes [`ExtensionOutcome::KeepLooping`] inside
    /// dispatch, surfaced here as a queued `<system-reminder>` framing
    /// the loop-keep-alive reason. The caller decides whether to honour
    /// the keep-loop by reading the returned `bool` (`true` =
    /// continue the loop).
    pub async fn run_stop(
        &self,
        transcript_path: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) -> bool {
        let event = ExtensionEvent::Stop;
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return false;
        }
        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };
        let mut keep_looping = false;
        for spec in &specs {
            let payload = dispatch::ExtensionPayload::Stop { transcript_path };
            let outcome = dispatch::run_hook(spec, payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::InjectContext(text) => {
                    self.queue_injection(PendingInjection {
                        text,
                        source: spec.display_name(),
                        event,
                    })
                    .await;
                }
                ExtensionOutcome::KeepLooping { reason } => {
                    // CC inversion: surface the reason as a queued
                    // system reminder framing the keep-alive, and tell
                    // the caller to continue the loop.
                    self.queue_injection(PendingInjection {
                        text: format!("stopped continuation: {reason}"),
                        source: spec.display_name(),
                        event,
                    })
                    .await;
                    keep_looping = true;
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
                ExtensionOutcome::Blocked { stderr, reason } => {
                    // Non-inverted block on Stop shouldn't happen
                    // (dispatch normalises `decision: "block"` to
                    // KeepLooping for Stop); the exit-2 path is the
                    // exception and is also inverted to KeepLooping
                    // there. If we somehow see a Blocked, treat it as
                    // KeepLooping for safety + log a debug note.
                    let why = reason.unwrap_or_else(|| trim_one_line(&stderr));
                    tracing::debug!(
                        hook = %spec.display_name(),
                        "Stop hook returned Blocked outside the dispatcher's inversion path; treating as KeepLooping"
                    );
                    self.queue_injection(PendingInjection {
                        text: format!("stopped continuation: {why}"),
                        source: spec.display_name(),
                        event,
                    })
                    .await;
                    keep_looping = true;
                }
                // Rewrite variants don't apply to Stop.
                ExtensionOutcome::Mutated(_) | ExtensionOutcome::MutatedJson(_) => {
                    tracing::debug!(
                        hook = %spec.display_name(),
                        "Stop hook returned a rewrite outcome that does not apply; ignoring"
                    );
                }
            }
        }
        keep_looping
    }

    /// Shared scaffolding for events where the only meaningful outcome
    /// is `additional_context` → queued. Called by `run_session_start`
    /// today and (once compaction wiring lands) by `PreCompact` /
    /// `PostCompact`. Centralises the warn-on-block / drop-rewrites
    /// posture so the per-event functions stay short.
    async fn run_inject_only_event<'a, F>(
        &self,
        event: ExtensionEvent,
        ctx: ExtensionContext<'a>,
        tx: &EventSender,
        mut build_payload: F,
    ) where
        F: for<'b> FnMut(&'b ExtensionSpec) -> PayloadWithLabel<'a>,
    {
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return;
        }
        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };
        for spec in &specs {
            let pwl = build_payload(spec);
            let outcome = dispatch::run_hook(spec, pwl.payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::InjectContext(text) => {
                    self.queue_injection(PendingInjection {
                        text,
                        source: pwl.label.clone(),
                        event,
                    })
                    .await;
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, pwl.label)).await;
                    break;
                }
                ExtensionOutcome::Blocked { .. } => {
                    emit_warning(
                        tx,
                        event,
                        &format!("{} returned `decision: \"block\"` (ignored)", pwl.label),
                    )
                    .await;
                }
                other => {
                    tracing::debug!(
                        hook = %pwl.label,
                        event = event.as_str(),
                        outcome = ?other,
                        "hook returned outcome that does not apply; ignoring"
                    );
                }
            }
        }
    }

    /// Run the `AssistantMessageRender` chain. Same semantics as
    /// `run_user_prompt_submit` but for `updatedOutput`. Hooks blocking
    /// (`exit 2` or `continue: false`) are degraded to a soft failure —
    /// the assistant message already exists; we can't lose it.
    pub async fn run_assistant_message_render(
        &self,
        content: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) -> String {
        self.run_render_chain(content, ctx, tx).await
    }

    /// Are any hooks declared for `event`? Fast path the render seam uses
    /// to skip task-spawn when there's nothing to do.
    pub async fn has_hooks(&self, event: ExtensionEvent) -> bool {
        !self.inner.read().await.for_event(event).is_empty()
    }

    /// Cheap clone of the in-memory `ExtensionsConfig`. Used by `/hooks` to
    /// render the currently-registered chains without re-reading disk.
    /// The list reflects the last `reload()` (or initial load) — it is
    /// not a live probe of the filesystem.
    pub async fn snapshot(&self) -> ExtensionsConfig {
        self.inner.read().await.clone()
    }

    /// Format the current chains into a single multi-line notice. Holds
    /// the read lock only across the sync `format_list` call — no
    /// `.await` is nested, so the lock is dropped before this returns.
    /// Prefer this over `snapshot() + format_list(&cfg)` to avoid cloning
    /// two `Vec<ExtensionSpec>` per call.
    pub async fn format_list(&self) -> String {
        let cfg = self.inner.read().await;
        format_list(&cfg)
    }

    /// `UserPromptSubmit` chain — same body as the render chain except
    /// that `Blocked` is honoured (returns `PromptExtensionResult::Blocked`)
    /// instead of being degraded to a Warning. Kept separate so the
    /// caller's return type carries the block decision at the type level.
    async fn run_prompt_chain(
        &self,
        initial: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) -> PromptExtensionResult {
        let event = ExtensionEvent::UserPromptSubmit;
        // Clone the small Vec of ExtensionSpec out of the RwLock so we don't
        // hold the read guard across `.await` points in dispatch.
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return PromptExtensionResult::Continue(initial.to_string());
        }

        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };

        let mut current = initial.to_string();
        for spec in &specs {
            let payload = dispatch::ExtensionPayload::UserPromptSubmit { prompt: &current };
            let outcome = dispatch::run_hook(spec, payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::Mutated(next) => current = next,
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::Blocked { stderr, reason } => {
                    // Honour the block. Spec: "Hook exits 2 → Block the
                    // turn (only event where blocking makes sense —
                    // UserPromptSubmit). Show stderr to user." Emit the
                    // warning here so the caller doesn't have to.
                    let display = reason.unwrap_or_else(|| trim_one_line(&stderr));
                    let trimmed = trim_one_line(&display);
                    emit_warning(
                        tx,
                        event,
                        &format!("blocked by {}: {}", spec.display_name(), trimmed),
                    )
                    .await;
                    return PromptExtensionResult::Blocked { stderr: trimmed };
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
                // Variants that don't apply to UserPromptSubmit's text-rewrite
                // semantics yet — InjectContext is wired in commit 4 via the
                // additional-context system-reminder path; MutatedJson is
                // PreToolUse-only; KeepLooping is Stop-only.
                ExtensionOutcome::InjectContext(_)
                | ExtensionOutcome::MutatedJson(_)
                | ExtensionOutcome::KeepLooping { .. } => {}
            }
        }
        PromptExtensionResult::Continue(current)
    }

    /// `AssistantMessageRender` chain — `Blocked` is degraded to a
    /// Warning because the assistant message already exists; the spec
    /// explicitly forbids losing it.
    async fn run_render_chain(
        &self,
        initial: &str,
        ctx: ExtensionContext<'_>,
        tx: &EventSender,
    ) -> String {
        let event = ExtensionEvent::AssistantMessageRender;
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return initial.to_string();
        }

        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };

        let mut current = initial.to_string();
        for spec in &specs {
            let payload = dispatch::ExtensionPayload::AssistantMessageRender { content: &current };
            let outcome = dispatch::run_hook(spec, payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::Mutated(next) => current = next,
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::Blocked { stderr, reason } => {
                    let display = reason.unwrap_or_else(|| trim_one_line(&stderr));
                    emit_warning(
                        tx,
                        event,
                        &format!(
                            "{} requested block (ignored for render): {}",
                            spec.display_name(),
                            trim_one_line(&display)
                        ),
                    )
                    .await;
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
                // Same as UserPromptSubmit — these v2 variants don't apply
                // to AssistantMessageRender's text-rewrite path.
                ExtensionOutcome::InjectContext(_)
                | ExtensionOutcome::MutatedJson(_)
                | ExtensionOutcome::KeepLooping { .. } => {}
            }
        }
        current
    }
}

/// Outcome of running the `UserPromptSubmit` chain. Drives the caller's
/// decision in `Session::prompt` — `Continue` proceeds to history.push
/// and the model call; `Blocked` short-circuits the turn entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptExtensionResult {
    /// All hooks passed/rewrote, or the chain soft-failed; carry on with
    /// the (possibly-rewritten) prompt.
    Continue(String),
    /// A hook explicitly blocked (`exit 2` or `continue: false`). The
    /// warning has already been emitted to the event channel. The caller
    /// MUST NOT push the prompt to history or call the agent.
    Blocked { stderr: String },
}
async fn emit_warning(tx: &EventSender, event: ExtensionEvent, message: &str) {
    let _ = tx
        .send(AgentEvent::Warning {
            source: event.as_str().to_string(),
            message: message.to_string(),
        })
        .await;
}

fn trim_one_line(s: &str) -> String {
    let one = s.replace('\n', " ").trim().to_string();
    truncate_chars(&one, 200)
}

/// Render the in-memory `ExtensionsConfig` as a multi-line notice suitable for
/// `add_assistant_notice`. Always returns at least one line; the empty
/// case is rendered as a single `[info]` line so the user gets feedback
/// rather than a silent no-op. The list reflects the last `reload()` —
/// it is not a live probe of `~/.ignis/hooks.json`.
pub(crate) fn format_list(cfg: &ExtensionsConfig) -> String {
    let total = cfg.total_len();
    if total == 0 {
        return "[info] no extensions registered · edit ~/.ignis/extensions.json, then /extensions reload"
            .to_string();
    }
    // Compute the widest display name across every chain so the program
    // column starts at a fixed offset regardless of how long any single
    // hook's filename is. `display_name()` is the file_stem — long
    // project-prefixed names like `translate-to-french-v2` would
    // otherwise push the program column right and break alignment for
    // shorter names in the same chain.
    let name_width = ExtensionEvent::ALL
        .iter()
        .flat_map(|ev| {
            cfg.for_event(*ev)
                .iter()
                .map(|s| s.display_name().chars().count())
        })
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!(
        "[info] {total} extension{total_plural} registered · /extensions reload to re-read · run unsandboxed; audit before installing:",
        total_plural = if total == 1 { "" } else { "s" },
    ));
    for event in ExtensionEvent::ALL {
        let chain = cfg.for_event(*event);
        if chain.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n  {event_name} ({n}):",
            event_name = event.as_str(),
            n = chain.len(),
        ));
        for spec in chain {
            let argv = if spec.args.is_empty() {
                String::new()
            } else {
                format!(" {}", spec.args.join(" "))
            };
            out.push_str(&format!(
                "\n    \u{00b7} {name:<width$}  {prog}{argv}  (timeout {ms}ms)",
                name = spec.display_name(),
                width = name_width,
                prog = spec.program.display(),
                ms = spec.timeout_ms,
            ));
        }
    }
    out
}

/// Char-boundary-safe truncation. Returns at most `n` chars from `s`,
/// appending `…` when the original was longer. Slicing `&s[..200]` on a
/// CJK / multibyte string panics if 200 lands inside a code point; warning
/// paths must never panic.
pub(crate) fn truncate_chars(s: &str, n: usize) -> String {
    let mut iter = s.chars();
    let mut out: String = iter.by_ref().take(n).collect();
    if iter.next().is_some() {
        out.push('…');
    }
    out
}

/// `ToolExtensions` implementation that fires `PreToolUse` / `PostToolUse`
/// subprocess hooks. Composes with the in-process policy gate
/// (`PermissionChecker`) via the agent loop's `before_tool_call_block` —
/// the policy gate runs first; only allowed calls reach this impl.
///
/// The `before_tool_call` semantics:
///
/// * No `PreToolUse` hooks declared → `Ok(None)`.
/// * Hook returns `MutatedJson(v)` → the registry threads `v` through
///   subsequent chain members and returns `Ok(Some(final))` if any hook
///   rewrote.
/// * Hook returns `Blocked { reason }` → registry surfaces `reason` as
///   the error string; the agent loop emits a `role:"tool"` block
///   message carrying it.
/// * `PassThrough` / `SoftFailed` (or text/inject-context outcomes that
///   don't apply to `PreToolUse`) → continue with the current args.
///
/// The `after_tool_call` semantics:
///
/// * No `PostToolUse` hooks declared → the result passes through
///   unchanged.
/// * Hook returns `InjectContext(text)` → the text is queued for the
///   system-reminder insertion path (wired in a follow-up commit) and
///   the result is unchanged.
/// * Hook returns `Blocked { reason }` → CC's posture: the model sees
///   the hook's rejection as a tool error. The `ToolResult` is
///   transformed to carry `is_error: true` with the reason appended;
///   the original result content is preserved so the model can react.
/// * `PassThrough` / `SoftFailed` → continue with the current result.
///
/// Subprocess hooks fire even when the registry's session envelope
/// context is unset (logged at `debug` once). Production wiring sets
/// it via `set_envelope_context` in `Session::open`.
#[async_trait]
impl ToolExtensions for ExtensionRegistry {
    async fn before_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.pre_tool_use.clone()
        };
        if specs.is_empty() {
            return Ok(None);
        }
        let (session_id, cwd) = self.envelope_context().await;
        let dispatch_ctx = DispatchContext {
            session_id: &session_id,
            cwd: &cwd,
        };

        let original = args.clone();
        let mut current = args.clone();
        for spec in &specs {
            if !spec.applies_to_tool(tool_name) {
                continue;
            }
            let payload = dispatch::ExtensionPayload::PreToolUse {
                tool_name,
                tool_input: &current,
            };
            let outcome = dispatch::run_hook(spec, payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::MutatedJson(v) => current = v,
                ExtensionOutcome::Blocked { reason, stderr } => {
                    return Err(reason.unwrap_or_else(|| trim_one_line(&stderr)));
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    tracing::debug!(
                        hook = %spec.display_name(),
                        reason,
                        "PreToolUse hook soft-failed; continuing chain"
                    );
                }
                // Variants that don't apply to PreToolUse semantics.
                // `Mutated(String)` would be a text rewrite for the
                // wrong event; `InjectContext`/`KeepLooping` are
                // post-tool / Stop concerns. Drop with a debug log so
                // misuse is visible without crashing the loop.
                other => {
                    tracing::debug!(
                        hook = %spec.display_name(),
                        outcome = ?other,
                        "PreToolUse hook returned outcome that does not apply; ignoring"
                    );
                }
            }
        }
        if current == original {
            Ok(None)
        } else {
            Ok(Some(current))
        }
    }

    async fn drain_pending_context(&self) -> Vec<PendingInjection> {
        self.drain_injections().await
    }

    async fn after_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        result: ToolResult,
    ) -> ToolResult {
        let specs: Vec<ExtensionSpec> = {
            let guard = self.inner.read().await;
            guard.post_tool_use.clone()
        };
        if specs.is_empty() {
            return result;
        }
        let (session_id, cwd) = self.envelope_context().await;
        let dispatch_ctx = DispatchContext {
            session_id: &session_id,
            cwd: &cwd,
        };

        // The tool_response shape mirrors CC: { success, content }.
        let tool_response = serde_json::json!({
            "success": !result.is_error,
            "content": result.content,
        });

        let mut current = result;
        for spec in &specs {
            if !spec.applies_to_tool(tool_name) {
                continue;
            }
            let payload = dispatch::ExtensionPayload::PostToolUse {
                tool_name,
                tool_input: args,
                tool_response: &tool_response,
            };
            let outcome = dispatch::run_hook(spec, payload, &dispatch_ctx).await;
            match outcome {
                ExtensionOutcome::PassThrough => {}
                ExtensionOutcome::InjectContext(text) => {
                    // Queue the context for the next LLM call. The agent
                    // loop drains `pending_injections` after each tool
                    // batch and prepends each entry as a
                    // `<system-reminder>` block (wired in commit 5).
                    self.queue_injection(PendingInjection {
                        text,
                        source: spec.display_name(),
                        event: ExtensionEvent::PostToolUse,
                    })
                    .await;
                }
                ExtensionOutcome::Blocked { reason, stderr } => {
                    // CC posture: a PostToolUse "block" frames the tool
                    // result as an error the model should react to. The
                    // original content is preserved alongside the reason
                    // so the model sees both what ran and why it was
                    // rejected.
                    let why = reason.unwrap_or_else(|| trim_one_line(&stderr));
                    current = ToolResult {
                        content: format!("{}\n[hook rejection: {}]", current.content, why),
                        is_error: true,
                    };
                }
                ExtensionOutcome::SoftFailed { reason } => {
                    tracing::debug!(
                        hook = %spec.display_name(),
                        reason,
                        "PostToolUse hook soft-failed; continuing chain"
                    );
                }
                // Text rewrite / object rewrite / keep-looping don't
                // apply to PostToolUse.
                other => {
                    tracing::debug!(
                        hook = %spec.display_name(),
                        outcome = ?other,
                        "PostToolUse hook returned outcome that does not apply; ignoring"
                    );
                }
            }
        }
        current
    }
}

/// Compose multiple `ToolExtensions` impls into a single dispatch point. The
/// agent loop calls only ONE `ToolExtensions` impl; this wrapper lets the
/// in-tree policy gate (`PermissionChecker`) and the subprocess
/// `ExtensionRegistry` both fire from the same path.
///
/// Order matters:
/// * `before_tool_call` runs children left-to-right. Each child sees
///   the args produced by the previous child. The first `Err` short-
///   circuits the chain — typical layering puts the policy gate
///   first so a denied call never reaches user-authored hooks.
/// * `after_tool_call` runs children left-to-right over a folding
///   `ToolResult`. Each child sees what the previous child produced.
/// * `drain_pending_context` concatenates all children's drains in
///   declaration order.
pub struct ChainedToolExtensions {
    children: Vec<Box<dyn ToolExtensions>>,
}

impl ChainedToolExtensions {
    pub fn new(children: Vec<Box<dyn ToolExtensions>>) -> Self {
        Self { children }
    }

    /// Convenience: wrap a single existing impl plus a `ExtensionRegistry`
    /// behind a `ChainedToolExtensions`. Used by `Session::set_hooks` to
    /// fold the subprocess registry into whatever policy gate the
    /// runner installs (typically `PermissionChecker`).
    pub fn wrap(
        policy: Box<dyn ToolExtensions>,
        registry: ExtensionRegistry,
    ) -> Box<dyn ToolExtensions> {
        Box::new(Self::new(vec![policy, Box::new(registry)]))
    }
}

#[async_trait]
impl ToolExtensions for ChainedToolExtensions {
    async fn before_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        let original = args.clone();
        let mut current = args.clone();
        for child in &self.children {
            match child.before_tool_call(tool_name, &current).await? {
                None => {}
                Some(rewritten) => current = rewritten,
            }
        }
        if current == original {
            Ok(None)
        } else {
            Ok(Some(current))
        }
    }

    async fn after_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        result: ToolResult,
    ) -> ToolResult {
        let mut current = result;
        for child in &self.children {
            current = child.after_tool_call(tool_name, args, current).await;
        }
        current
    }

    async fn drain_pending_context(&self) -> Vec<PendingInjection> {
        let mut out = Vec::new();
        for child in &self.children {
            out.extend(child.drain_pending_context().await);
        }
        out
    }
}

/// Render a queued [`PendingInjection`] as the `<system-reminder>`
/// block text the agent loop prepends before the next LLM call.
/// Single source of truth for the framing so dashboards and tests can
/// pin the wire shape.
pub fn render_injection_as_system_reminder(inj: &PendingInjection) -> String {
    format!(
        "<system-reminder>\nhook {} ({}): {}\n</system-reminder>",
        inj.event.as_str(),
        inj.source,
        inj.text
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ExtensionContext<'static> {
        ExtensionContext {
            session_id: "s",
            cwd: "/tmp",
        }
    }

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
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
    async fn empty_registry_passes_through() {
        let reg = ExtensionRegistry::empty();
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("hello", ctx(), &tx).await;
        assert_eq!(out, PromptExtensionResult::Continue("hello".to_string()));
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn chain_of_two_hooks_composes_left_to_right() {
        let tmp = crate::util::unique_temp_dir("ignis-hooks-chain");
        // First hook: replaces prompt with "STEP1".
        let s1 = write_script(
            &tmp,
            "s1.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"STEP1\"}}'\n",
        );
        // Second hook: drains the stdin (which now carries STEP1 as the
        // prompt) and writes the final "STEP1!".
        let s2 = write_script(
            &tmp,
            "s2.sh",
            r#"#!/bin/sh
cat >/dev/null
printf '%s' '{"hookSpecificOutput":{"updatedInput":"STEP1!"}}'
"#,
        );

        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![
                ExtensionSpec {
                    program: s1,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
                ExtensionSpec {
                    program: s2,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
            ],
            assistant_message_render: vec![],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, _rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("A", ctx(), &tx).await;
        assert_eq!(out, PromptExtensionResult::Continue("STEP1!".to_string()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn soft_failure_mid_chain_keeps_last_good_and_warns() {
        let tmp = crate::util::unique_temp_dir("ignis-hooks-softfail");
        // First hook rewrites successfully.
        let good = write_script(
            &tmp,
            "good.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":\"GOOD\"}}'\n",
        );
        // Second hook returns malformed JSON.
        let bad = write_script(
            &tmp,
            "bad.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf 'not json'\n",
        );

        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![
                ExtensionSpec {
                    program: good,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
                ExtensionSpec {
                    program: bad,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
            ],
            assistant_message_render: vec![],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("A", ctx(), &tx).await;
        // Last good value is preserved.
        assert_eq!(out, PromptExtensionResult::Continue("GOOD".to_string()));
        drop(tx);
        // Exactly one warning emitted.
        let mut warnings = 0;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, AgentEvent::Warning { .. }) {
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn user_prompt_submit_block_returns_blocked_and_warns() {
        // Spec: exit 2 from a UserPromptSubmit hook MUST short-circuit the
        // turn. Previously this path returned the last good string and the
        // caller pushed it to history anyway — defeating the only event
        // where blocking is meaningful.
        let tmp = crate::util::unique_temp_dir("ignis-hooks-prompt-block");
        let blocker = write_script(
            &tmp,
            "blk.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf 'leaks secret' >&2\nexit 2\n",
        );
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![ExtensionSpec {
                program: blocker,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            assistant_message_render: vec![],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("original", ctx(), &tx).await;
        match out {
            PromptExtensionResult::Blocked { stderr } => {
                assert!(stderr.contains("leaks secret"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        drop(tx);
        let mut warnings = 0;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Warning { source, message } = ev {
                assert_eq!(source, "UserPromptSubmit");
                assert!(message.contains("blocked"));
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn assistant_render_block_is_degraded_to_warning() {
        let tmp = crate::util::unique_temp_dir("ignis-hooks-render-block");
        // Hook returns exit 2 — for render, must NOT lose the content.
        let blocker = write_script(
            &tmp,
            "blk.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf 'no' >&2\nexit 2\n",
        );
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![],
            assistant_message_render: vec![ExtensionSpec {
                program: blocker,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_assistant_message_render("kept", ctx(), &tx).await;
        assert_eq!(out, "kept");
        drop(tx);
        let mut warnings = 0;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Warning { source, .. } = ev {
                assert_eq!(source, "AssistantMessageRender");
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn truncate_chars_handles_cjk_at_boundary() {
        // 250 CJK characters (each 3 bytes in UTF-8) — a byte-slice at 200
        // would have panicked at a multibyte boundary. Char-safe trim
        // returns 200 chars + an ellipsis.
        let s: String = "中".repeat(250);
        let out = truncate_chars(&s, 200);
        assert_eq!(out.chars().count(), 201);
        assert!(out.ends_with('…'));
        // No truncation when already short enough.
        assert_eq!(truncate_chars("hi", 200), "hi");
        // Exact-length input: no ellipsis.
        let exact: String = "a".repeat(200);
        assert_eq!(truncate_chars(&exact, 200), exact);
    }

    #[tokio::test]
    async fn reload_swaps_config_in_place() {
        let tmp = crate::util::unique_temp_dir("ignis-hooks-reload");
        let home = tmp.join("home");
        std::fs::create_dir_all(home.join(".ignis")).unwrap();

        // No file → empty registry.
        let reg = ExtensionRegistry::from_config_dir(&home).unwrap();
        assert_eq!(reg.inner.read().await.total_len(), 0);

        // Write a hooks.json with one entry, reload.
        let echo = write_script(&home, "echo.sh", "#!/bin/sh\ncat >/dev/null\nprintf '{}'\n");
        let echo_str = echo.to_string_lossy();
        let raw = format!(r#"{{"hooks": {{"UserPromptSubmit": [{{"command": "{echo_str}"}}]}}}}"#);
        std::fs::write(home.join(".ignis/hooks.json"), raw).unwrap();
        let count = reg.reload(&home).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(reg.inner.read().await.total_len(), 1);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn snapshot_clones_current_config() {
        // `/hooks` listing is driven by `snapshot()` — pins that it returns
        // the in-memory state (not a disk probe) and is decoupled from any
        // concurrent `run_*` chain.
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![ExtensionSpec {
                program: PathBuf::from("/a/hook.sh"),
                args: vec!["--flag".to_string()],
                timeout_ms: 7_000,
                matcher: None,
            }],
            assistant_message_render: vec![],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg.clone());
        let snap = reg.snapshot().await;
        assert_eq!(snap, cfg);
    }

    #[test]
    fn format_list_empty_registry_prompts_to_install() {
        let out = format_list(&ExtensionsConfig::default());
        assert!(out.starts_with("[info]"));
        assert!(out.contains("no extensions registered"));
        // The hint mentions both the file path and the reload action so a
        // user who just typed `/hooks` knows what to do next.
        assert!(out.contains("~/.ignis/extensions.json"));
        assert!(out.contains("/extensions reload"));
    }

    #[test]
    fn format_list_one_hook_per_event_uses_singular_wording() {
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![ExtensionSpec {
                program: PathBuf::from("/opt/translate/run.py"),
                args: vec![],
                timeout_ms: 10_000,
                matcher: None,
            }],
            assistant_message_render: vec![],
            ..ExtensionsConfig::default()
        };
        let out = format_list(&cfg);
        assert!(out.contains("1 extension registered "), "got: {out}");
        assert!(!out.contains("1 extensions"), "got: {out}");
        assert!(out.contains("UserPromptSubmit (1):"));
        assert!(out.contains("translate")); // file_stem of run.py
        assert!(out.contains("/opt/translate/run.py"));
        assert!(out.contains("timeout 10000ms"));
        // No args tail when argv is empty.
        assert!(!out.contains("--"));
    }

    #[test]
    fn format_list_multi_hook_renders_each_chain_and_argv() {
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![
                ExtensionSpec {
                    program: PathBuf::from("/opt/translate/run.py"),
                    args: vec!["--source".to_string(), "en".to_string()],
                    timeout_ms: 30_000,
                    matcher: None,
                },
                ExtensionSpec {
                    program: PathBuf::from("/opt/redact.sh"),
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
            ],
            assistant_message_render: vec![ExtensionSpec {
                program: PathBuf::from("/opt/translate/run.py"),
                args: vec![],
                timeout_ms: 10_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let out = format_list(&cfg);
        // Plural wording, both event headers, both hooks in the prompt
        // chain, the single render hook, and the argv tail all appear.
        assert!(out.contains("3 extensions registered "), "got: {out}");
        assert!(out.contains("UserPromptSubmit (2):"));
        assert!(out.contains("AssistantMessageRender (1):"));
        assert!(out.contains("translate"));
        assert!(out.contains("redact"));
        assert!(out.contains("--source en"), "argv tail missing in: {out}");
        assert!(out.contains("timeout 30000ms"));
    }

    #[test]
    fn format_list_omits_empty_event_chains() {
        // If only AssistantMessageRender is configured, the UserPromptSubmit
        // header must not be rendered as `(0):` — that's noise.
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![],
            assistant_message_render: vec![ExtensionSpec {
                program: PathBuf::from("/opt/render.sh"),
                args: vec![],
                timeout_ms: 1_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let out = format_list(&cfg);
        assert!(!out.contains("UserPromptSubmit"));
        assert!(out.contains("AssistantMessageRender (1):"));
    }

    #[test]
    fn format_list_aligns_columns_when_names_have_different_lengths() {
        // Regression: a long `display_name()` (the file_stem of the
        // program) co-existing with a short one would, with a fixed
        // `:16` width, push the program column right for the long name
        // and leave stray spaces for the short one. The formatter now
        // computes the max width across the call, so both program
        // paths line up. We assert by checking the *byte* offset of the
        // program path on each line — they should all be equal.
        //
        // `display_name()` is the file_stem of the program, not any
        // parent directory component — so we put the long name in the
        // file name itself: `…/translate-to-french-v2.py` (stem is
        // `translate-to-french-v2`, 22 chars) vs. `…/redact.sh` (stem
        // is `redact`, 6 chars).
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![
                ExtensionSpec {
                    program: PathBuf::from("/opt/translate-to-french-v2.py"),
                    args: vec![],
                    timeout_ms: 10_000,
                    matcher: None,
                },
                ExtensionSpec {
                    program: PathBuf::from("/opt/redact.sh"),
                    args: vec![],
                    timeout_ms: 10_000,
                    matcher: None,
                },
            ],
            assistant_message_render: vec![],
            ..ExtensionsConfig::default()
        };
        let out = format_list(&cfg);
        // Expected max name width: "translate-to-french-v2" == 22 chars.
        // Each hook line has the prefix "    \u{00b7} " (4-space indent +
        // bullet + space) == 7 bytes (the bullet is 2 bytes in UTF-8),
        // then a name left-padded to 22, then "  " (2 spaces) before the
        // program. So the program column starts at byte 7 + 22 + 2 == 31
        // for every line.
        let expected_prog_col = 7 + "translate-to-french-v2".chars().count() + 2;
        let hook_lines: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("· ") && l.contains("(timeout"))
            .collect();
        assert_eq!(hook_lines.len(), 2, "got: {out}");
        for line in &hook_lines {
            assert!(
                line[expected_prog_col..].starts_with("/opt/"),
                "program column mis-aligned: `{line}` (len={}, expected prog at byte {expected_prog_col})",
                line.len()
            );
        }
    }

    #[test]
    fn format_list_iterates_all_known_events() {
        // Pin the "list of events" surface: formatter iterates the same
        // slice the registry is typed against, so adding a new event
        // variant to `ExtensionEvent::ALL` automatically appears in the
        // listing without touching this function.
        assert_eq!(ExtensionEvent::ALL.len(), 9);
        let cfg = ExtensionsConfig {
            user_prompt_submit: vec![ExtensionSpec {
                program: PathBuf::from("/a"),
                args: vec![],
                timeout_ms: 1_000,
                matcher: None,
            }],
            assistant_message_render: vec![ExtensionSpec {
                program: PathBuf::from("/b"),
                args: vec![],
                timeout_ms: 1_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let out = format_list(&cfg);
        assert!(out.contains("UserPromptSubmit (1):"));
        assert!(out.contains("AssistantMessageRender (1):"));
    }

    // -------- ToolExtensions impl for ExtensionRegistry --------

    #[tokio::test]
    async fn tool_hooks_before_with_no_hooks_returns_ok_none() {
        let reg = ExtensionRegistry::empty();
        let args = serde_json::json!({"command": "ls"});
        assert_eq!(reg.before_tool_call("Bash", &args).await, Ok(None));
    }

    #[tokio::test]
    async fn tool_hooks_pre_tool_use_rewrites_args() {
        // PreToolUse hook returns `updatedInput: {…}` — registry threads
        // the rewritten object back as `Ok(Some(new_args))`.
        let tmp = crate::util::unique_temp_dir("ignis-th-rewrite");
        let s = write_script(
            &tmp,
            "rewrite.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"ls -la\"}}}'\n",
        );
        let cfg = ExtensionsConfig {
            pre_tool_use: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let args = serde_json::json!({"command": "ls"});
        let out = reg.before_tool_call("Bash", &args).await.unwrap();
        let rewritten = out.expect("expected rewrite");
        assert_eq!(rewritten["command"], "ls -la");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn tool_hooks_pre_tool_use_block_returns_err_with_reason() {
        let tmp = crate::util::unique_temp_dir("ignis-th-block");
        let s = write_script(
            &tmp,
            "block.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"decision\":\"block\",\"reason\":\"rm -rf is destructive\"}'\n",
        );
        let cfg = ExtensionsConfig {
            pre_tool_use: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let args = serde_json::json!({"command": "rm -rf /"});
        match reg.before_tool_call("Bash", &args).await {
            Err(reason) => assert_eq!(reason, "rm -rf is destructive"),
            other => panic!("expected Err, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn tool_hooks_matcher_skips_non_matching_tool() {
        // Matcher `Bash` — when called with `Edit` the hook must not
        // spawn. Pins the matcher fast-path so the registry doesn't pay
        // a process fork for every-event-every-tool dispatch.
        let tmp = crate::util::unique_temp_dir("ignis-th-matcher");
        // The "block" hook would Err if it fired; if matcher works it
        // never runs and the call returns Ok(None).
        let s = write_script(
            &tmp,
            "block.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf 'wrong tool' >&2\nexit 2\n",
        );
        let raw = format!(
            r#"{{"hooks":{{"PreToolUse":[{{"command":"{}","matcher":"Bash"}}]}}}}"#,
            s.to_string_lossy()
        );
        let cfg = ExtensionsConfig::from_str(&raw, std::path::Path::new("/h")).unwrap();
        let reg = ExtensionRegistry::from_config(cfg);
        let args = serde_json::json!({"file_path": "/a"});
        // `Edit` doesn't match `Bash` — hook is skipped, result Ok(None).
        assert_eq!(reg.before_tool_call("Edit", &args).await, Ok(None));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn tool_hooks_after_with_no_hooks_returns_result_unchanged() {
        let reg = ExtensionRegistry::empty();
        let args = serde_json::json!({});
        let result = ToolResult {
            content: "ok".to_string(),
            is_error: false,
        };
        let out = reg.after_tool_call("Bash", &args, result.clone()).await;
        assert_eq!(out.content, "ok");
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn tool_hooks_post_tool_use_block_appends_reason_and_flags_error() {
        // CC posture for PostToolUse Block: the result still flows to the
        // model, but is_error flips to true and the hook's reason is
        // appended so the model can react.
        let tmp = crate::util::unique_temp_dir("ignis-th-post-block");
        let s = write_script(
            &tmp,
            "post.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"decision\":\"block\",\"reason\":\"tests still failing\"}'\n",
        );
        let cfg = ExtensionsConfig {
            post_tool_use: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let args = serde_json::json!({});
        let result = ToolResult {
            content: "ran successfully".to_string(),
            is_error: false,
        };
        let out = reg.after_tool_call("Bash", &args, result).await;
        assert!(out.is_error, "PostToolUse block should flip is_error");
        assert!(
            out.content.contains("ran successfully"),
            "original content must be preserved: {}",
            out.content
        );
        assert!(
            out.content.contains("tests still failing"),
            "reason must be appended: {}",
            out.content
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn tool_hooks_post_tool_use_inject_context_queues_for_next_llm_call() {
        // A PostToolUse hook returning `additional_context` doesn't
        // change the tool result — the text is queued as a
        // `PendingInjection` for the agent loop to flush as a
        // `<system-reminder>` before the next LLM call (commit 5
        // consumer). Order matches firing order.
        let tmp = crate::util::unique_temp_dir("ignis-th-inject");
        let s1 = write_script(
            &tmp,
            "inject1.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":\"alpha\"}}'\n",
        );
        let s2 = write_script(
            &tmp,
            "inject2.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":\"beta\"}}'\n",
        );
        let cfg = ExtensionsConfig {
            post_tool_use: vec![
                ExtensionSpec {
                    program: s1,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
                ExtensionSpec {
                    program: s2,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
            ],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let args = serde_json::json!({});
        let result = ToolResult {
            content: "ran".to_string(),
            is_error: false,
        };
        let out = reg.after_tool_call("Bash", &args, result.clone()).await;
        // Tool result is unchanged — InjectContext does NOT mutate
        // `result`, only enqueues context for the next turn.
        assert_eq!(out.content, "ran");
        assert!(!out.is_error);
        // Queue carries both injections in order with provenance.
        let drained = reg.drain_injections().await;
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "alpha");
        assert_eq!(drained[0].event, ExtensionEvent::PostToolUse);
        assert!(drained[0].source.contains("inject1"));
        assert_eq!(drained[1].text, "beta");
        assert!(drained[1].source.contains("inject2"));
        // Second drain is empty — `drain_injections` consumes.
        assert_eq!(reg.pending_injection_count().await, 0);
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- SessionStart ----

    #[tokio::test]
    async fn session_start_no_hooks_is_noop() {
        let reg = ExtensionRegistry::empty();
        let (tx, _rx) = mpsc::channel(8);
        reg.run_session_start("new", ctx(), &tx).await;
        // No queue, no warnings — just a no-op.
        assert_eq!(reg.pending_injection_count().await, 0);
    }

    #[tokio::test]
    async fn session_start_inject_context_queues_for_first_llm_call() {
        let tmp = crate::util::unique_temp_dir("ignis-ss-inject");
        let s = write_script(
            &tmp,
            "ss.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":\"welcome back\"}}'\n",
        );
        let cfg = ExtensionsConfig {
            session_start: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, _rx) = mpsc::channel(8);
        reg.run_session_start("resume", ctx(), &tx).await;
        let drained = reg.drain_injections().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].text, "welcome back");
        assert_eq!(drained[0].event, ExtensionEvent::SessionStart);
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- Stop ----

    #[tokio::test]
    async fn stop_no_hooks_returns_false() {
        let reg = ExtensionRegistry::empty();
        let (tx, _rx) = mpsc::channel(8);
        assert!(!reg.run_stop("/tmp/transcript", ctx(), &tx).await);
    }

    #[tokio::test]
    async fn stop_decision_block_returns_keep_looping_true_and_queues_reminder() {
        // CC inversion: a Stop hook returning `decision:"block"` tells
        // the loop to continue instead of terminating. Dispatch maps
        // this to KeepLooping; run_stop returns true and queues the
        // reason as a system reminder for the next LLM call.
        let tmp = crate::util::unique_temp_dir("ignis-stop-keep");
        let s = write_script(
            &tmp,
            "stop.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"decision\":\"block\",\"reason\":\"tests still failing\"}'\n",
        );
        let cfg = ExtensionsConfig {
            stop: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, _rx) = mpsc::channel(8);
        let keep = reg.run_stop("/tmp/t", ctx(), &tx).await;
        assert!(keep, "decision:block on Stop must return KeepLooping");
        let drained = reg.drain_injections().await;
        assert_eq!(drained.len(), 1);
        assert!(drained[0].text.contains("tests still failing"));
        assert!(drained[0].text.contains("stopped continuation"));
        assert_eq!(drained[0].event, ExtensionEvent::Stop);
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- SystemPromptCompose ----

    #[tokio::test]
    async fn system_prompt_compose_no_hooks_returns_base() {
        let reg = ExtensionRegistry::empty();
        let (tx, _rx) = mpsc::channel(8);
        let out = reg
            .run_system_prompt_compose("base prompt", "test-model", ctx(), &tx)
            .await;
        assert_eq!(out, "base prompt");
    }

    #[tokio::test]
    async fn system_prompt_compose_rewrite_returns_updated() {
        let tmp = crate::util::unique_temp_dir("ignis-spc-rewrite");
        let s = write_script(
            &tmp,
            "trim.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"updatedSystemPrompt\":\"trimmed\"}}'\n",
        );
        let cfg = ExtensionsConfig {
            system_prompt_compose: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, _rx) = mpsc::channel(8);
        let out = reg
            .run_system_prompt_compose("verbose base", "test-model", ctx(), &tx)
            .await;
        assert_eq!(out, "trimmed");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn system_prompt_compose_inject_context_queued() {
        let tmp = crate::util::unique_temp_dir("ignis-spc-inject");
        let s = write_script(
            &tmp,
            "inject.sh",
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":\"note about prompt\"}}'\n",
        );
        let cfg = ExtensionsConfig {
            system_prompt_compose: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, _rx) = mpsc::channel(8);
        let out = reg
            .run_system_prompt_compose("base", "test-model", ctx(), &tx)
            .await;
        // Base prompt is unchanged when only context is injected.
        assert_eq!(out, "base");
        // The context is queued for the next-LLM-call drain.
        let drained = reg.drain_injections().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].text, "note about prompt");
        assert_eq!(drained[0].event, ExtensionEvent::SystemPromptCompose);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn system_prompt_compose_chain_threads_rewrite() {
        // Hook 1: prepends `A:`. Hook 2: prepends `B:` to whatever it
        // receives. The second hook MUST see the first hook's output;
        // pins the chain-threading invariant for SystemPromptCompose.
        let tmp = crate::util::unique_temp_dir("ignis-spc-chain");
        let s1 = write_script(
            &tmp,
            "h1.sh",
            "#!/bin/sh\nread payload\nprintf '%s' '{\"hookSpecificOutput\":{\"updatedSystemPrompt\":\"A:original\"}}'\n",
        );
        let s2 = write_script(
            &tmp,
            "h2.sh",
            // Always prepend B: to whatever it gets — the value carries
            // the previous hook's rewrite.
            "#!/bin/sh\nread payload\nprintf '%s' '{\"hookSpecificOutput\":{\"updatedSystemPrompt\":\"B:received\"}}'\n",
        );
        let cfg = ExtensionsConfig {
            system_prompt_compose: vec![
                ExtensionSpec {
                    program: s1,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
                ExtensionSpec {
                    program: s2,
                    args: vec![],
                    timeout_ms: 5_000,
                    matcher: None,
                },
            ],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let (tx, _rx) = mpsc::channel(8);
        let out = reg
            .run_system_prompt_compose("original", "test-model", ctx(), &tx)
            .await;
        // Final rewrite is the last hook's output.
        assert_eq!(out, "B:received");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // ---- ChainedToolExtensions ----

    struct AllowAll;
    struct AlwaysBlock(&'static str);
    struct ConstantRewrite(serde_json::Value);

    #[async_trait]
    impl ToolExtensions for AllowAll {}

    #[async_trait]
    impl ToolExtensions for AlwaysBlock {
        async fn before_tool_call(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<Option<serde_json::Value>, String> {
            Err(self.0.to_string())
        }
    }

    #[async_trait]
    impl ToolExtensions for ConstantRewrite {
        async fn before_tool_call(
            &self,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<Option<serde_json::Value>, String> {
            Ok(Some(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn chained_first_block_short_circuits_chain() {
        // Policy gate denies → registry must never be consulted. Pins
        // the layering invariant used by Session::set_hooks.
        let chain = ChainedToolExtensions::new(vec![
            Box::new(AlwaysBlock("policy denied")),
            Box::new(ConstantRewrite(serde_json::json!({"never": "ran"}))),
        ]);
        let res = chain.before_tool_call("Bash", &serde_json::json!({})).await;
        assert_eq!(res, Err("policy denied".to_string()));
    }

    #[tokio::test]
    async fn chained_rewrites_thread_through_next_child() {
        // First child rewrites → second child sees rewrite. Final args
        // are the second child's output if any. Tests the
        // ChainedToolExtensions args-threading contract.
        let chain = ChainedToolExtensions::new(vec![
            Box::new(ConstantRewrite(serde_json::json!({"step": 1}))),
            Box::new(ConstantRewrite(serde_json::json!({"step": 2}))),
        ]);
        let res = chain
            .before_tool_call("Bash", &serde_json::json!({"step": 0}))
            .await
            .unwrap();
        assert_eq!(res, Some(serde_json::json!({"step": 2})));
    }

    #[tokio::test]
    async fn chained_drain_aggregates_in_order() {
        // Two ExtensionRegistry impls queued with different injections —
        // the chain returns both, registry-A entries before registry-B.
        let reg_a = ExtensionRegistry::empty();
        reg_a
            .queue_injection(PendingInjection {
                text: "from a".to_string(),
                source: "a".to_string(),
                event: ExtensionEvent::PostToolUse,
            })
            .await;
        let reg_b = ExtensionRegistry::empty();
        reg_b
            .queue_injection(PendingInjection {
                text: "from b".to_string(),
                source: "b".to_string(),
                event: ExtensionEvent::PostToolUse,
            })
            .await;
        let chain = ChainedToolExtensions::new(vec![Box::new(reg_a), Box::new(reg_b)]);
        let drained = chain.drain_pending_context().await;
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "from a");
        assert_eq!(drained[1].text, "from b");
        // Second drain is empty — children consumed.
        assert_eq!(chain.drain_pending_context().await.len(), 0);
    }

    #[tokio::test]
    async fn render_injection_as_system_reminder_wraps_with_provenance() {
        // Pin the rendered shape: the wire format consumed by both
        // dashboards and the agent loop's history.push.
        let inj = PendingInjection {
            text: "tests are still failing".to_string(),
            source: "auto-test".to_string(),
            event: ExtensionEvent::PostToolUse,
        };
        let rendered = render_injection_as_system_reminder(&inj);
        assert!(rendered.starts_with("<system-reminder>"));
        assert!(rendered.ends_with("</system-reminder>"));
        assert!(rendered.contains("hook PostToolUse (auto-test): tests are still failing"));
    }

    #[tokio::test]
    async fn chained_wrap_helper_runs_policy_then_registry() {
        // Smoke test for Session::set_hooks's wiring: wrap(policy,
        // registry) → policy fires first; if it allows, registry sees
        // the args.
        let registry = ExtensionRegistry::empty();
        let chained = ChainedToolExtensions::wrap(Box::new(AllowAll), registry);
        let args = serde_json::json!({"command": "ls"});
        // No registry hooks declared → Ok(None).
        let res = chained.before_tool_call("Bash", &args).await;
        assert_eq!(res, Ok(None));
    }

    #[tokio::test]
    async fn tool_hooks_post_tool_use_pass_through_keeps_result() {
        let tmp = crate::util::unique_temp_dir("ignis-th-post-pass");
        let s = write_script(&tmp, "noop.sh", "#!/bin/sh\ncat >/dev/null\n");
        let cfg = ExtensionsConfig {
            post_tool_use: vec![ExtensionSpec {
                program: s,
                args: vec![],
                timeout_ms: 5_000,
                matcher: None,
            }],
            ..ExtensionsConfig::default()
        };
        let reg = ExtensionRegistry::from_config(cfg);
        let args = serde_json::json!({});
        let result = ToolResult {
            content: "result".to_string(),
            is_error: false,
        };
        let out = reg.after_tool_call("Bash", &args, result.clone()).await;
        assert_eq!(out.content, "result");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
