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

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::RwLock;

use crate::AgentEvent;

pub use config::{HookSpec, HooksConfig, DEFAULT_TIMEOUT_MS};
pub use dispatch::{DispatchContext, HookOutcome};
pub use protocol::{HookEvent, HookInput, HookOutput, HookSpecificOutput};

/// Context the registry needs at every dispatch call. Borrowed strings so
/// callers don't have to allocate per turn.
#[derive(Debug, Clone, Copy)]
pub struct HookContext<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
}

/// Owned variant of [`HookContext`] for storage on long-lived structs
/// (e.g. a `Session` holds one across many turns). Borrow back via
/// [`OwnedHookContext::as_ref`] at dispatch time.
#[derive(Debug, Clone)]
pub struct OwnedHookContext {
    pub session_id: String,
    pub cwd: String,
}

impl OwnedHookContext {
    /// Borrow as a [`HookContext`] for one dispatch call.
    pub fn as_ref(&self) -> HookContext<'_> {
        HookContext {
            session_id: &self.session_id,
            cwd: &self.cwd,
        }
    }
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
    pub async fn reload(&self, home: &Path) -> anyhow::Result<usize> {
        let cfg = HooksConfig::from_home(home)?;
        let total = cfg.total_len();
        let mut guard = self.inner.write().await;
        *guard = cfg;
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

    /// Are any hooks declared for `event`? Fast path the render seam uses
    /// to skip task-spawn when there's nothing to do.
    pub async fn has_hooks(&self, event: HookEvent) -> bool {
        !self.inner.read().await.for_event(event).is_empty()
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

        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };

        let mut current = initial.to_string();
        for spec in &specs {
            let outcome = dispatch::run_hook(spec, event, &current, &dispatch_ctx).await;
            match outcome {
                HookOutcome::Mutated(next) => current = next,
                HookOutcome::PassThrough => {}
                HookOutcome::Blocked { stderr } => {
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
                HookOutcome::SoftFailed { reason } => {
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

        let dispatch_ctx = DispatchContext {
            session_id: ctx.session_id,
            cwd: ctx.cwd,
        };

        let mut current = initial.to_string();
        for spec in &specs {
            let outcome = dispatch::run_hook(spec, event, &current, &dispatch_ctx).await;
            match outcome {
                HookOutcome::Mutated(next) => current = next,
                HookOutcome::PassThrough => {}
                HookOutcome::Blocked { stderr } => {
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
                HookOutcome::SoftFailed { reason } => {
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
                },
                HookSpec {
                    program: s2,
                    args: vec![],
                    timeout_ms: 5_000,
                },
            ],
            assistant_message_render: vec![],
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
                },
                HookSpec {
                    program: bad,
                    args: vec![],
                    timeout_ms: 5_000,
                },
            ],
            assistant_message_render: vec![],
        };
        let reg = HookRegistry::from_config(cfg);
        let (tx, mut rx) = mpsc::channel(8);
        let out = reg.run_user_prompt_submit("A", ctx(), &tx).await;
        // Last good value is preserved.
        assert_eq!(out, PromptHookResult::Continue("GOOD".to_string()));
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
        let cfg = HooksConfig {
            user_prompt_submit: vec![HookSpec {
                program: blocker,
                args: vec![],
                timeout_ms: 5_000,
            }],
            assistant_message_render: vec![],
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
            }],
        };
        let reg = HookRegistry::from_config(cfg);
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
}
