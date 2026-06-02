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

    /// Run the `UserPromptSubmit` chain. Each hook's `updatedInput` feeds
    /// the next hook's `prompt`; soft-failures stop the chain at the last
    /// good value and emit a `Warning` event. Returns the final string —
    /// equal to the input when no hook mutated it.
    pub async fn run_user_prompt_submit(
        &self,
        prompt: &str,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> String {
        self.run_chain(HookEvent::UserPromptSubmit, prompt, ctx, tx)
            .await
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
        self.run_chain(HookEvent::AssistantMessageRender, content, ctx, tx)
            .await
    }

    /// Are any hooks declared for `event`? Fast path the render seam uses
    /// to skip task-spawn when there's nothing to do.
    pub async fn has_hooks(&self, event: HookEvent) -> bool {
        !self.inner.read().await.for_event(event).is_empty()
    }

    async fn run_chain(
        &self,
        event: HookEvent,
        initial: &str,
        ctx: HookContext<'_>,
        tx: &EventSender,
    ) -> String {
        // Clone the small Vec of HookSpec out of the RwLock so we don't
        // hold the read guard across `.await` points in dispatch.
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
                HookOutcome::Blocked { stderr } => match event {
                    HookEvent::UserPromptSubmit => {
                        // Honour the block: keep the last good value, warn,
                        // stop the chain.
                        emit_warning(
                            tx,
                            event,
                            &format!(
                                "blocked by {} (exit 2): {}",
                                spec.display_name(),
                                trim_one_line(&stderr)
                            ),
                        )
                        .await;
                        break;
                    }
                    HookEvent::AssistantMessageRender => {
                        // Render side: blocking a message we already
                        // produced would lose it. Degrade to soft failure.
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
                },
                HookOutcome::SoftFailed { reason } => {
                    emit_warning(tx, event, &format!("{} ({})", reason, spec.display_name())).await;
                    // Spec: "Soft failure mid-chain returns last good
                    // value, emits Warning." Stop the chain so we don't
                    // pile up duplicate warnings if a misconfigured hook
                    // fails for every entry.
                    break;
                }
            }
        }
        current
    }
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
    if one.len() > 200 {
        format!("{}…", &one[..200])
    } else {
        one
    }
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
        assert_eq!(out, "hello");
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
        assert_eq!(out, "STEP1!");
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
        assert_eq!(out, "GOOD");
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
