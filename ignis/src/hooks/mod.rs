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
//! **Security:** hook subprocesses run with an env-var allowlist and (on
//! Linux) a Landlock filesystem sandbox by default. Per-hook
//! `sandbox: false` opts out; per-hook `env: [...]` extends the universal
//! allowlist (`PATH HOME USER LANG LC_ALL TZ`). Network egress is NOT
//! restricted — a hook with `env: ["ANTHROPIC_API_KEY"]` can still
//! exfiltrate it. See `docs/usage/hooks.md` for the full threat model.

pub mod config;
pub mod dispatch;
pub mod protocol;
pub mod sandbox;

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::RwLock;

use crate::AgentEvent;

pub use config::{HookSpec, HooksConfig, DEFAULT_TIMEOUT_MS};
pub use dispatch::{DispatchContext, HookOutcome};
pub use protocol::{HookEvent, HookInput, HookOutput, HookSpecificOutput};
pub use sandbox::SandboxStatus;

/// Context the registry needs at every dispatch call. Borrowed strings so
/// callers don't have to allocate per turn.
#[derive(Debug, Clone, Copy)]
pub struct HookContext<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
}

/// Sender for `AgentEvent::Warning` lines. The registry owns no channel of
/// its own — every dispatch path takes the channel the caller already has.
pub type EventSender = mpsc::Sender<AgentEvent>;

/// The registered hook chains, loaded once at session start and swappable
/// via `/hooks reload`. The wrapper holds an `Arc<RwLock<…>>` so the swap
/// is cheap and reload doesn't tear down outstanding references.
#[derive(Debug, Default, Clone)]
pub struct HookRegistry {
    inner: Arc<RwLock<HooksConfig>>,
}

impl HookRegistry {
    /// Load `~/.ignis/hooks.json` into a fresh registry.
    pub fn from_config_dir(home: &Path) -> anyhow::Result<Self> {
        let cfg = HooksConfig::from_home(home)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(cfg)),
        })
    }

    /// Empty registry — useful in tests and when no home dir is available.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Construct directly from a parsed config (test helper, used by the
    /// integration test in `tests/hook_roundtrip.rs`).
    pub fn from_config(cfg: HooksConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(cfg)),
        }
    }

    /// Rebuild the registry from disk in place. Returns the new hook count
    /// for the `/hooks reload` confirmation line.
    ///
    /// Also clears the dispatcher's per-session "Landlock unavailable"
    /// suppression set: a freshly-edited hook gets a fresh degradation
    /// notice instead of being silently swallowed because an earlier
    /// invocation of the same name already warned once.
    pub async fn reload(&self, home: &Path) -> anyhow::Result<usize> {
        let cfg = HooksConfig::from_home(home)?;
        let total = cfg.total_len();
        let mut guard = self.inner.write().await;
        *guard = cfg;
        dispatch::reset_sandbox_warnings();
        Ok(total)
    }

    /// Run the `UserPromptSubmit` chain. Returns:
    ///
    /// - [`PromptHookResult::Continue`] with the (possibly rewritten) prompt
    ///   when every hook passed through or successfully rewrote, or when a
    ///   soft-failure short-circuited the chain at the last good value
    ///   (caller pushes the string to history and runs the agent).
    /// - [`PromptHookResult::Blocked`] when a hook returned exit 2 or
    ///   `continue: false`. The spec's iron rule: hooks cannot kill a
    ///   turn EXCEPT here — `UserPromptSubmit` is the one event where
    ///   blocking is meaningful. The caller MUST NOT push the prompt to
    ///   history and MUST NOT call the agent; the warning has already
    ///   been emitted to `tx`, so the user sees the block reason.
    pub async fn run_user_prompt_submit(
        &self,
        prompt: &str,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> PromptHookResult {
        self.run_prompt_chain(prompt, ctx, tx).await
    }

    /// Run the `AssistantMessageRender` chain. Same semantics as
    /// `run_user_prompt_submit` but for `updatedOutput`. Hooks blocking
    /// (`exit 2` or `continue: false`) are degraded to a soft failure —
    /// the assistant message already exists; we can't lose it.
    pub async fn run_assistant_message_render(
        &self,
        content: &str,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> String {
        self.run_render_chain(content, ctx, tx).await
    }

    /// Run the `PreToolUse` chain for a tool call. Hooks see the tool name +
    /// args and may rewrite the args (`updatedInput`, parsed back to JSON) or
    /// block the call (`continue:false` / exit 2). Runs BEFORE the permission
    /// gate, so a rewrite is what both the gate and the tool see. A malformed
    /// rewrite or any failure degrades to the prior args + a Warning (never
    /// blocks). Only specs whose `matcher` matches `tool_name` run.
    pub async fn run_pre_tool_use(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> PreToolOutcome {
        let event = HookEvent::PreToolUse;
        let specs = self.matching_tool_specs(event, tool_name).await;
        if specs.is_empty() {
            return PreToolOutcome::Proceed(args.clone());
        }
        let mut current_val = args.clone();
        let mut current_str = serde_json::to_string(args).unwrap_or_default();
        for spec in &specs {
            let dctx = DispatchContext {
                session_id: ctx.session_id,
                cwd: ctx.cwd,
                tool: Some(dispatch::ToolDispatch {
                    tool_name,
                    tool_input: &current_val,
                    tool_result: None,
                    is_error: None,
                }),
            };
            match dispatch::run_hook(spec, event, &current_str, &dctx, Some(tx)).await {
                HookOutcome::Mutated { updated, .. } => {
                    // `updatedInput` must parse back into a JSON args object;
                    // a malformed rewrite is a soft failure (keep prior args).
                    match serde_json::from_str::<serde_json::Value>(&updated) {
                        Ok(v) => {
                            current_val = v;
                            current_str = updated;
                        }
                        Err(e) => {
                            emit_warning(
                                tx,
                                event,
                                &format!(
                                    "{}: ignoring malformed updatedInput ({e})",
                                    spec.display_name()
                                ),
                            )
                            .await;
                        }
                    }
                }
                HookOutcome::PassThrough { .. } => {}
                HookOutcome::Blocked { stderr, .. } => {
                    let trimmed = trim_one_line(&stderr);
                    emit_warning(
                        tx,
                        event,
                        &format!("{} blocked {tool_name}: {trimmed}", spec.display_name()),
                    )
                    .await;
                    return PreToolOutcome::Block(trimmed);
                }
                HookOutcome::SoftFailed { reason, .. } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
            }
        }
        PreToolOutcome::Proceed(current_val)
    }

    /// Run the `PostToolUse` chain after a tool ran (success or error). Hooks
    /// may rewrite the result the model sees (`updatedOutput`) or just observe.
    /// Cannot block — a `continue:false` degrades to a Warning (the tool already
    /// ran). Returns the possibly-rewritten result. Matcher applies.
    pub async fn run_post_tool_use(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        result: &str,
        is_error: bool,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> String {
        let event = HookEvent::PostToolUse;
        let specs = self.matching_tool_specs(event, tool_name).await;
        if specs.is_empty() {
            return result.to_string();
        }
        let mut current = result.to_string();
        for spec in &specs {
            let dctx = DispatchContext {
                session_id: ctx.session_id,
                cwd: ctx.cwd,
                tool: Some(dispatch::ToolDispatch {
                    tool_name,
                    tool_input: args,
                    tool_result: Some(&current),
                    is_error: Some(is_error),
                }),
            };
            match dispatch::run_hook(spec, event, &current, &dctx, Some(tx)).await {
                HookOutcome::Mutated { updated, .. } => current = updated,
                HookOutcome::PassThrough { .. } => {}
                HookOutcome::Blocked { stderr, .. } => {
                    // PostToolUse cannot block (the tool already ran); surface
                    // the attempt as a Warning and keep the current result.
                    emit_warning(
                        tx,
                        event,
                        &format!(
                            "{}: PostToolUse cannot block ({})",
                            spec.display_name(),
                            trim_one_line(&stderr)
                        ),
                    )
                    .await;
                    break;
                }
                HookOutcome::SoftFailed { reason, .. } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
            }
        }
        current
    }

    /// The specs for a tool `event` whose `matcher` applies to `tool_name`.
    async fn matching_tool_specs(&self, event: HookEvent, tool_name: &str) -> Vec<HookSpec> {
        let guard = self.inner.read().await;
        guard
            .for_event(event)
            .iter()
            .filter(|s| config::matches_tool(s.matcher.as_deref(), tool_name))
            .cloned()
            .collect()
    }

    /// Are any hooks declared for `event`? Fast path the render seam uses
    /// to skip task-spawn when there's nothing to do.
    pub async fn has_hooks(&self, event: HookEvent) -> bool {
        !self.inner.read().await.for_event(event).is_empty()
    }

    /// Cheap clone of the in-memory `HooksConfig`. Used by `/hooks` to
    /// render the currently-registered chains without re-reading disk.
    /// The list reflects the last `reload()` (or initial load) — it is
    /// not a live probe of the filesystem.
    pub async fn snapshot(&self) -> HooksConfig {
        self.inner.read().await.clone()
    }

    /// Format the current chains into a single multi-line notice. Holds
    /// the read lock only across the sync `format_list` call — no
    /// `.await` is nested, so the lock is dropped before this returns.
    /// Prefer this over `snapshot() + format_list(&cfg)` to avoid cloning
    /// two `Vec<HookSpec>` per call.
    pub async fn format_list(&self) -> String {
        let cfg = self.inner.read().await;
        format_list(&cfg)
    }

    /// `UserPromptSubmit` chain — same body as the render chain except
    /// that `Blocked` is honoured (returns `PromptHookResult::Blocked`)
    /// instead of being degraded to a Warning. Kept separate so the
    /// caller's return type carries the block decision at the type level.
    async fn run_prompt_chain(
        &self,
        initial: &str,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> PromptHookResult {
        let event = HookEvent::UserPromptSubmit;
        // Clone the small Vec of HookSpec out of the RwLock so we don't
        // hold the read guard across `.await` points in dispatch.
        let specs: Vec<HookSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return PromptHookResult::Continue(initial.to_string());
        }

        let dispatch_ctx = DispatchContext::new(ctx.session_id, ctx.cwd);

        let mut current = initial.to_string();
        for spec in &specs {
            let outcome = dispatch::run_hook(spec, event, &current, &dispatch_ctx, Some(tx)).await;
            match outcome {
                HookOutcome::Mutated { updated, .. } => current = updated,
                HookOutcome::PassThrough { .. } => {}
                HookOutcome::Blocked { stderr, .. } => {
                    // Honour the block. Spec: "Hook exits 2 → Block the
                    // turn (only event where blocking makes sense —
                    // UserPromptSubmit). Show stderr to user." Emit the
                    // warning here so the caller doesn't have to.
                    let trimmed = trim_one_line(&stderr);
                    emit_warning(
                        tx,
                        event,
                        &format!("blocked by {} (exit 2): {}", spec.display_name(), trimmed),
                    )
                    .await;
                    return PromptHookResult::Blocked { stderr: trimmed };
                }
                HookOutcome::SoftFailed { reason, .. } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
            }
        }
        PromptHookResult::Continue(current)
    }

    /// `AssistantMessageRender` chain — `Blocked` is degraded to a
    /// Warning because the assistant message already exists; the spec
    /// explicitly forbids losing it.
    async fn run_render_chain(
        &self,
        initial: &str,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> String {
        let event = HookEvent::AssistantMessageRender;
        let specs: Vec<HookSpec> = {
            let guard = self.inner.read().await;
            guard.for_event(event).to_vec()
        };
        if specs.is_empty() {
            return initial.to_string();
        }

        let dispatch_ctx = DispatchContext::new(ctx.session_id, ctx.cwd);

        let mut current = initial.to_string();
        for spec in &specs {
            let outcome = dispatch::run_hook(spec, event, &current, &dispatch_ctx, Some(tx)).await;
            match outcome {
                HookOutcome::Mutated { updated, .. } => current = updated,
                HookOutcome::PassThrough { .. } => {}
                HookOutcome::Blocked { stderr, .. } => {
                    emit_warning(
                        tx,
                        event,
                        &format!(
                            "{} requested block (ignored for render): {}",
                            spec.display_name(),
                            trim_one_line(&stderr)
                        ),
                    )
                    .await;
                }
                HookOutcome::SoftFailed { reason, .. } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    break;
                }
            }
        }
        current
    }
}

/// Outcome of running the `UserPromptSubmit` chain. Drives the caller's
/// decision in `Session::prompt` — `Continue` proceeds to history.push
/// and the model call; `Blocked` short-circuits the turn entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptHookResult {
    /// All hooks passed/rewrote, or the chain soft-failed; carry on with
    /// the (possibly-rewritten) prompt.
    Continue(String),
    /// A hook explicitly blocked (`exit 2` or `continue: false`). The
    /// warning has already been emitted to the event channel. The caller
    /// MUST NOT push the prompt to history or call the agent.
    Blocked { stderr: String },
}

/// Outcome of the `PreToolUse` chain. `Proceed` carries the (possibly
/// rewritten) args the permission gate + the tool then use; `Block` skips the
/// tool with a "blocked by hook" result. The warning is already emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolOutcome {
    Proceed(serde_json::Value),
    Block(String),
}

async fn emit_warning(tx: &EventSender, event: HookEvent, message: &str) {
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

/// Render the in-memory `HooksConfig` as a multi-line notice suitable for
/// `add_assistant_notice`. Always returns at least one line; the empty
/// case is rendered as a single `[info]` line so the user gets feedback
/// rather than a silent no-op. The list reflects the last `reload()` —
/// it is not a live probe of `~/.ignis/hooks.json`.
pub(crate) fn format_list(cfg: &HooksConfig) -> String {
    let total = cfg.total_len();
    if total == 0 {
        return "[info] no hooks registered · edit ~/.ignis/hooks.json, then /hooks reload"
            .to_string();
    }
    // Compute the widest display name across every chain so the program
    // column starts at a fixed offset regardless of how long any single
    // hook's filename is. `display_name()` is the file_stem — long
    // project-prefixed names like `translate-to-french-v2` would
    // otherwise push the program column right and break alignment for
    // shorter names in the same chain.
    let name_width = HookEvent::ALL
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
        "[info] {total} hook{total_plural} registered · /hooks reload to re-read · run unsandboxed; audit before installing:",
        total_plural = if total == 1 { "" } else { "s" },
    ));
    for event in HookEvent::ALL {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> HookContext<'static> {
        HookContext {
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
        let reg = HookRegistry::empty();
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("hello", ctx(), &tx).await;
        assert_eq!(out, PromptHookResult::Continue("hello".to_string()));
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

        let cfg = HooksConfig {
            user_prompt_submit: vec![
                HookSpec {
                    program: s1,
                    args: vec![],
                    timeout_ms: 5_000,
                    ..HookSpec::default()
                },
                HookSpec {
                    program: s2,
                    args: vec![],
                    timeout_ms: 5_000,
                    ..HookSpec::default()
                },
            ],
            assistant_message_render: vec![],
            ..Default::default()
        };
        let reg = HookRegistry::from_config(cfg);
        let (tx, _rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("A", ctx(), &tx).await;
        assert_eq!(out, PromptHookResult::Continue("STEP1!".to_string()));
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

        let cfg = HooksConfig {
            user_prompt_submit: vec![
                HookSpec {
                    program: good,
                    args: vec![],
                    timeout_ms: 5_000,
                    ..HookSpec::default()
                },
                HookSpec {
                    program: bad,
                    args: vec![],
                    timeout_ms: 5_000,
                    ..HookSpec::default()
                },
            ],
            assistant_message_render: vec![],
            ..Default::default()
        };
        let reg = HookRegistry::from_config(cfg);
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("A", ctx(), &tx).await;
        // Last good value is preserved.
        assert_eq!(out, PromptHookResult::Continue("GOOD".to_string()));
        drop(tx);
        // Exactly one warning emitted. Ignore the dispatcher's "hook runs
        // unconfined" notice, which fires only on platforms/kernels without an
        // enforceable sandbox (e.g. Landlock-less Linux, non-Linux) and would
        // otherwise make this assertion environment-dependent.
        let mut warnings = 0;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Warning { source, .. } = &ev {
                if source == "hook.sandbox" {
                    continue;
                }
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
        let cfg = HooksConfig {
            user_prompt_submit: vec![HookSpec {
                program: blocker,
                args: vec![],
                timeout_ms: 5_000,
                ..HookSpec::default()
            }],
            assistant_message_render: vec![],
            ..Default::default()
        };
        let reg = HookRegistry::from_config(cfg);
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("original", ctx(), &tx).await;
        match out {
            PromptHookResult::Blocked { stderr } => {
                assert!(stderr.contains("leaks secret"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        drop(tx);
        let mut warnings = 0;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Warning { source, message } = ev {
                // Skip the environment-dependent unconfined-sandbox notice.
                if source == "hook.sandbox" {
                    continue;
                }
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
        let cfg = HooksConfig {
            user_prompt_submit: vec![],
            assistant_message_render: vec![HookSpec {
                program: blocker,
                args: vec![],
                timeout_ms: 5_000,
                ..HookSpec::default()
            }],
            ..Default::default()
        };
        let reg = HookRegistry::from_config(cfg);
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_assistant_message_render("kept", ctx(), &tx).await;
        assert_eq!(out, "kept");
        drop(tx);
        let mut warnings = 0;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Warning { source, .. } = ev {
                // Skip the environment-dependent unconfined-sandbox notice.
                if source == "hook.sandbox" {
                    continue;
                }
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
        let reg = HookRegistry::from_config_dir(&home).unwrap();
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
        let cfg = HooksConfig {
            user_prompt_submit: vec![HookSpec {
                program: PathBuf::from("/a/hook.sh"),
                args: vec!["--flag".to_string()],
                timeout_ms: 7_000,
                ..HookSpec::default()
            }],
            assistant_message_render: vec![],
            ..Default::default()
        };
        let reg = HookRegistry::from_config(cfg.clone());
        let snap = reg.snapshot().await;
        assert_eq!(snap, cfg);
    }

    #[test]
    fn format_list_empty_registry_prompts_to_install() {
        let out = format_list(&HooksConfig::default());
        assert!(out.starts_with("[info]"));
        assert!(out.contains("no hooks registered"));
        // The hint mentions both the file path and the reload action so a
        // user who just typed `/hooks` knows what to do next.
        assert!(out.contains("~/.ignis/hooks.json"));
        assert!(out.contains("/hooks reload"));
    }

    #[test]
    fn format_list_one_hook_per_event_uses_singular_wording() {
        let cfg = HooksConfig {
            user_prompt_submit: vec![HookSpec {
                program: PathBuf::from("/opt/translate/run.py"),
                args: vec![],
                timeout_ms: 10_000,
                ..HookSpec::default()
            }],
            assistant_message_render: vec![],
            ..Default::default()
        };
        let out = format_list(&cfg);
        assert!(out.contains("1 hook registered "), "got: {out}");
        assert!(!out.contains("1 hooks"), "got: {out}");
        assert!(out.contains("UserPromptSubmit (1):"));
        assert!(out.contains("translate")); // file_stem of run.py
        assert!(out.contains("/opt/translate/run.py"));
        assert!(out.contains("timeout 10000ms"));
        // No args tail when argv is empty.
        assert!(!out.contains("--"));
    }

    #[test]
    fn format_list_multi_hook_renders_each_chain_and_argv() {
        let cfg = HooksConfig {
            user_prompt_submit: vec![
                HookSpec {
                    program: PathBuf::from("/opt/translate/run.py"),
                    args: vec!["--source".to_string(), "en".to_string()],
                    timeout_ms: 30_000,
                    ..HookSpec::default()
                },
                HookSpec {
                    program: PathBuf::from("/opt/redact.sh"),
                    args: vec![],
                    timeout_ms: 5_000,
                    ..HookSpec::default()
                },
            ],
            assistant_message_render: vec![HookSpec {
                program: PathBuf::from("/opt/translate/run.py"),
                args: vec![],
                timeout_ms: 10_000,
                ..HookSpec::default()
            }],
            ..Default::default()
        };
        let out = format_list(&cfg);
        // Plural wording, both event headers, both hooks in the prompt
        // chain, the single render hook, and the argv tail all appear.
        assert!(out.contains("3 hooks registered "), "got: {out}");
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
        let cfg = HooksConfig {
            user_prompt_submit: vec![],
            assistant_message_render: vec![HookSpec {
                program: PathBuf::from("/opt/render.sh"),
                args: vec![],
                timeout_ms: 1_000,
                ..HookSpec::default()
            }],
            ..Default::default()
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
        let cfg = HooksConfig {
            user_prompt_submit: vec![
                HookSpec {
                    program: PathBuf::from("/opt/translate-to-french-v2.py"),
                    args: vec![],
                    timeout_ms: 10_000,
                    ..HookSpec::default()
                },
                HookSpec {
                    program: PathBuf::from("/opt/redact.sh"),
                    args: vec![],
                    timeout_ms: 10_000,
                    ..HookSpec::default()
                },
            ],
            assistant_message_render: vec![],
            ..Default::default()
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
        // variant to `HookEvent::ALL` automatically appears in the
        // listing without touching this function.
        assert_eq!(HookEvent::ALL.len(), 4);
        let cfg = HooksConfig {
            user_prompt_submit: vec![HookSpec {
                program: PathBuf::from("/a"),
                args: vec![],
                timeout_ms: 1_000,
                ..HookSpec::default()
            }],
            assistant_message_render: vec![HookSpec {
                program: PathBuf::from("/b"),
                args: vec![],
                timeout_ms: 1_000,
                ..HookSpec::default()
            }],
            pre_tool_use: vec![HookSpec {
                program: PathBuf::from("/c"),
                args: vec![],
                timeout_ms: 1_000,
                ..HookSpec::default()
            }],
            ..Default::default()
        };
        let out = format_list(&cfg);
        assert!(out.contains("UserPromptSubmit (1):"));
        assert!(out.contains("AssistantMessageRender (1):"));
        assert!(out.contains("PreToolUse (1):"));
    }
}
