//! Console event loop. Owns the terminal handle, the background-agent
//! channels (`prompt_tx`/`agent_rx`/`cancel_tx`/`picker_tx`/inject), and the
//! per-frame draw + key-poll cycle. Everything stateful about the live UI
//! flows through here; `App` is the in-memory model the loop drives.
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::console::app::App;
use crate::console::format::AgentRequest;
use crate::console::inline_picker;
use crate::console::keys::{handle_key, ActiveInject};
use crate::console::render::{self, draw};
use crate::{AgentEvent, Message, Session};

/// Create a fullscreen (alternate-screen) terminal over stdout. ratatui's
/// default Viewport::Fullscreen owns the whole frame and reflows on resize
/// automatically — no per-band height bookkeeping, no rebuild loop.
fn make_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::new(backend)
}

/// RAII guard for the alternate-screen + raw-mode pair. On drop (Err-bubble,
/// panic, or clean exit) it restores the user's prior terminal state.
/// Without this, any `?` after `EnterAlternateScreen` would strand the user
/// in an empty alt screen with no way back to their shell prompt.
struct TerminalGuard;

impl TerminalGuard {
    fn install() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        // Panic hook: a panic inside ratatui / the main loop would otherwise
        // print its message into the alt-screen the user can't see. Restore
        // the terminal first, then chain through to the prior hook.
        let prior_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableMouseCapture,
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            prior_hook(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}
#[allow(clippy::too_many_arguments)]
pub async fn run_console(
    provider_name: String,
    model_name: String,
    session_id: String,
    system_prompt: String,
    storage_dir: std::path::PathBuf,
    cwd: PathBuf,
    config: crate::config::Config,
    skill_registry: std::sync::Arc<crate::skills::SkillRegistry>,
    mcp_registry: std::sync::Arc<crate::mcp::McpRegistry>,
    permissions: std::sync::Arc<crate::permissions::runtime::PermissionState>,
    hook_registry: crate::hooks::HookRegistry,
) -> Result<(), anyhow::Error> {
    let mut app = App::new(provider_name, model_name, session_id, cwd.clone());
    // Context windows: config override → cached models.dev → compaction threshold.
    // The cache loads instantly; refresh runs in the background for next launch.
    let catalog = crate::llm::catalog::load();
    app.fallback_context_window = config.compaction.threshold_tokens;
    app.set_context_window(
        config
            .active_context(&catalog)
            .map(|c| c as usize)
            .unwrap_or(config.compaction.threshold_tokens),
    );
    app.set_model_options(config.model_options(&catalog), config.active_effort());
    tokio::spawn(crate::llm::catalog::refresh_if_stale());

    // Render fullscreen in the alternate-screen buffer: the whole terminal is
    // ours, native scrollback is preserved across the session, and quit/exit
    // restores whatever was on the user's terminal before launch. Transcript
    // history lives in `app.transcript` and scrolls in-app. `_term_guard`
    // restores the user's terminal on early-return or panic.
    let _term_guard = TerminalGuard::install()?;
    let mut terminal = make_terminal()?;

    // Welcome banner: first lines of the in-app transcript. Scrolls with the
    // conversation rather than being pinned chrome.
    app.commit_transcript_lines(render::welcome_lines(&app));

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (prompt_tx, mut prompt_rx) = mpsc::channel::<AgentRequest>(8);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(8);
    // Tool → console: the `ask_user` tool sends a PickerRequest when the model
    // wants to ask the user something mid-turn. Capacity 4 — pickers serialize
    // (one open at a time); the buffer just decouples send from console drain.
    let (picker_tx, mut picker_rx) = mpsc::channel::<crate::console::picker::PickerRequest>(4);
    let picker_tx_runner = picker_tx.clone();
    // Picker reply confirmation channel: handlers that run in `tokio::spawn`
    // (telemetry, AFK) can't reach `app.add_assistant_notice` directly, so
    // they send the confirm string here and the main loop drains it.
    let (notice_tx, mut notice_rx) = mpsc::channel::<String>(8);
    // AssistantMessageRender hook chain runs on a single per-session
    // worker task that drains a bounded queue serially — so two
    // back-to-back MessageEnds with different hook latencies always
    // commit their rewrites in the order they arrived. Rewrite events
    // ride the same `agent_tx` the live UI consumes, so scrollback
    // ordering follows event arrival.
    let render_hook_queue = spawn_render_hook_worker(hook_registry.clone(), agent_tx.clone());
    let active_inject: ActiveInject = std::sync::Arc::new(std::sync::Mutex::new(None));
    let active_inject_runner = active_inject.clone();

    let agent_system_prompt = system_prompt;
    let agent_storage_dir = storage_dir.clone();
    let ui_storage_dir = storage_dir;
    let agent_cwd = cwd;
    let mut agent_config = config;
    let runner_hook_registry = hook_registry.clone();
    app.hooks = Some(hook_registry);

    let ui_skill_registry = skill_registry.clone();
    let runner_skill_registry = skill_registry.clone();
    app.skills = Some(ui_skill_registry);

    let runner_mcp_registry = mcp_registry.clone();
    app.mcp = Some(mcp_registry);

    let runner_permissions = permissions.clone();
    app.permissions = Some(permissions);

    // Auto-update check: fire-and-forget HTTP GET in the background; the
    // event loop polls the oneshot via try_recv and sets `app.update_notice`
    // when it lands. Skip gate (env opt-out, CI, stderr-not-TTY, debug build,
    // unsupported target) lives in cli::upgrade::should_check_for_update.
    let mut update_check_rx = if crate::cli::upgrade::should_check_for_update() {
        Some(crate::cli::upgrade::spawn_update_check())
    } else {
        None
    };

    // Background agent runner
    tokio::spawn(async move {
        while let Some(request) = prompt_rx.recv().await {
            let (session_id, prompt) = match request {
                AgentRequest::Prompt { session_id, prompt } => (session_id, Some(prompt)),
                AgentRequest::Compact { session_id } => (session_id, None),
                AgentRequest::SetModel {
                    provider,
                    model,
                    effort,
                } => {
                    // Apply to the config the runner rebuilds the provider from;
                    // the next prompt picks it up. No session work needed.
                    agent_config.model = Some(format!("{provider}/{model}"));
                    agent_config.reasoning_effort = effort;
                    continue;
                }
                AgentRequest::ReloadConfig => {
                    // /connect just wrote a fresh `[providers.X] api_key = …`
                    // to disk; re-read it so the next prompt resolves with the
                    // new key. A read failure leaves the in-memory config as
                    // it was — but log loudly: the user will hit a stale-config
                    // error on their next prompt and the log is the only
                    // breadcrumb explaining why.
                    match crate::config::load_config() {
                        Ok(reloaded) => agent_config = reloaded,
                        Err(e) => log::error!("ReloadConfig: failed to re-read config.toml: {e}"),
                    }
                    continue;
                }
            };
            let provider = match crate::config::build_provider(&agent_config) {
                Ok(p) => p,
                Err(e) => {
                    let _ = agent_tx.send(AgentEvent::TurnEnd).await;
                    log::error!("Provider error: {}", e);
                    continue;
                }
            };
            let storage = crate::storage::FileStorage::new(agent_storage_dir.clone());
            let mut session = match Session::open(
                session_id,
                agent_system_prompt.clone(),
                provider,
                Box::new(storage),
                agent_cwd.to_string_lossy().to_string(),
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    let _ = agent_tx.send(AgentEvent::TurnEnd).await;
                    log::error!("Session open error: {}", e);
                    continue;
                }
            };
            session.set_compaction(agent_config.compaction.clone());
            // Share the runner's HookRegistry handle so `/hooks reload`
            // immediately affects the next prompt — Session::open loaded
            // its own copy from disk, but the runner owns the live one.
            session.set_hook_registry(runner_hook_registry.clone());

            let mcp_for_subagent = if !runner_mcp_registry.is_empty() {
                Some(runner_mcp_registry.clone())
            } else {
                None
            };
            crate::tools::register_native_tools_with_mcp(
                &mut session,
                &agent_cwd,
                &agent_config,
                mcp_for_subagent,
                Some(picker_tx_runner.clone()),
                Some(runner_permissions.clone()),
            );
            if !runner_skill_registry.is_empty() {
                session.set_skills(runner_skill_registry.clone());
                session.register_tool(std::sync::Arc::new(crate::tools::SkillTool::new(
                    runner_skill_registry.clone(),
                )));
            }
            if !runner_mcp_registry.is_empty() {
                session.set_mcp(runner_mcp_registry.clone());
                crate::tools::register_mcp_tools(&mut session, &runner_mcp_registry);
            }

            // Permission gate. The TUI's picker channel is wired in so an
            // `Ask` decision opens the 3-option permission picker (Approve
            // once / Approve session / Deny) over the same plumbing
            // `ask_user` uses; on `Approve session` the checker writes back
            // into the shared `PermissionState`.
            session.set_hooks(Box::new(
                crate::permissions::checker::PermissionChecker::new(runner_permissions.clone())
                    .with_picker(picker_tx_runner.clone()),
            ));

            let notice_msg = |content: &str| Message {
                role: "assistant".to_string(),
                content: Some(content.to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            };
            let tx = agent_tx.clone();
            match prompt {
                Some(prompt) => {
                    // Discard any cancel that arrived after the previous turn
                    // already ended (its end-of-turn window) — it must not cancel
                    // this fresh prompt.
                    while cancel_rx.try_recv().is_ok() {}
                    let (inj_tx, inj_rx) = mpsc::channel::<String>(8);
                    *active_inject_runner.lock().unwrap() = Some(inj_tx);
                    session.set_inject_source(inj_rx);
                    tokio::select! {
                        result = session.prompt(&prompt, tx) => {
                            if let Err(e) = result {
                                let _ = agent_tx.send(AgentEvent::TurnEnd).await;
                                log::error!("Agent error: {}", e);
                            }
                        }
                        _ = cancel_rx.recv() => {
                            let _ = agent_tx.send(AgentEvent::TurnEnd).await;
                        }
                    }
                    *active_inject_runner.lock().unwrap() = None;
                }
                None => {
                    // /compact: summarize earlier history and report a notice.
                    let _ = agent_tx.send(AgentEvent::TurnStart).await;
                    let notice = match session.compact().await {
                        Ok(0) => "Nothing to compact yet.".to_string(),
                        Ok(n) => format!("Compacted {n} earlier messages into a summary."),
                        Err(e) => format!("Compact failed: {e}"),
                    };
                    let _ = agent_tx
                        .send(AgentEvent::MessageStart {
                            message: notice_msg(""),
                        })
                        .await;
                    let _ = agent_tx
                        .send(AgentEvent::MessageUpdate {
                            delta: notice.clone(),
                        })
                        .await;
                    let _ = agent_tx
                        .send(AgentEvent::MessageEnd {
                            message: notice_msg(&notice),
                        })
                        .await;
                    let _ = agent_tx.send(AgentEvent::TurnEnd).await;
                }
            }
        }
    });

    // Render at a capped frame rate. Agent events and keystrokes are coalesced
    // between frames and the screen is redrawn at most once per frame, so a fast
    // token stream never triggers a redraw per delta — which tears/flickers on
    // slow terminals (e.g. Windows Terminal over WSL2).
    let frame = std::time::Duration::from_millis(33); // ~30fps cap
    let mut last_draw = std::time::Instant::now();
    terminal.draw(|f| draw(f, &mut app))?;

    loop {
        // Wake on the next frame deadline, an agent event, or an `ask_user`
        // picker request from a tool.
        tokio::select! {
            _ = tokio::time::sleep(frame) => {}
            Some(ev) = agent_rx.recv() => {
                enqueue_render_hook(
                    &ev,
                    &render_hook_queue,
                    &app.session_id,
                    &app.cwd,
                );
                app.handle_event(ev);
            }
            Some(req) = picker_rx.recv() => {
                if app.inline_picker.is_some() {
                    // One picker at a time — reject the second so the tool
                    // returns an error instead of stalling.
                    let _ = req.reply.send(crate::console::picker::PickerResponse::Cancelled);
                } else {
                    app.inline_picker = Some(inline_picker::InlinePickerState::new(req));
                }
            }
        }

        // Drain any other pending agent events and key input — state only, no draw.
        while let Ok(ev) = agent_rx.try_recv() {
            enqueue_render_hook(&ev, &render_hook_queue, &app.session_id, &app.cwd);
            app.handle_event(ev);
        }
        // Drain picker-spawn notices into the transcript.
        while let Ok(msg) = notice_rx.try_recv() {
            app.add_assistant_notice(msg);
        }
        // Poll the auto-update-check oneshot. Resolves once (Ok or Closed),
        // after which we drop the receiver so the branch goes dormant.
        if let Some(rx) = &mut update_check_rx {
            match rx.try_recv() {
                Ok(notice) => {
                    app.update_notice = notice;
                    update_check_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    update_check_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
        while let Ok(req) = picker_rx.try_recv() {
            if app.inline_picker.is_some() {
                let _ = req
                    .reply
                    .send(crate::console::picker::PickerResponse::Cancelled);
            } else {
                app.inline_picker = Some(inline_picker::InlinePickerState::new(req));
            }
        }

        // Edge-triggered: exactly one queued line per turn-end (TurnEnd).
        // Route through the same dispatcher Enter uses so queued slash
        // commands (`/compact`, `/model`, …) actually execute — sending the
        // text as a raw `AgentRequest::Prompt` would deliver "/compact" to
        // the LLM as a user message. The user block is rendered when
        // `Session::prompt` emits `UserPromptCommitted` (post-hook), so we
        // don't push it here.
        if app.take_turn_just_ended() {
            // Control returned to the user — refresh the footer branch so a
            // `git checkout` the agent ran this turn is reflected (oh-my-zsh
            // recomputes per prompt; same idea).
            app.refresh_git_branch();
            if let Some(text) = app.take_queued_front() {
                crate::console::keys::submit_text(
                    &mut app,
                    text,
                    &prompt_tx,
                    &picker_tx,
                    &notice_tx,
                    &ui_storage_dir,
                )
                .await;
            }
        }

        while event::poll(std::time::Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => {
                    handle_key(
                        &mut app,
                        key,
                        &prompt_tx,
                        &cancel_tx,
                        &active_inject,
                        &ui_storage_dir,
                        &picker_tx,
                        &notice_tx,
                    )
                    .await;
                }
                Event::Paste(data) => crate::console::keys::handle_paste(&mut app, data),
                // Mouse wheel scrolls the transcript (3 lines/notch). ScrollUp
                // releases auto-follow; ScrollDown re-enables it at the bottom.
                Event::Mouse(me) => match me.kind {
                    MouseEventKind::ScrollUp => app.scroll_transcript_up(3),
                    MouseEventKind::ScrollDown => {
                        let visible = app.transcript_visible_rows.max(1);
                        app.scroll_transcript_down(3, visible);
                    }
                    _ => {}
                },
                // A SIGWINCH reaches us as Resize — fired on a real resize AND
                // on a tmux/screen detach→reattach, which re-sends the window
                // size even when it's unchanged. ratatui's autoresize only
                // repaints when the size *changed*, so a same-size reattach
                // would leave the alt-screen blank (history "lost"). Force a
                // full redraw from `app.transcript` on the next draw.
                Event::Resize(_, _) => terminal.clear()?,
                _ => {}
            }
        }

        if app.should_quit {
            break;
        }

        // Fullscreen viewport reflows on resize automatically — no rebuild loop.
        // Append newly-finalized blocks to the in-app transcript buffer.
        let width = terminal.size()?.width;
        while app.committed < app.blocks.len() && app.block_done(app.committed) {
            let lines = render::block_lines(&app.blocks[app.committed], app.tick, &app.cwd, width);
            if !lines.is_empty() {
                app.commit_transcript_lines(lines);
            }
            app.committed += 1;
        }

        // The `ask_user` trace is committed via the usual block flush above
        // (block_lines special-cases UIBlock::Tool{name:"ask_user"} into a
        // compact trace via ask_user_resume_trace). No separate live-flush
        // path — that used to double-emit.

        // Coalesced redraw of the live band: at most once per frame interval.
        if last_draw.elapsed() >= frame {
            app.tick_update();
            terminal.draw(|f| draw(f, &mut app))?;
            last_draw = std::time::Instant::now();
        }
    }

    // The terminal cursor was hidden by ratatui at construction; restore it
    // before the guard drops so it's visible after exit. `_term_guard`
    // (TerminalGuard::install above) then runs LeaveAlternateScreen +
    // disable_raw_mode on the way out, covering this clean-exit path, all
    // `?`-bubbled Err returns, and panics in the main loop.
    terminal.show_cursor()?;
    Ok(())
}

/// Prefix used to label the assistant block that carries an
/// `AssistantMessageRender`-hook rewrite. Doubles as the gate that keeps
/// the render seam from re-processing its own output (see
/// `enqueue_render_hook`).
const HOOK_REWRITE_PREFIX: &str = "[hook rewrite]";

/// One unit of work for the render-hook queue: a single assistant
/// `MessageEnd`'s text plus the context the hook needs.
pub(crate) struct RenderJob {
    pub content: String,
    pub session_id: String,
    pub cwd: String,
}

/// Inspect an incoming event; when it's an assistant `MessageEnd`
/// carrying *final assistant text* (i.e. not a reasoning block), submit
/// a [`RenderJob`] to the per-session render-hook queue so the
/// `AssistantMessageRender` chain runs in submission order.
///
/// Two filters per spec & PR review:
///   1. **Skip reasoning blocks.** Each `MessageEnd` for the same turn
///      can carry a reasoning-only `Message` (✻ Thinking) before the
///      final assistant text. The translator hook should NOT see those
///      — they aren't shown to the user as the assistant's reply, and
///      running translation on them is both wrong and wasteful.
///   2. **Skip our own rewrite blocks.** The rewrite we emit lands as
///      another `MessageEnd`; we mustn't loop on it.
///
/// Design choice (per spec's render-seam section): a true *swap* of the
/// already-committed scrollback block isn't trivial — committed
/// `Line`s in `app.transcript` are styled and indexed by `app.committed`.
/// Re-rendering one block in place would require tracking its start/end
/// row, undoing the auto-follow scroll, and re-flowing the next blocks.
/// The spec explicitly allows the fallback: render the rewrite as a new
/// labeled block "below the original" and document the choice. That's
/// what we do — the assistant block produced by the model commits as
/// usual, then a follow-up `[hook rewrite] <rewritten>` block lands
/// underneath. History stores only the model's original text (see
/// `Agent::run`'s `history.push`), so prompt cache + replay stay exact.
fn enqueue_render_hook(
    ev: &crate::AgentEvent,
    queue_tx: &mpsc::Sender<RenderJob>,
    session_id: &str,
    cwd: &std::path::Path,
) {
    use crate::AgentEvent;
    let message = match ev {
        AgentEvent::MessageEnd { message } => message,
        _ => return,
    };
    // Reasoning-only blocks (`reasoning_content` set, `content` empty
    // or absent) are not the assistant's reply — the translator hook
    // must skip them. Some providers emit BOTH reasoning_content and
    // content in the same MessageEnd; that's a real assistant turn so
    // we keep it.
    if message
        .reasoning_content
        .as_deref()
        .is_some_and(|r| !r.is_empty())
        && message.content.as_deref().is_none_or(str::is_empty)
    {
        return;
    }
    let content = match message.content.as_deref() {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => return,
    };
    // Don't re-process our own rewrite blocks — they're the *output* of
    // the hook chain, not a fresh assistant turn.
    if content.starts_with(HOOK_REWRITE_PREFIX) {
        return;
    }
    // Drop if the queue is full — the worker is presumably stuck on a
    // slow hook; dropping a single render is better than blocking the
    // whole event loop. Render hook is best-effort by design.
    let _ = queue_tx.try_send(RenderJob {
        content,
        session_id: session_id.to_string(),
        cwd: cwd.to_string_lossy().to_string(),
    });
}

/// Spawn the per-session render-hook worker. Owns a single
/// `mpsc::Receiver<RenderJob>` and processes jobs in submission order so
/// concurrent hook latencies don't reorder rewrites (which previously
/// happened because every `MessageEnd` fired a fresh `tokio::spawn`).
/// The worker exits when the queue's sender side drops at console exit.
pub(crate) fn spawn_render_hook_worker(
    registry: crate::hooks::HookRegistry,
    event_tx: mpsc::Sender<crate::AgentEvent>,
) -> mpsc::Sender<RenderJob> {
    // Bounded; if hooks back up the producer drops via try_send rather
    // than stall the event loop (see `enqueue_render_hook`).
    let (queue_tx, mut queue_rx) = mpsc::channel::<RenderJob>(16);
    tokio::spawn(async move {
        while let Some(job) = queue_rx.recv().await {
            // Fast path: no hooks declared → drop straight through. The
            // check is cheap (one read-lock guard length check) and
            // saves the envelope encode + subprocess spawn on the
            // overwhelmingly common no-hook path.
            if !registry
                .has_hooks(crate::hooks::HookEvent::AssistantMessageRender)
                .await
            {
                continue;
            }
            let ctx = crate::hooks::HookContext {
                session_id: &job.session_id,
                cwd: &job.cwd,
            };
            let rewritten = registry
                .run_assistant_message_render(&job.content, ctx, &event_tx)
                .await;
            if rewritten == job.content {
                continue;
            }
            let labeled = format!("{HOOK_REWRITE_PREFIX}\n{rewritten}");
            let msg = crate::Message {
                role: "assistant".to_string(),
                content: Some(labeled.clone()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            };
            let _ = event_tx
                .send(crate::AgentEvent::MessageStart {
                    message: msg.clone(),
                })
                .await;
            let _ = event_tx
                .send(crate::AgentEvent::MessageUpdate { delta: labeled })
                .await;
            let _ = event_tx
                .send(crate::AgentEvent::MessageEnd { message: msg })
                .await;
        }
    });
    queue_tx
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hooks::{HookRegistry, HookSpec, HooksConfig};
    use crate::{AgentEvent, Message};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
        p
    }

    fn assistant_msg(content: &str, reasoning: Option<&str>) -> Message {
        Message {
            role: "assistant".to_string(),
            content: Some(content.to_string()),
            reasoning_content: reasoning.map(str::to_string),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }
    }

    #[tokio::test]
    async fn render_hook_skips_reasoning_only_messages() {
        // Reasoning-only MessageEnd MUST NOT trigger the render hook —
        // the translator hook is for the assistant's *reply* text, not
        // the ✻ Thinking block (it isn't shown as the assistant's
        // visible answer, and running translation on it is both wrong
        // and wasteful).
        let (queue_tx, mut queue_rx) = mpsc::channel::<RenderJob>(4);
        let ev = AgentEvent::MessageEnd {
            message: Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: Some("thinking out loud".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        };
        enqueue_render_hook(&ev, &queue_tx, "s", Path::new("/tmp"));
        // Same shape, but reasoning_content set + content empty string.
        let ev2 = AgentEvent::MessageEnd {
            message: Message {
                role: "assistant".to_string(),
                content: Some("".to_string()),
                reasoning_content: Some("more thinking".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        };
        enqueue_render_hook(&ev2, &queue_tx, "s", Path::new("/tmp"));
        // A real assistant text DOES enqueue.
        let real = AgentEvent::MessageEnd {
            message: assistant_msg("hello user", None),
        };
        enqueue_render_hook(&real, &queue_tx, "s", Path::new("/tmp"));
        drop(queue_tx);
        // Expect exactly ONE job — the assistant text.
        let first = queue_rx.recv().await.expect("one job enqueued");
        assert_eq!(first.content, "hello user");
        assert!(queue_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn render_hook_preserves_order_when_first_hook_is_slower() {
        // Spec from review: two MessageEnd events with different hook
        // latencies must commit their rewrites in the order they
        // arrived. Previously every event spawned its own task, so a
        // slow first hook could land AFTER a fast second one.
        //
        // We use ONE hook whose body sleeps based on the prompt: the
        // first call sleeps longer than the second. With the per-
        // session queue, jobs drain serially — outputs land in order.
        let tmp = crate::util::unique_temp_dir("ignis-render-order");
        // Reads prompt, sleeps proportional to its length, emits rewrite
        // that prefixes the prompt with the round number.
        let script = write_script(
            &tmp,
            "ordered.sh",
            r#"#!/bin/sh
RAW=$(cat)
CONTENT=$(printf '%s' "$RAW" | sed -E 's/.*"content":"([^"]*)".*/\1/' )
case "$CONTENT" in
  slow*) sleep 0.3 ;;
  fast*) sleep 0.05 ;;
esac
printf '{"hookSpecificOutput":{"updatedOutput":"R:%s"}}' "$CONTENT"
"#,
        );
        let cfg = HooksConfig {
            user_prompt_submit: vec![],
            assistant_message_render: vec![HookSpec {
                program: script,
                args: vec![],
                timeout_ms: 5_000,
            }],
        };
        let registry = HookRegistry::from_config(cfg);
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
        let queue_tx = spawn_render_hook_worker(registry, event_tx);

        // First job is slow, second is fast — without the per-session
        // queue these would commit out-of-order.
        let slow = AgentEvent::MessageEnd {
            message: assistant_msg("slow-one", None),
        };
        let fast = AgentEvent::MessageEnd {
            message: assistant_msg("fast-two", None),
        };
        enqueue_render_hook(&slow, &queue_tx, "s", Path::new("/tmp"));
        enqueue_render_hook(&fast, &queue_tx, "s", Path::new("/tmp"));
        // Close the worker's input so it exits after draining.
        drop(queue_tx);

        // Collect the rewrite MessageEnd events in order.
        let mut rewrites = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            if let AgentEvent::MessageEnd { message } = ev {
                if let Some(c) = message.content {
                    if c.starts_with(HOOK_REWRITE_PREFIX) {
                        rewrites.push(c);
                    }
                }
            }
        }
        assert_eq!(rewrites.len(), 2, "two rewrites expected");
        // Submission order preserved: slow-one before fast-two.
        assert!(
            rewrites[0].contains("R:slow-one"),
            "first rewrite: {}",
            rewrites[0]
        );
        assert!(
            rewrites[1].contains("R:fast-two"),
            "second rewrite: {}",
            rewrites[1]
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
