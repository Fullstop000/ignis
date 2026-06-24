//! Console event loop. Owns the terminal handle, the background-agent
//! channels (`prompt_tx`/`agent_rx`/`cancel_tx`/`picker_tx`/inject), and the
//! per-frame draw + key-poll cycle. Everything stateful about the live UI
//! flows through here; `App` is the in-memory model the loop drives.
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{backend::CrosstermBackend, text::Line, Terminal, TerminalOptions, Viewport};
use std::io;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::console::app::{App, UIBlock};
use crate::console::format::AgentRequest;
use crate::console::frontend::{
    local_tui, Acceptor, ClientCommand, ClientRequest, CommandOutcome, ControlSignal, FrontendHub,
    Outbound, ReplyAnswer, RequestBroker, StdioPort,
};
use crate::console::inline_picker;
use crate::console::keys::{handle_key, ActiveInject};
use crate::console::render::anchor::{self, Anchor, ClearOutcome, Wipe};
use crate::console::render::{self, draw};
use crate::console::render_diag::RenderDiag;
use crate::storage::{FileStorage, SessionStorage};
use crate::{AgentEvent, Message, Session};

/// Create an inline-viewport terminal over stdout: a fixed `viewport_rows`-tall
/// live band pinned at the bottom of the normal buffer. Finalized conversation
/// blocks are pushed into the terminal's real scrollback via `insert_before`,
/// so native copy, native scroll, and tmux detach/reattach all work. The band
/// height is fixed per `Terminal`; it is rebuilt only when the band needs to
/// grow/shrink (e.g. a picker opens).
fn make_terminal(viewport_rows: u16) -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_rows.max(2)),
        },
    )
}

/// True for crossterm's transient "cursor position could not be read within a
/// normal duration" error. Re-anchoring the inline viewport (on a rebuild or a
/// resize) queries the cursor row via a DSR (`ESC[6n`) and waits up to ~2s for
/// the terminal to reply; under output backpressure on a slow pty (WSL2, tmux)
/// that reply can land late. It's recoverable — skip this frame's rebuild/draw
/// and retry next tick — so it must NOT be `?`-bubbled into tearing down the
/// whole TUI and losing the live session. `ErrorKind::Other` + the message text
/// is the only signal crossterm 0.27 gives; any other I/O error stays fatal.
fn transient_cursor_read_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string().contains("cursor position could not be read")
}

/// Rebuild the inline-viewport terminal at `rows`, tolerating the transient
/// cursor-read timeout. `Ok(true)` = rebuilt (caller commits the new size);
/// `Ok(false)` = the terminal didn't answer in time, so the old viewport is
/// kept and the caller should leave its size state untouched to retry next
/// frame; `Err` = a real I/O failure that should propagate.
fn try_rebuild(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rows: u16,
) -> io::Result<bool> {
    match make_terminal(rows) {
        Ok(t) => {
            *terminal = t;
            Ok(true)
        }
        Err(e) if transient_cursor_read_error(&e) => {
            log::warn!("inline viewport rebuild skipped (cursor read timeout): {e}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Draw the live band, tolerating the transient cursor-read timeout (a resize
/// racing `draw`'s autoresize can trigger a DSR). Swallows only that specific
/// timeout — every other draw error propagates.
fn draw_tolerant(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    match terminal.draw(|f| draw(f, app)) {
        Ok(_) => Ok(()),
        Err(e) if transient_cursor_read_error(&e) => {
            log::warn!("inline draw skipped (cursor read timeout): {e}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// RAII guard for raw-mode + bracketed paste. On drop (Err-bubble, panic, or
/// clean exit) it restores the user's prior terminal state. Inline rendering
/// stays in the *normal* buffer (no alternate screen) so the conversation
/// remains in native scrollback after exit; no mouse capture, so click-drag
/// selection works.
struct TerminalGuard;

impl TerminalGuard {
    fn install() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnableBracketedPaste)?;
        let prior_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableBracketedPaste);
            prior_hook(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste);
    }
}
/// The frontend-agnostic agent loop: dispatch `AgentRequest`s into `Session`
/// runs, streaming `AgentEvent`s out on `agent_tx`. Shared by the ratatui
/// runner and the headless `--engine` mode — it has no idea which frontend
/// (if any) is on the other end of `agent_tx`.
#[allow(clippy::too_many_arguments)]
async fn agent_loop(
    mut prompt_rx: mpsc::Receiver<AgentRequest>,
    mut cancel_rx: mpsc::Receiver<()>,
    agent_tx: mpsc::Sender<AgentEvent>,
    picker_tx_runner: mpsc::Sender<crate::console::picker::PickerRequest>,
    active_inject_runner: ActiveInject,
    mut agent_config: crate::config::Config,
    agent_system_prompt: String,
    agent_storage_dir: PathBuf,
    agent_cwd: PathBuf,
    runner_hook_registry: crate::hooks::HookRegistry,
    mut runner_skill_registry: std::sync::Arc<crate::skills::SkillRegistry>,
    runner_mcp_registry: std::sync::Arc<crate::mcp::McpRegistry>,
    runner_permissions: std::sync::Arc<crate::permissions::runtime::PermissionState>,
    // Background-shell registry, owned by the caller (run_console / run_engine)
    // so it can SIGKILL-all on the *reliable* shutdown path — this detached task
    // is never joined, so its own kill_all (below) is only a best-effort early
    // cleanup, not the guarantee.
    bg_shells: std::sync::Arc<crate::tools::BackgroundShells>,
) {
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
                // the next prompt picks it up. No session work needed. `effort`
                // is the frontend's authoritative pick (`Some(level)`, or `None`
                // when the model has no effort control); `build_provider`
                // re-validates it against the new model on the next prompt.
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
            AgentRequest::ReloadSkills(registry) => {
                // The user pressed `r` in the `/skills` picker. Adopt the
                // *same* registry the UI just built, so both share one `Arc`
                // — a later enable/disable toggle (interior mutability) is
                // then visible to the next prompt without another reload.
                runner_skill_registry = registry;
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
            Some(bg_shells.clone()),
            Some(agent_tx.clone()),
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
        // Re-sync the frontend's todo panel from the persisted list at the start
        // of each turn. Within a run Ink already holds it; after a reconnect
        // the panel is empty until something re-emits. (Resume paths now emit
        // directly — see the `send_transcript` sites — but this also covers
        // reconnect and is a cheap no-op when the list is already in step.)
        // Skip when empty (nothing to show; the panel stays hidden).
        {
            let todos = session.todos_handle().lock().unwrap().clone();
            if !todos.is_empty() {
                let _ = agent_tx.send(AgentEvent::Todos { items: todos }).await;
            }
        }
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
                    // Only a *real* cancel (`Some(())`) aborts the turn. When the
                    // frontend disconnects, the driver drops `cancel_tx`, so
                    // `recv()` resolves `None` — matching `_` here would cancel
                    // the in-flight turn mid-`persist()` and lose it (the turn the
                    // user just saw finish). Binding `Some(())` disables this arm
                    // on channel-close, letting `session.prompt` run to its save.
                    Some(()) = cancel_rx.recv() => {
                        let _ = agent_tx.send(AgentEvent::TurnEnd).await;
                    }
                }
                *active_inject_runner.lock().unwrap() = None;
            }
            None => {
                // /compact: summarize earlier history. On success the report
                // block (token reduction + full summary) replaces the old
                // generic notice; the notice is kept only for the no-op and
                // error cases so the user knows the command ran. Wrapped in
                // tokio::select! so Ctrl+C cancels the (potentially long) LLM
                // summarization — same pattern as the Some(prompt) arm.
                while cancel_rx.try_recv().is_ok() {}
                let _ = agent_tx.send(AgentEvent::TurnStart).await;
                let _ = agent_tx.send(AgentEvent::CompactStart).await;
                tokio::select! {
                    outcome = session.compact() => {
                        let _ = agent_tx.send(AgentEvent::CompactEnd).await;
                        let notice = match outcome {
                            Ok(o) if o.messages_replaced > 0 => {
                                let _ = agent_tx
                                    .send(AgentEvent::CompactReport {
                                        before: o.before_tokens,
                                        after: o.after_tokens,
                                        summary: o.summary,
                                    })
                                    .await;
                                None
                            }
                            Ok(_) => Some("Nothing to compact yet.".to_string()),
                            Err(e) => Some(format!("Compact failed: {e}")),
                        };
                        if let Some(notice) = notice {
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
                        }
                    }
                    // Only a real cancel (Some(())) aborts. Channel-close
                    // (None) lets compact finish so a frontend disconnect
                    // mid-compaction doesn't lose the work — same guard as
                    // the Some(prompt) arm.
                    Some(()) = cancel_rx.recv() => {
                        let _ = agent_tx.send(AgentEvent::CompactEnd).await;
                    }
                }
                let _ = agent_tx.send(AgentEvent::TurnEnd).await;
            }
        }
    }
    // Best-effort early cleanup when the prompt channel closes. The reliable
    // SIGKILL-all is in the caller (run_console / run_engine), which `main`
    // actually awaits — this detached task may not be scheduled before teardown.
    bg_shells.kill_all();
}

#[allow(clippy::too_many_arguments)]
pub async fn run_console(
    provider_name: Option<String>,
    model_name: Option<String>,
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
    // Apply the persisted `/settings` Statusline choices (hidden footer
    // segments) before the first render.
    app.statusline_hidden = crate::state::load_state().statusline_hidden;
    // Model windows: config override → cached models.dev → compaction threshold.
    // Each model's resolved window is baked into `model_options`; the footer
    // gauge derives the active one from there on demand (see `App::context_window`),
    // with the compaction threshold as the fallback. The cache loads instantly;
    // refresh runs in the background for next launch.
    let catalog = crate::llm::catalog::load();
    app.fallback_context_window = config.compaction.threshold_tokens;
    app.set_model_options(config.model_options(&catalog), config.active_effort());
    // Keep the catalog so the footer gauge can resolve an active model that
    // isn't in `model_options` (an un-baked, undeclared model only models.dev knows).
    app.model_catalog = catalog;
    tokio::spawn(crate::llm::catalog::refresh_if_stale());

    // Render inline in the normal buffer: finalized blocks are pushed into the
    // terminal's real scrollback, so native copy/scroll/tmux-reattach all work.
    // A fixed-height band stays pinned at the bottom. `_term_guard` restores raw
    // mode on early-return or panic.
    let _term_guard = TerminalGuard::install()?;
    let init_size = crossterm::terminal::size()?;
    let viewport_rows = render::viewport_height(&app, init_size.0, init_size.1);
    let mut terminal = make_terminal(viewport_rows)?;

    // Welcome banner: pushed into scrollback above the band, like any block.
    {
        let welcome = render::welcome_lines(&app);
        let h = welcome.len() as u16;
        if h > 0 {
            terminal.insert_before(h, |buf| render::render_block_into(buf, &welcome))?;
        }
    }

    // Startup resume (`ignis --resume <id>`, or `auto_resume_last_session`): if
    // this session id already has a persisted transcript, paint it into
    // scrollback so the user sees their prior conversation — not just new
    // turns. The agent already continues the right session (the runner's
    // `Session::open` reads the same JSONL), but the launch path never put it on
    // screen — the in-session `/sessions` path is the only place that called
    // `render_session_history`. A fresh session's id has no file, so this is a
    // no-op (and a quick miss) on a normal launch.
    let resumed_history = FileStorage::new(storage_dir.clone())
        .load_session(&app.session_id)
        .await
        .unwrap_or_default();
    if !resumed_history.is_empty() {
        let id = app.session_id.clone();
        app.render_session_history(id, resumed_history);
        // `render_session_history` requests a screen-clear re-anchor so the
        // in-session `/sessions` path can wipe the *current* transcript before
        // repainting. At launch there's nothing committed yet — the blocks just
        // stream into scrollback below the welcome banner — and the wipe would
        // also purge the user's pre-launch terminal scrollback. So drop it.
        app.pending_screen_clear = false;
    }

    let (agent_tx, agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (prompt_tx, prompt_rx) = mpsc::channel::<AgentRequest>(8);
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>(8);
    // Background-shell registry: owned here so we SIGKILL-all on the reliable
    // shutdown path (main awaits run_console). agent_loop holds a clone.
    let bg_shells = std::sync::Arc::new(crate::tools::BackgroundShells::new());
    // Two picker channels, split by origin (capacity 4 — pickers serialize, one
    // open at a time; the buffer just decouples send from drain):
    //   * `tool_picker_*` — the `ask_user` tool and the permission gate
    //     (core side). These ride the `FrontendHub`/`RequestBroker` so the
    //     answer correlates back to the blocked tool by id — the same path an
    //     out-of-process frontend will use.
    //   * `local_picker_*` — frontend-originated pickers (`/connect`, `/afk`,
    //     `/telemetry`) whose reply logic runs in the frontend itself; they
    //     open directly with a local oneshot and never cross the core boundary.
    let (tool_picker_tx, tool_picker_rx) =
        mpsc::channel::<crate::console::picker::PickerRequest>(4);
    let picker_tx_runner = tool_picker_tx;
    let (local_picker_tx, local_picker_rx) =
        mpsc::channel::<crate::console::picker::PickerRequest>(4);
    // Picker reply confirmation channel: handlers that run in `tokio::spawn`
    // (telemetry, AFK) can't reach `app.add_assistant_notice` directly, so
    // they send the confirm string here and the main loop drains it.
    let (notice_tx, notice_rx) = mpsc::channel::<String>(8);
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
    let driver_storage_dir = storage_dir.clone();
    let ui_storage_dir = storage_dir;
    let agent_cwd = cwd;
    let agent_config = config;
    let runner_hook_registry = hook_registry.clone();
    app.hooks = Some(hook_registry);

    let ui_skill_registry = skill_registry.clone();
    // The UI keeps its own `App.skills` clone; `agent_loop` owns the runner's
    // copy (and takes it `mut` so `ReloadSkills` can swap in a fresh scan).
    let runner_skill_registry = skill_registry.clone();
    let driver_skill_registry = skill_registry.clone();
    app.skills = Some(ui_skill_registry);

    let runner_mcp_registry = mcp_registry.clone();
    let driver_mcp_registry = mcp_registry.clone();
    app.mcp = Some(mcp_registry);

    let runner_permissions = permissions.clone();
    let driver_permissions = permissions.clone();
    app.permissions = Some(permissions);

    // Auto-update check: fire-and-forget HTTP GET in the background; the
    // event loop polls the oneshot via try_recv and sets `app.update_notice`
    // when it lands. Skip gate (env opt-out, CI, stderr-not-TTY, debug build,
    // unsupported target) lives in cli::upgrade::should_check_for_update.
    let update_check_rx = if crate::cli::upgrade::should_check_for_update() {
        Some(crate::cli::upgrade::spawn_update_check())
    } else {
        None
    };

    // Background agent runner — the frontend-agnostic core loop, shared with
    // the headless `--engine` mode.
    tokio::spawn(agent_loop(
        prompt_rx,
        cancel_rx,
        agent_tx,
        picker_tx_runner,
        active_inject_runner,
        agent_config,
        agent_system_prompt,
        agent_storage_dir,
        agent_cwd,
        runner_hook_registry,
        runner_skill_registry,
        runner_mcp_registry,
        runner_permissions,
        bg_shells.clone(),
    ));

    // Frontend seam (PR #174, phase 1). The core drives a `FrontendHub` over a
    // `LocalTuiPort`; the ratatui loop below holds the matching `TuiHandle`.
    // Agent events and tool-picker requests flow core→frontend as `Outbound`
    // frames; the frontend's picker answers flow back as `ClientCommand::Reply`,
    // which the broker correlates to the blocked tool's oneshot. Submit, cancel,
    // and inject also ride the protocol now; the core maps Submit→AgentRequest
    // against the session id it tracks (seeded here, retargeted by SetSession on
    // /clear · /resume). The frontend keeps a `prompt_tx` clone only for config
    // commands (model switch / config + skills reload).
    let (local_port, tui_handle) = local_tui(256);
    let mut acceptor = Acceptor::new();
    acceptor.attach(Box::new(local_port));
    let hub = FrontendHub::new(
        app.session_id.clone(),
        app.provider.clone().unwrap_or_default(),
        app.model.clone().unwrap_or_default(),
        app.cwd.to_string_lossy().to_string(),
        // The ratatui frontend ignores snapshots (its /model picker reads App
        // directly), so the wire model list is unused here.
        Vec::new(),
        acceptor,
        RequestBroker::new(),
    );
    tokio::spawn(drive_frontend_core(
        hub,
        agent_rx,
        tool_picker_rx,
        cancel_tx,
        active_inject,
        prompt_tx.clone(),
        driver_permissions,
        driver_skill_registry,
        driver_mcp_registry,
        app.session_id.clone(),
        driver_storage_dir,
    ));

    // The in-progress block's already-streamed row count lives on `app`
    // (`app.committed_rows`) so a session reset (`/clear`, `/resume`) resets it
    // in lockstep with `app.committed` — see reset_transcript_view.
    //
    // All anchoring state — screen-clear re-anchor episodes (#140/#154), the
    // resize settle (#138), and the band's converged geometry — lives in the
    // pure `Anchor` machine (see `render::anchor` for the invariants and their
    // table tests). The loop only executes the wipes/rebuilds it asks for and
    // reports the outcomes back. `epoch` is the monotonic clock it runs on.
    let mut console = ConsoleLoop {
        anchor: Anchor::new(viewport_rows, crossterm::terminal::size()?),
        epoch: std::time::Instant::now(),
        last_draw: std::time::Instant::now(),
        // Opt-in render-loop health heartbeat (frames / commits / re-anchors)
        // to `~/.ignis/logs/ignis.log`, for diagnosing rendering in the field.
        diag: RenderDiag::from_env(),
        scrollback_replay: None,
        app,
        terminal,
        outbound: tui_handle.outbound,
        commands: tui_handle.commands,
        local_picker_rx,
        notice_rx,
        update_check_rx,
        render_hook_queue,
        prompt_tx,
        local_picker_tx,
        notice_tx,
        ui_storage_dir,
    };
    let run_result = console.run().await;
    // SIGKILL any live background shells before returning (success or error) so
    // none are orphaned. Reliable here — main awaits run_console — unlike the
    // detached agent_loop's best-effort cleanup.
    bg_shells.kill_all();
    run_result?;

    // Restore the cursor before the guard drops. The band stays in the normal
    // buffer, so the conversation remains in scrollback after exit. `_term_guard`
    // disables raw mode + bracketed paste on the way out (clean exit, `?`-bubbled
    // Err returns, and panics).
    console.terminal.show_cursor()?;

    // Leave a copy-pasteable resume hint below the band, like `claude --resume
    // <id>`. We only reach this line on a clean Ctrl+D exit — every error path
    // `?`-bubbles earlier — so it never fires on a crash. Skipped when the user
    // sent nothing: an untouched session has nothing worth resuming. Uses
    // `app.session_id`, which tracks the *current* session after any mid-run
    // `/resume` or `/clear`, not the id we opened with.
    if console.app.turn_count() > 0 {
        let session_id = console.app.session_id.clone();
        // Row just below the live band. The inline viewport anchors near the
        // top after a short-history re-anchor and at the screen bottom in
        // normal use; reading its area keeps the hint flush under the footer
        // either way (vs. jumping to the screen bottom and leaving a gap).
        let band_bottom = console.terminal.get_frame().size().bottom();
        // Restore cooked mode first so `\n` and colors render normally.
        drop(_term_guard);
        let _ = print_resume_hint(&session_id, band_bottom);
    }
    Ok(())
}

/// Headless core engine (PR #174, topology ii). Run the agent + `FrontendHub`
/// over NDJSON on this process's own stdin/stdout, with no terminal/ratatui:
/// the interactive frontend (the Ink `ignis-tui`) owns the TTY and spawns this
/// process, reading `Outbound` frames from our stdout and writing
/// `ClientCommand`s to our stdin. Doubles as a scriptable agent — pipe NDJSON
/// in, get NDJSON out.
///
/// Reuses the exact same [`agent_loop`] and [`drive_frontend_core`] as the
/// ratatui runner — only the port differs ([`StdioPort`] vs `LocalTuiPort`).
/// Documented gaps vs. the ratatui path (not part of the protocol): no
/// `AssistantMessageRender` hook rewrite and no `/afk`·`/telemetry` notice
/// relay — those are frontend-local UI affordances, not core behavior.
#[allow(clippy::too_many_arguments)]
pub async fn run_engine(
    session_id: String,
    system_prompt: String,
    storage_dir: PathBuf,
    cwd: PathBuf,
    config: crate::config::Config,
    skill_registry: std::sync::Arc<crate::skills::SkillRegistry>,
    mcp_registry: std::sync::Arc<crate::mcp::McpRegistry>,
    permissions: std::sync::Arc<crate::permissions::runtime::PermissionState>,
    hook_registry: crate::hooks::HookRegistry,
) -> Result<(), anyhow::Error> {
    let (agent_tx, agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (prompt_tx, prompt_rx) = mpsc::channel::<AgentRequest>(8);
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>(8);
    let (tool_picker_tx, tool_picker_rx) =
        mpsc::channel::<crate::console::picker::PickerRequest>(4);
    let active_inject: ActiveInject = std::sync::Arc::new(std::sync::Mutex::new(None));
    // Background-shell registry: owned here so we can SIGKILL-all on the reliable
    // shutdown path below (main awaits run_engine). agent_loop holds a clone.
    let bg_shells = std::sync::Arc::new(crate::tools::BackgroundShells::new());

    // Capture the statusline + /model-picker meta before `config`/`cwd` move
    // into the agent loop.
    let provider = config.active_provider().unwrap_or_default();
    let model = config.active_model().unwrap_or_default();
    let effort = config.active_effort();
    let cwd_str = cwd.to_string_lossy().to_string();
    let catalog = crate::llm::catalog::load();
    let models: Vec<crate::console::frontend::protocol::ModelRef> = config
        .model_options(&catalog)
        .into_iter()
        .map(|o| crate::console::frontend::protocol::ModelRef {
            provider: o.provider,
            model: o.model,
            context: o.context,
            effort_levels: o.effort_levels,
        })
        .collect();

    let agent_handle = tokio::spawn(agent_loop(
        prompt_rx,
        cancel_rx,
        agent_tx,
        tool_picker_tx,
        active_inject.clone(),
        config,
        system_prompt,
        storage_dir.clone(),
        cwd,
        hook_registry,
        skill_registry.clone(),
        mcp_registry.clone(),
        permissions.clone(),
        bg_shells.clone(),
    ));

    // The frontend speaks NDJSON on our own stdio. emit() writes Outbound to
    // stdout (→ frontend); next_command() reads ClientCommands from stdin.
    let port = StdioPort::new(tokio::io::stdout(), tokio::io::stdin());
    let mut acceptor = Acceptor::new();
    acceptor.attach(Box::new(port));
    let mut hub = FrontendHub::new(
        session_id.clone(),
        provider,
        model,
        cwd_str,
        models,
        acceptor,
        RequestBroker::new(),
    );
    // Seed the active reasoning effort so the first snapshot's footer + `/model`
    // picker reflect it (provider/model/cwd ride `new`; effort updates on `/model`).
    hub.set_effort(effort);

    // Startup resume parity with the native runner (#165): if this session id
    // already has a persisted transcript, replay it so the frontend paints the
    // prior conversation at launch — not just new turns. The id reaches us via
    // `IGNIS_SESSION_ID` from the Ink launcher (see main.rs); a fresh id has no
    // file, so this is a quick no-op on a normal launch. Sent before the driver's
    // initial snapshot — transcript→snapshot mirrors the in-session `/resume`
    // order the frontend already handles.
    let resumed = FileStorage::new(storage_dir.clone())
        .load_session(&session_id)
        .await
        .unwrap_or_default();
    if !resumed.is_empty() {
        hub.send_transcript(session_id.clone(), transcript_blocks(resumed))
            .await;
    }
    // Re-emit the persisted task list so the todo panel paints immediately on
    // resume — the transcript frame resets the frontend's todos to [], so
    // this must follow it. Without this, the panel stays empty until the
    // user sends their next prompt (the per-turn re-sync in `agent_loop`
    // only fires at the start of a prompt, not at resume time).
    let todos = FileStorage::new(storage_dir.clone())
        .load_todos(&session_id)
        .await
        .unwrap_or_default();
    if !todos.is_empty() {
        hub.emit_event(AgentEvent::Todos { items: todos }).await;
    }

    // Drive until the frontend closes our stdin (EOF = clean disconnect).
    drive_frontend_core(
        hub,
        agent_rx,
        tool_picker_rx,
        cancel_tx,
        active_inject,
        prompt_tx,
        permissions,
        skill_registry,
        mcp_registry,
        session_id,
        storage_dir,
    )
    .await;

    // The driver returns the instant the frontend disconnects (stdin EOF /
    // Shutdown), but the agent loop persists each turn AFTER emitting `TurnEnd`,
    // on its own task. Returning here drops the tokio runtime, which would abort
    // an in-flight `persist()` — silently losing the turn that just completed
    // (a frontend that exits right after a reply hits this race). The driver
    // dropped the sole `prompt_tx`, so the loop's next `recv()` returns `None`
    // and it exits once any in-flight turn finishes; await it so that final save
    // lands. Bounded — a turn still mid-flight at disconnect can't wedge exit.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), agent_handle).await;

    // Frontend disconnected: SIGKILL any live background shells so none are
    // orphaned. Reliable here (main awaits run_engine), unlike the detached
    // agent_loop's own best-effort cleanup.
    bg_shells.kill_all();
    Ok(())
}

/// The core driver (PR #174, phase 1). Bridges agent events and tool-initiated
/// picker requests to the active frontend through `hub`, and resolves the
/// frontend's picker answers back onto the blocked tools' oneshots via the
/// broker. Runs until the frontend disconnects with no successor.
///
/// `Reply` is resolved against the broker inside `handle_command`. `Submit` is
/// mapped to an [`AgentRequest`] (against the core-tracked session id) and sent
/// to the agent task; `SetSession` retargets that id; `Cancel` / `Inject` are
/// forwarded to the agent (the cancel channel and the live turn's inject
/// source). The frontend keeps `prompt_tx` only for config commands (model
/// switch / config + skills reload), which still travel that channel directly.
/// Skill/MCP enabled state as wire [`Toggle`]s for the `/skills` `/mcp` pickers.
fn skill_toggles(
    reg: &crate::skills::SkillRegistry,
) -> Vec<crate::console::frontend::protocol::Toggle> {
    reg.all()
        .iter()
        .map(|s| crate::console::frontend::protocol::Toggle {
            name: s.name.clone(),
            enabled: reg.is_enabled(&s.name),
        })
        .collect()
}

fn mcp_toggles(reg: &crate::mcp::McpRegistry) -> Vec<crate::console::frontend::protocol::Toggle> {
    reg.entries()
        .into_iter()
        .map(|e| crate::console::frontend::protocol::Toggle {
            enabled: !matches!(e.status, crate::mcp::McpStatus::Disabled),
            name: e.name,
        })
        .collect()
}

/// The project's past sessions as wire [`SessionInfo`]s for the `/sessions`
/// picker — most-recent-first, with `exclude_id` (the live session) dropped.
fn session_infos(
    storage_dir: &std::path::Path,
    exclude_id: &str,
) -> Vec<crate::console::frontend::protocol::SessionInfo> {
    crate::session::SessionManager::new(storage_dir.to_path_buf())
        .list()
        .into_iter()
        .filter(|m| m.id != exclude_id)
        .map(|m| crate::console::frontend::protocol::SessionInfo {
            id: m.id,
            preview: m.preview,
            message_count: m.message_count,
            last_modified: m.last_modified,
        })
        .collect()
}

/// Map a loaded session's messages into render-ready [`TranscriptBlock`]s for
/// replay, via the shared `console::transcript::reduce_transcript` walk — the
/// same reduction the native renderer applies, mapped to the protocol block type.
fn transcript_blocks(
    messages: Vec<Message>,
) -> Vec<crate::console::frontend::protocol::TranscriptBlock> {
    use crate::console::frontend::protocol::TranscriptBlock;
    use crate::console::transcript::TranscriptItem;
    crate::console::transcript::reduce_transcript(messages)
        .into_iter()
        .map(|item| match item {
            TranscriptItem::User(text) => TranscriptBlock::User { text },
            TranscriptItem::Reasoning(text) => TranscriptBlock::Reasoning { text },
            TranscriptItem::Assistant(text) => TranscriptBlock::Assistant { text },
            TranscriptItem::Tool {
                name, args, result, ..
            } => TranscriptBlock::Tool {
                name,
                args,
                result: match result {
                    None => crate::ToolResult::ok(String::new()),
                    Some((content, is_error)) => crate::ToolResult { content, is_error },
                },
            },
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn drive_frontend_core(
    mut hub: FrontendHub,
    mut agent_rx: mpsc::Receiver<AgentEvent>,
    mut tool_picker_rx: mpsc::Receiver<crate::console::picker::PickerRequest>,
    cancel_tx: mpsc::Sender<()>,
    active_inject: ActiveInject,
    prompt_tx: mpsc::Sender<AgentRequest>,
    permissions: std::sync::Arc<crate::permissions::runtime::PermissionState>,
    skill_registry: std::sync::Arc<crate::skills::SkillRegistry>,
    mcp_registry: std::sync::Arc<crate::mcp::McpRegistry>,
    mut current_session_id: String,
    storage_dir: PathBuf,
) {
    // Seed the snapshot with the live permission mode + skill/MCP state + the
    // generic `/settings` config knobs (built from live perms + persisted state).
    hub.set_mode(permissions.mode().as_str().to_string());
    hub.set_skills(skill_toggles(&skill_registry));
    hub.set_mcp(mcp_toggles(&mcp_registry));
    hub.set_settings(crate::console::settings::build_settings(
        &permissions,
        &crate::state::load_state(),
        &crate::config::load_config().unwrap_or_default(),
    ));
    // Hand the freshly-attached frontend its session snapshot (provider/model/
    // cwd/session id) so it can render a statusline before any turn. The
    // ratatui frontend ignores snapshots; the out-of-process Ink one consumes it.
    hub.send_snapshot().await;

    // `/connect`'s multi-step wizard (provider → API key → model). Only ever
    // active for the out-of-process Ink frontend: the native ratatui frontend
    // runs `/connect` locally in `keys.rs` and never submits it across the seam.
    let mut connect = crate::console::connect::ConnectFlow::default();

    // `CoreWake` dodges the select-arm borrow: only `next_command` borrows
    // `hub` inside the select, and every arm future is dropped before the match
    // touches `hub` again (mirrors `ConsoleLoop::wake`).
    loop {
        let wake = tokio::select! {
            biased;
            cmd = hub.next_command() => CoreWake::Command(cmd),
            Some(ev) = agent_rx.recv() => CoreWake::Event(ev),
            Some(req) = tool_picker_rx.recv() => CoreWake::Request(req),
        };
        match wake {
            // A reply that lands while `/connect` is mid-flight drives the
            // wizard, not the broker — route it before the generic handler.
            CoreWake::Command(Some(ClientCommand::Reply { answer, .. })) if connect.is_active() => {
                advance_connect(&mut connect, &mut hub, &prompt_tx, answer).await;
            }
            CoreWake::Command(Some(cmd)) => match hub.handle_command(cmd) {
                // Map the user line to an agent request against the current
                // session. The frontend pre-resolves App-dependent dispatch
                // (provider gate, skill expansion, app-only commands), so what
                // arrives here is either "/compact", "/connect", or a
                // ready-to-run prompt.
                CommandOutcome::Submit(text) => {
                    if text == "/connect" {
                        // Engine owns the wizard: open its first picker. The
                        // frontend renders it like any `ask_user`; the reply
                        // comes back as a `Reply` we intercept above.
                        let current =
                            (!hub.provider().is_empty()).then(|| hub.provider().to_string());
                        match connect.start(hub.has_pending(), current) {
                            Ok(req) => hub.open_request(req).await,
                            Err(notice) => {
                                hub.emit_event(AgentEvent::Notice { message: notice }).await
                            }
                        }
                        continue;
                    }
                    let request = if text == "/compact" {
                        AgentRequest::Compact {
                            session_id: current_session_id.clone(),
                        }
                    } else {
                        AgentRequest::Prompt {
                            session_id: current_session_id.clone(),
                            prompt: text,
                        }
                    };
                    // `send` (not `try_send`): submits only arrive while the
                    // agent is idle/just-ended, so the queue isn't full — back-
                    // pressure here is correct, matching the pre-seam behavior.
                    let _ = prompt_tx.send(request).await;
                }
                // The frontend switched sessions (/resume); retarget.
                CommandOutcome::SetSession(id) => current_session_id = id,
                // `/clear`: the core mints a fresh session id, retargets the next
                // submit at it, and re-snapshots the frontend with the new id
                // (engine owns session creation for out-of-process frontends).
                CommandOutcome::NewSession => {
                    let new_id = crate::session::SessionManager::create_id();
                    current_session_id = new_id.clone();
                    hub.set_session_id(new_id);
                    hub.send_snapshot().await;
                }
                // `/model`: apply the switch to subsequent prompts and re-snapshot
                // so the frontend's statusline reflects the new model + effort.
                CommandOutcome::SetModel {
                    provider,
                    model,
                    effort,
                } => {
                    let _ = prompt_tx
                        .send(AgentRequest::SetModel {
                            provider: provider.clone(),
                            model: model.clone(),
                            effort: effort.clone(),
                        })
                        .await;
                    // Persist the switch (model + effort) so it survives a restart,
                    // exactly like the native picker.
                    let _ =
                        crate::state::persist_model_selection(&provider, &model, effort.as_deref());
                    hub.set_active_model(provider, model, effort);
                    hub.send_snapshot().await;
                }
                // `/afk`: apply + persist the permission mode and re-snapshot so
                // the statusline badge updates.
                CommandOutcome::SetMode(mode_str) => {
                    if let Some(m) = crate::permissions::Mode::parse(&mode_str) {
                        permissions.set_mode(m);
                        let _ = crate::state::persist_permission_mode(Some(m.as_str()));
                        hub.set_mode(m.as_str().to_string());
                        hub.send_snapshot().await;
                    }
                }
                // `/settings`: apply the knob (its effect + persist), rebuild the
                // settings list from the new state, and re-snapshot so the panel
                // (and, for statusline knobs, the footer) reflect it.
                CommandOutcome::SetSetting { id, value } => {
                    use crate::console::settings::Effect;
                    // Persist the knob; config.toml-overlay knobs need the agent
                    // loop to re-read its merged config so the next prompt honors
                    // them (mirrors `/connect`'s ReloadConfig).
                    if crate::console::settings::apply_setting(&id, value, &permissions)
                        == Effect::ReloadConfig
                    {
                        let _ = prompt_tx.send(AgentRequest::ReloadConfig).await;
                    }
                    // Rebuild from the merged config (state.json overlaid) so the
                    // panel — and the footer, for statusline knobs — reflect it.
                    hub.set_settings(crate::console::settings::build_settings(
                        &permissions,
                        &crate::state::load_state(),
                        &crate::config::load_config().unwrap_or_default(),
                    ));
                    hub.send_snapshot().await;
                }
                // `/skills`: flip the skill, persist the disabled set, re-snapshot.
                CommandOutcome::ToggleSkill(name) => {
                    skill_registry.toggle(&name);
                    let disabled: Vec<String> = skill_registry
                        .all()
                        .iter()
                        .filter(|s| !skill_registry.is_enabled(&s.name))
                        .map(|s| s.name.clone())
                        .collect();
                    let _ = crate::state::persist_disabled_skills(&disabled);
                    hub.set_skills(skill_toggles(&skill_registry));
                    hub.send_snapshot().await;
                }
                // `/mcp`: same, for MCP servers.
                CommandOutcome::ToggleMcp(name) => {
                    mcp_registry.toggle(&name);
                    let disabled: Vec<String> = mcp_registry
                        .entries()
                        .into_iter()
                        .filter(|e| matches!(e.status, crate::mcp::McpStatus::Disabled))
                        .map(|e| e.name)
                        .collect();
                    let _ = crate::state::persist_disabled_mcp_servers(&disabled);
                    hub.set_mcp(mcp_toggles(&mcp_registry));
                    hub.send_snapshot().await;
                }
                // `/copy`: write the frontend-supplied text (the last assistant
                // message) to the clipboard via the platform helper, warning
                // only if it fails — success is the frontend's optimistic notice
                // (mirrors ratatui's local feedback).
                CommandOutcome::Copy(text) => {
                    if let Err(err) = crate::console::clipboard::set_clipboard(&text) {
                        hub.emit_event(AgentEvent::Warning {
                            source: "clipboard".to_string(),
                            message: err,
                        })
                        .await;
                    }
                }
                // `/sessions`: list the project's past sessions off disk
                // (current one excluded) for the frontend's picker.
                CommandOutcome::ListSessions => {
                    let sessions = session_infos(&storage_dir, &current_session_id);
                    hub.send_sessions(sessions).await;
                }
                // `/sessions` pick / `/resume`: retarget subsequent submits at
                // the chosen session (the agent's `Session::open` continues it),
                // replay its transcript so the frontend repaints scrollback, and
                // re-snapshot so the statusline session id follows.
                CommandOutcome::ResumeSession(id) => {
                    let messages = FileStorage::new(storage_dir.clone())
                        .load_session(&id)
                        .await
                        .unwrap_or_default();
                    let todos = FileStorage::new(storage_dir.clone())
                        .load_todos(&id)
                        .await
                        .unwrap_or_default();
                    current_session_id = id.clone();
                    hub.set_session_id(id.clone());
                    hub.send_transcript(id, transcript_blocks(messages)).await;
                    // Re-emit persisted todos so the panel paints now, not on
                    // the next prompt (the transcript frame cleared them).
                    if !todos.is_empty() {
                        hub.emit_event(AgentEvent::Todos { items: todos }).await;
                    }
                    hub.send_snapshot().await;
                }
                // The agent task drains stale cancels at each prompt's start, so
                // an inter-turn cancel is harmless — no gating needed here.
                CommandOutcome::Control(ControlSignal::Cancel) => {
                    let _ = cancel_tx.try_send(());
                }
                // Steer the live prompt's inject source. The frontend only sends
                // this while a prompt is accepting injects, so the sender is
                // normally present; a missing/full one is dropped, not blocked.
                CommandOutcome::Control(ControlSignal::Inject(text)) => {
                    let sender = active_inject.lock().unwrap().clone();
                    if let Some(tx) = sender {
                        let _ = tx.try_send(text);
                    }
                }
                // Frontend asked to wind down (explicit Shutdown). End the driver
                // loop, same as an EOF disconnect — don't silently ignore it.
                CommandOutcome::Control(ControlSignal::Shutdown) => break,
                _ => {}
            },
            // Frontend disconnected with no successor — nothing left to drive.
            CoreWake::Command(None) => break,
            CoreWake::Event(ev) => hub.emit_event(ev).await,
            CoreWake::Request(req) => hub.open_request(req).await,
        }
    }
}

/// Drive one step of the `/connect` wizard from a frontend reply. `ConnectFlow`
/// owns all the logic + disk writes (config.toml + state.json); the runner just
/// opens the next picker, or — on completion — reloads config, rebuilds the
/// `/model` list, and re-snapshots so the frontend sees the new provider.
async fn advance_connect(
    connect: &mut crate::console::connect::ConnectFlow,
    hub: &mut FrontendHub,
    prompt_tx: &mpsc::Sender<AgentRequest>,
    answer: ReplyAnswer,
) {
    use crate::console::connect::{ConnectOutcome, ConnectResult};
    // We route this reply ourselves, not through the broker, so clear the
    // in-flight slot the picker left set.
    hub.clear_pending();
    let answers = match answer {
        ReplyAnswer::Answered(v) => v,
        ReplyAnswer::Cancelled => {
            if let Some(notice) = connect.cancel() {
                hub.emit_event(AgentEvent::Notice { message: notice }).await;
            }
            return;
        }
    };
    // Clone the active pair so the `hub` borrow doesn't span `advance`.
    let cp = hub.provider().to_string();
    let cm = hub.model().to_string();
    let current = (!cp.is_empty()).then_some((cp.as_str(), cm.as_str()));
    match connect.advance(answers, current) {
        ConnectOutcome::NextPicker(req) => hub.open_request(req).await,
        ConnectOutcome::Done { notices, result } => {
            for message in notices {
                hub.emit_event(AgentEvent::Notice { message }).await;
            }
            match result {
                // The new provider's models are now in config.toml; reload so
                // the agent loop's `agent_config` (and the next prompt) sees the
                // fresh api_key + active model.
                ConnectResult::Switched(provider, model) => {
                    let _ = prompt_tx.send(AgentRequest::ReloadConfig).await;
                    connect_refresh_models(hub, Some((provider, model))).await;
                }
                ConnectResult::KeptCurrent => {
                    let _ = prompt_tx.send(AgentRequest::ReloadConfig).await;
                    connect_refresh_models(hub, None).await;
                }
                ConnectResult::Failed => {}
            }
        }
    }
}

/// Rebuild the hub's `/model` list from the freshly-written config and
/// re-snapshot. `switch_to` is `Some` only when `/connect` activated a model.
async fn connect_refresh_models(hub: &mut FrontendHub, switch_to: Option<(String, String)>) {
    let catalog = crate::llm::catalog::load();
    match crate::config::load_config() {
        Ok(cfg) => {
            let models = cfg
                .model_options(&catalog)
                .into_iter()
                .map(|o| crate::console::frontend::protocol::ModelRef {
                    provider: o.provider,
                    model: o.model,
                    context: o.context,
                    effort_levels: o.effort_levels,
                })
                .collect();
            hub.set_models(models);
            if let Some((provider, model)) = switch_to {
                hub.set_active_model(provider, model, cfg.active_effort());
            }
        }
        // Config re-read failed (rare): still reflect the requested switch.
        Err(_) => {
            if let Some((provider, model)) = switch_to {
                hub.set_active_model(provider, model, None);
            }
        }
    }
    hub.send_snapshot().await;
}

/// Redraw cadence for the live band: agent events and keystrokes are coalesced
/// between frames and the screen is redrawn at most once per interval, so a
/// fast token stream never triggers a redraw per delta — which tears/flickers
/// on slow terminals (e.g. Windows Terminal over WSL2). ~30fps.
const FRAME: std::time::Duration = std::time::Duration::from_millis(33);

/// What woke the frame loop (see [`ConsoleLoop::wake`]).
enum Wake {
    /// Frame deadline: nothing arrived, just tick/draw.
    Tick,
    /// A frame from the core: an agent event or a tool-initiated picker
    /// request (or a snapshot, unused with a single local frontend).
    Frame(Outbound),
    /// A frontend-originated picker (`/connect`, `/afk`, `/telemetry`) opened
    /// over the local channel — never crosses the core boundary.
    LocalPicker(crate::console::picker::PickerRequest),
}

/// What woke the core driver task (the `FrontendHub` side; see `run_console`).
enum CoreWake {
    /// An upstream command from the active frontend (`None` = disconnected).
    Command(Option<ClientCommand>),
    /// A streaming agent event to forward to the frontend.
    Event(AgentEvent),
    /// A tool-initiated picker request to bridge through the broker.
    Request(crate::console::picker::PickerRequest),
}

/// The live console: `App` (the UI model), the inline terminal, the `Anchor`
/// machine, and every channel the frame loop drains. [`Self::run`] is the
/// stable frame skeleton — mirroring [`crate::agent::Agent::run`]'s turn
/// skeleton — and each lifecycle moment lives in its own method: waking
/// ([`Self::wake`]), event intake ([`Self::drain_events`]), the queued-prompt
/// pump ([`Self::pump_queued`]), terminal input ([`Self::poll_input`]),
/// screen-clear re-anchoring ([`Self::resolve_reanchor`]), pushing rows into
/// scrollback ([`Self::commit_scrollback`]), and the coalesced band redraw
/// ([`Self::draw_frame`]).
struct ConsoleLoop {
    app: App,
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    anchor: Anchor,
    /// Monotonic clock the `Anchor` machine runs on.
    epoch: std::time::Instant,
    diag: RenderDiag,
    /// Welcome-row cursor for a resize replay. Starting a replay also rewinds
    /// `app.committed`/`committed_rows`, so after these rows drain the normal
    /// commit loop replays every finalized transcript block plus stable rows
    /// from an active stream at the new width.
    scrollback_replay: Option<usize>,
    last_draw: std::time::Instant,
    /// Core → frontend frames (agent events, tool picker requests, snapshots),
    /// delivered through the `FrontendHub`'s `LocalTuiPort`.
    outbound: mpsc::Receiver<Outbound>,
    /// Frontend → core commands. Used here only to answer a tool-initiated
    /// picker with a `ClientCommand::Reply` (see `keys::reply_picker`).
    commands: mpsc::Sender<ClientCommand>,
    /// Frontend-originated picker requests (`/connect`, `/afk`, `/telemetry`),
    /// opened locally with a oneshot reply — distinct from the core boundary.
    local_picker_rx: mpsc::Receiver<crate::console::picker::PickerRequest>,
    notice_rx: mpsc::Receiver<String>,
    update_check_rx:
        Option<tokio::sync::oneshot::Receiver<Option<crate::cli::upgrade::UpdateNotice>>>,
    render_hook_queue: mpsc::Sender<RenderJob>,
    prompt_tx: mpsc::Sender<AgentRequest>,
    /// Sender half of [`Self::local_picker_rx`] — handed to the key handlers so
    /// `/connect`'s multi-step flow can open its next picker locally.
    local_picker_tx: mpsc::Sender<crate::console::picker::PickerRequest>,
    notice_tx: mpsc::Sender<String>,
    ui_storage_dir: PathBuf,
}

impl ConsoleLoop {
    fn begin_scrollback_replay(&mut self) {
        self.scrollback_replay = Some(0);
        self.app.committed = 0;
        self.app.committed_rows = 0;
    }

    /// Drive the console until the user quits. The stable frame skeleton:
    /// every iteration wakes, ingests state, then renders — state mutation
    /// strictly precedes terminal output, so a frame never paints half-applied
    /// events. Returns only on quit or a fatal terminal I/O error.
    async fn run(&mut self) -> io::Result<()> {
        let Self { terminal, app, .. } = &mut *self;
        terminal.draw(|f| draw(f, app))?;

        loop {
            self.wake().await;
            self.drain_events();
            self.pump_queued().await;
            self.poll_input().await?;
            if self.app.should_quit {
                break;
            }
            self.resolve_reanchor()?;
            self.commit_scrollback()?;
            self.draw_frame()?;
            self.diag.heartbeat();
        }
        Ok(())
    }

    /// Park until there's work: the next frame deadline, a core frame, or a
    /// frontend-originated picker request.
    async fn wake(&mut self) {
        let wake = tokio::select! {
            _ = tokio::time::sleep(FRAME) => Wake::Tick,
            Some(frame) = self.outbound.recv() => Wake::Frame(frame),
            Some(req) = self.local_picker_rx.recv() => Wake::LocalPicker(req),
        };
        match wake {
            Wake::Tick => {}
            Wake::Frame(frame) => self.on_frame(frame),
            Wake::LocalPicker(req) => self.open_local_picker(req),
        }
    }

    /// Apply one core → frontend frame to the UI model.
    fn on_frame(&mut self, frame: Outbound) {
        match frame {
            Outbound::Event(ev) => self.on_agent_event(*ev),
            Outbound::Request(req) => self.open_request(req),
            // A snapshot only arrives on a FIFO handover to a freshly-promoted
            // frontend. With a single in-process TUI there is no successor, so
            // this is unreachable today; ignore it rather than invent state.
            Outbound::Snapshot(_) => {}
            // Session list / transcript replay answer `ListSessions` /
            // `ResumeSession`, which only the out-of-process frontend sends —
            // the ratatui TUI drives `/sessions` against `App` directly, so it
            // never requests these and ignores them if they ever arrive.
            Outbound::Sessions(_) | Outbound::Transcript { .. } => {}
        }
    }

    /// Feed one agent event to the UI model (and the render-hook queue).
    fn on_agent_event(&mut self, ev: AgentEvent) {
        enqueue_render_hook(
            &ev,
            &self.render_hook_queue,
            &self.app.session_id,
            &self.app.cwd,
        );
        self.app.handle_event(ev);
    }

    /// Open a tool-initiated picker delivered over the frontend protocol — one
    /// at a time. A second request is refused with a `Cancelled` reply so the
    /// blocked tool fails fast instead of stalling.
    fn open_request(&mut self, req: ClientRequest) {
        if self.app.inline_picker.is_some() {
            let _ = self.commands.try_send(ClientCommand::Reply {
                id: req.id,
                answer: ReplyAnswer::Cancelled,
            });
        } else {
            self.app.inline_picker = Some(inline_picker::InlinePickerState::from_request(req));
        }
    }

    /// Open a frontend-originated picker (`/connect`, `/afk`, `/telemetry`),
    /// which replies on its own oneshot. Same one-at-a-time rule as
    /// [`Self::open_request`].
    fn open_local_picker(&mut self, req: crate::console::picker::PickerRequest) {
        if self.app.inline_picker.is_some() {
            let _ = req
                .reply
                .send(crate::console::picker::PickerResponse::Cancelled);
        } else {
            self.app.inline_picker = Some(inline_picker::InlinePickerState::local(req));
        }
    }

    /// Drain everything else pending — core frames, picker-spawn notices, the
    /// auto-update oneshot, queued local picker requests. State only, no draw.
    fn drain_events(&mut self) {
        while let Ok(frame) = self.outbound.try_recv() {
            self.on_frame(frame);
        }
        while let Ok(msg) = self.notice_rx.try_recv() {
            self.app.add_assistant_notice(msg);
        }
        // Poll the auto-update-check oneshot. Resolves once (Ok or Closed),
        // after which we drop the receiver so the branch goes dormant.
        if let Some(rx) = &mut self.update_check_rx {
            match rx.try_recv() {
                Ok(notice) => {
                    self.app.update_notice = notice;
                    self.update_check_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.update_check_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
        while let Ok(req) = self.local_picker_rx.try_recv() {
            self.open_local_picker(req);
        }
    }

    /// Edge-triggered: exactly one queued line per turn-end (TurnEnd).
    /// Route through the same dispatcher Enter uses so queued slash
    /// commands (`/compact`, `/model`, …) actually execute — sending the
    /// text as a raw `AgentRequest::Prompt` would deliver "/compact" to
    /// the LLM as a user message. The user block is rendered when
    /// `Session::prompt` emits `UserPromptCommitted` (post-hook), so we
    /// don't push it here.
    async fn pump_queued(&mut self) {
        if self.app.take_turn_just_ended() {
            // Control returned to the user — refresh the footer branch so a
            // `git checkout` the agent ran this turn is reflected (oh-my-zsh
            // recomputes per prompt; same idea).
            self.app.refresh_git_branch();
            if let Some(text) = self.app.take_queued_front() {
                crate::console::keys::submit_text(
                    &mut self.app,
                    text,
                    &self.commands,
                    &self.local_picker_tx,
                    &self.notice_tx,
                    &self.ui_storage_dir,
                )
                .await;
            }
        }
    }

    /// Poll terminal input — keys, paste, resize — without blocking.
    async fn poll_input(&mut self) -> io::Result<()> {
        while event::poll(std::time::Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) => {
                    handle_key(
                        &mut self.app,
                        key,
                        &self.prompt_tx,
                        &self.ui_storage_dir,
                        &self.local_picker_tx,
                        &self.notice_tx,
                        &self.commands,
                    )
                    .await;
                }
                Event::Paste(data) => crate::console::keys::handle_paste(&mut self.app, data),
                // A resize (incl. same-grid-size DPI changes) must force a
                // settle re-anchor — see `Anchor::band_step`.
                Event::Resize(_, _) => self.anchor.on_resize(self.epoch.elapsed()),
                // No mouse capture in inline mode — wheel scroll and click-drag
                // selection are handled natively by the terminal/scrollback.
                _ => {}
            }
        }
        Ok(())
    }

    /// Resolve a session reset (`/clear`, `/resume`): wipe the visible screen
    /// AND the scrollback history, then re-anchor a fresh viewport, so old
    /// output doesn't linger when scrolling up. `committed`/`committed_rows`
    /// were already reset in App, so the new session's blocks commit from the
    /// top. The episode policy (wipe once, bounded DSR attempts, DSR-free
    /// fallback) lives in `Anchor`; this executes its instructions.
    fn resolve_reanchor(&mut self) -> io::Result<()> {
        // Compute the target viewport height *before* the rebuild — the
        // pending viewport_rows is from the previous frame (e.g. 2 rows while
        // a picker was open), but the picker just closed, so the new size is
        // the band height. Building the post-clear terminal at the right size
        // up front means the commit loop runs in the correct viewport and the
        // draw-time rebuild check is a no-op.
        let term_size = crossterm::terminal::size()?;
        let want_rows = render::viewport_height(&self.app, term_size.0, term_size.1);
        // Transfer a reset's wipe request (`/clear`, `/resume`) into the anchor
        // machine; `anchor.can_commit()` stays false until the episode resolves.
        if self.app.pending_screen_clear {
            self.app.pending_screen_clear = false;
            self.anchor.request_reanchor();
        }
        let now = self.epoch.elapsed();
        let Some(step) = self.anchor.clear_step(now, want_rows) else {
            return Ok(());
        };
        if step.wipe == Some(Wipe::All) {
            // First frame of the episode: wipe visible screen + scrollback
            // exactly once (re-wiping every frame floods the terminal and
            // worsens the very DSR backpressure that keeps the re-anchor
            // from landing).
            execute!(
                io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
                crossterm::cursor::MoveTo(0, 0)
            )?;
            log::info!(
                "render: re-anchor started (blocks={}, committed={}, want_rows={})",
                self.app.blocks.len(),
                self.app.committed,
                step.want_rows
            );
        }
        let ok = try_rebuild(&mut self.terminal, step.want_rows)?;
        let pending_blocks = self.app.blocks.len().saturating_sub(self.app.committed);
        // Re-read the clock for the report: `waited` in the log lines must
        // include the rebuild attempt itself (~2s when its DSR times out),
        // or a stalled first attempt logs a misleading 0ms.
        match self
            .anchor
            .clear_rebuilt(ok, self.epoch.elapsed(), step.want_rows, term_size)
        {
            ClearOutcome::Landed { attempts, waited } => {
                // Re-anchor landed: resume commits from the top of scrollback.
                if attempts > 0 {
                    log::info!(
                        "render: re-anchor landed after {} retr{} ({}ms) — resuming commits",
                        attempts,
                        if attempts == 1 { "y" } else { "ies" },
                        waited.as_millis()
                    );
                }
                self.diag.on_reanchor_ok();
            }
            ClearOutcome::Held { attempts, waited } => {
                self.diag.on_reanchor_failed();
                log::warn!(
                    "render: re-anchor DSR not landed (attempt {}/{}, {}ms) — \
                     holding {} block(s); retrying",
                    attempts,
                    anchor::MAX_REANCHOR_ATTEMPTS,
                    waited.as_millis(),
                    pending_blocks
                );
            }
            ClearOutcome::ForcedFallback { attempts, waited } => {
                // Backstop: the re-anchor DSR isn't answering (WSL2/conpty
                // under backpressure). Gating all commits on it leaves the
                // screen blank while the agent keeps working — the "blank
                // after input, full content on resume" wedge. Re-anchor
                // WITHOUT a DSR via `terminal.clear()` (resets the back
                // buffer using the known viewport area) and resume commits.
                self.diag.on_reanchor_failed();
                log::warn!(
                    "render: re-anchor DSR unresponsive after {} attempts ({}ms) — \
                     forcing DSR-free re-anchor so {} block(s) can paint \
                     (blocks={}, committed={})",
                    attempts,
                    waited.as_millis(),
                    pending_blocks,
                    self.app.blocks.len(),
                    self.app.committed
                );
                let _ = self.terminal.clear();
                self.diag.on_forced_reanchor();
            }
        }
        Ok(())
    }

    /// Push settled rows into the terminal's native scrollback via
    /// insert_before — the terminal owns them from there (native scroll +
    /// copy + tmux-reattach persistence). Finalized blocks flush whole; the
    /// in-progress assistant/reasoning block streams its *stable* rows as
    /// they settle (stream_commit), so text flows smoothly into scrollback
    /// rather than appearing all at once. A pending tool block waits.
    fn commit_scrollback(&mut self) -> io::Result<()> {
        let width = self.terminal.size()?.width;
        // Collect the new rows for every block we're about to commit in this
        // frame, then hand them to insert_before as a single batch. Calling
        // insert_before multiple times in succession overwrites prior inserts
        // — each call's `clear()` only protects the live viewport, so earlier
        // buffer content gets shifted up off the top of the terminal by
        // subsequent `append_lines` calls and overwritten. One call per frame
        // preserves the full ordered history (matters for /resume which
        // commits N blocks back-to-back after a screen clear). For the
        // streaming case, the loop breaks after the first in-progress block,
        // so the batch is always exactly the new rows of that one block.
        // Cap rows per frame so the single `insert_before` buffer (width * rows
        // cells) stays within ratatui's u16 limit — see `max_commit_rows`. A
        // long `/resume` transcript would otherwise commit every block in one
        // oversized batch and panic; the remainder now streams on the next
        // frame(s) via `committed`/`committed_rows`.
        let max_rows = render::max_commit_rows(width);
        let mut batch: Vec<Line<'static>> = Vec::new();
        if let Some(start) = self.scrollback_replay {
            let welcome = render::welcome_lines(&self.app);
            let cut = anchor::commit_take(welcome.len(), start, max_rows);
            batch.extend_from_slice(&welcome[cut.start..cut.start + cut.take]);
            self.scrollback_replay = if cut.drained {
                None
            } else {
                Some(cut.start + cut.take)
            };
        }
        let app = &mut self.app;
        // Defer commits while a screen-clear re-anchor is still pending.
        // Committing before the re-anchor lands would advance `committed` into
        // blocks the next Clear(All) wipes — a resumed transcript paints once,
        // gets erased, and never repaints. So we hold off; once the re-anchor
        // lands we commit the batch at once. Crucially, the episode is bounded
        // (`anchor::MAX_REANCHOR_ATTEMPTS`): if its DSR never answers we force
        // a DSR-free re-anchor and reopen the gate, so it can't wedge
        // rendering blank indefinitely (the "blank after input" bug).
        while self.scrollback_replay.is_none()
            && self.anchor.can_commit()
            && app.committed < app.blocks.len()
            && batch.len() < max_rows
        {
            let block = &app.blocks[app.committed];
            let done = app.block_done(app.committed);
            let rows = if done {
                // A finished thought commits its collapsed breadcrumb (lead +
                // "(N more lines, ctrl+o to expand)") unless expanded.
                match block {
                    UIBlock::Reasoning(t) if !app.reasoning_expanded => {
                        render::reasoning_collapsed_lines(t, width)
                    }
                    _ => render::block_lines(block, app.tick, &app.cwd, width),
                }
            } else if matches!(block, UIBlock::Reasoning(_)) && !app.reasoning_expanded {
                break; // collapsed thought still streaming: the live preview owns it
            } else if render::stream_commit::is_streamed(block) {
                render::stream_commit::stable_rows(block, app.tick, &app.cwd, width)
            } else {
                break; // pending tool: nothing to commit until it finalizes
            };
            // Take only what fits under this frame's row budget. A block taller
            // than the cap splits across frames: `committed` stays put until its
            // final row lands, with `committed_rows` marking the split point.
            let cut = anchor::commit_take(rows.len(), app.committed_rows, max_rows - batch.len());
            batch.extend_from_slice(&rows[cut.start..cut.start + cut.take]);
            if done && cut.drained {
                app.committed += 1;
                app.committed_rows = 0;
            } else {
                app.committed_rows = cut.start + cut.take;
                break; // in-progress block, or a finalized block split by the cap
            }
        }
        if !batch.is_empty() {
            let h = batch.len() as u16;
            self.terminal
                .insert_before(h, |buf| render::render_block_into(buf, &batch))?;
            self.diag.on_commit(batch.len());
        }
        Ok(())
    }

    /// Coalesced redraw of the live band: at most once per [`FRAME`] interval.
    fn draw_frame(&mut self) -> io::Result<()> {
        if self.last_draw.elapsed() < FRAME {
            return Ok(());
        }
        self.app.tick_update();
        self.diag.on_frame();
        // Rebuild the inline viewport when the band height changes (picker
        // open/close, multi-line input) OR the terminal resized. ratatui
        // 0.26's inline autoresize leaves the old band stranded in
        // scrollback (#77); taking over with clear()+fresh viewport
        // re-anchors cleanly.
        let term_size = crossterm::terminal::size()?;
        let want_rows = render::viewport_height(&self.app, term_size.0, term_size.1);
        // Band geometry: `Anchor::band_step` decides when to rebuild (height
        // change, terminal resize, or a post-resize settle that scrubs
        // conpty's late-reflow duplicates) and how much to wipe. A timed-out
        // DSR keeps the old viewport and retries next frame; the resize
        // marker is consumed only by a settle re-anchor that landed.
        if let Some(step) = self
            .anchor
            .band_step(self.epoch.elapsed(), want_rows, term_size)
        {
            match step.wipe {
                Some(Wipe::Band) => self.terminal.clear()?,
                Some(Wipe::All) => {
                    // A terminal resize can reflow the old inline band into
                    // scrollback before Ignis receives Event::Resize. There is
                    // no selective scrollback delete, so purge it and replay
                    // the welcome + App-owned transcript at the new width.
                    execute!(
                        io::stdout(),
                        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
                        crossterm::cursor::MoveTo(0, 0)
                    )?;
                    // The purge already happened even if the following DSR
                    // rebuild times out. Arm replay now so a tolerated timeout
                    // cannot leave App-owned history erased.
                    self.begin_scrollback_replay();
                }
                None => {}
            }
            let ok = try_rebuild(&mut self.terminal, step.want_rows)?;
            self.anchor.band_rebuilt(ok, step.want_rows, term_size);
        }
        draw_tolerant(&mut self.terminal, &mut self.app)?;
        self.last_draw = std::time::Instant::now();
        Ok(())
    }
}

/// Print a `ignis --resume <id>` hint below the live band, where the shell
/// prompt returns after exit. Best-effort: terminal I/O errors are swallowed —
/// the session already ended cleanly and a missing hint is harmless.
fn print_resume_hint(session_id: &str, band_bottom: u16) -> io::Result<()> {
    use crossterm::{
        cursor::MoveTo,
        style::{Color, Print, ResetColor, SetForegroundColor},
    };
    execute!(
        io::stdout(),
        // Land just under the band (clamped to the screen, scrolls if needed),
        // then a blank separator line before the hint.
        MoveTo(0, band_bottom),
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("Resume this session with:\r\n"),
        ResetColor,
        SetForegroundColor(Color::Rgb {
            r: 0xcb,
            g: 0xa6,
            b: 0xf7,
        }),
        Print(format!(
            "  ignis --resume {}\r\n",
            quote_session_id(session_id)
        )),
        ResetColor,
    )
}

/// Render a session id for the resume hint. Generated ids
/// (`session-<ts>-<hex>`) print bare; but `--resume <id>` accepts an arbitrary
/// user-supplied id verbatim, so one with spaces or shell metacharacters is
/// single-quoted to stay copy-pasteable.
fn quote_session_id(id: &str) -> String {
    let safe = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
    if safe {
        id.to_string()
    } else {
        format!("'{}'", id.replace('\'', r"'\''"))
    }
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
    use crate::console::frontend::protocol::TranscriptBlock;
    use crate::hooks::{HookRegistry, HookSpec, HooksConfig};
    use crate::{AgentEvent, Message};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// The core driver (PR #174, phase 1) end to end over the real
    /// `LocalTuiPort`: an agent event reaches the frontend as `Outbound::Event`,
    /// a tool picker request surfaces as `Outbound::Request`, and the frontend's
    /// `ClientCommand::Reply` resolves the blocked tool's oneshot by id.
    #[tokio::test]
    async fn core_driver_forwards_events_and_resolves_picker_replies() {
        use crate::console::picker::{PickerAnswer, PickerQuestion, PickerRequest, PickerResponse};

        fn question() -> PickerQuestion {
            PickerQuestion {
                question: "proceed?".to_string(),
                kind: "ask_user".to_string(),
                header: "Q".to_string(),
                multi_select: false,
                options: vec![],
                allow_other: true,
                text_input: false,
                mask: false,
            }
        }

        let (agent_tx, agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (tool_tx, tool_rx) = mpsc::channel::<PickerRequest>(4);
        let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(8);
        let (prompt_tx, mut prompt_rx) = mpsc::channel::<AgentRequest>(8);
        // Stand in for a live prompt's inject source so we can assert Inject
        // reaches it, the same way the agent task wires `set_inject_source`.
        let (inj_tx, mut inj_rx) = mpsc::channel::<String>(8);
        let active_inject: crate::console::keys::ActiveInject =
            std::sync::Arc::new(std::sync::Mutex::new(Some(inj_tx)));
        let (local_port, mut handle) = local_tui(8);
        let mut acceptor = Acceptor::new();
        acceptor.attach(Box::new(local_port));
        let hub = FrontendHub::new(
            "s1".to_string(),
            String::new(),
            String::new(),
            String::new(),
            Vec::new(),
            acceptor,
            RequestBroker::new(),
        );
        // A session store with one prior session, for the list/resume steps.
        let store = tempfile::TempDir::new().unwrap();
        let user_msg = |text: &str| Message {
            role: "user".to_string(),
            content: Some(text.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        };
        FileStorage::new(store.path().to_path_buf())
            .save_session("older-session", &[user_msg("hi from older")], None)
            .await
            .unwrap();
        let driver = tokio::spawn(drive_frontend_core(
            hub,
            agent_rx,
            tool_rx,
            cancel_tx,
            active_inject,
            prompt_tx,
            crate::permissions::runtime::PermissionState::new(crate::permissions::Mode::Off),
            std::sync::Arc::new(crate::skills::SkillRegistry::load(
                None,
                std::path::Path::new("/nonexistent-skills-test"),
                std::collections::HashSet::new(),
            )),
            crate::mcp::McpRegistry::empty(),
            "s1".to_string(),
            store.path().to_path_buf(),
        ));

        // 0) The driver greets the freshly-attached frontend with a snapshot.
        assert!(matches!(
            handle.outbound.recv().await,
            Some(Outbound::Snapshot(s)) if s.session_id == "s1"
        ));

        // 1) An agent event reaches the frontend verbatim as `Outbound::Event`.
        agent_tx.send(AgentEvent::TurnStart).await.unwrap();
        assert!(matches!(
            handle.outbound.recv().await,
            Some(Outbound::Event(e)) if matches!(*e, AgentEvent::TurnStart)
        ));

        // 2) A tool picker request surfaces as `Outbound::Request`; the
        //    frontend's `Reply` resolves the blocked tool's oneshot.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tool_tx
            .send(PickerRequest {
                questions: vec![question()],
                reply: reply_tx,
            })
            .await
            .unwrap();
        let id = match handle.outbound.recv().await {
            Some(Outbound::Request(req)) => {
                assert_eq!(req.questions.len(), 1);
                req.id
            }
            other => panic!("expected Outbound::Request, got {other:?}"),
        };
        handle
            .commands
            .send(ClientCommand::Reply {
                id,
                answer: ReplyAnswer::Answered(vec![PickerAnswer::Single("ok".to_string())]),
            })
            .await
            .unwrap();
        assert!(matches!(
            reply_rx.await.unwrap(),
            PickerResponse::Answered(v) if v == vec![PickerAnswer::Single("ok".to_string())]
        ));

        // 3) A `Cancel` command is forwarded to the agent task's cancel channel.
        handle.commands.send(ClientCommand::Cancel).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), cancel_rx.recv())
                .await
                .expect("cancel forwarded within timeout")
                .is_some(),
            "ClientCommand::Cancel reaches the agent task's cancel channel"
        );

        // 4) An `Inject` command reaches the live prompt's inject source.
        handle
            .commands
            .send(ClientCommand::Inject {
                text: "steer".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), inj_rx.recv())
                .await
                .expect("inject forwarded within timeout"),
            Some("steer".to_string()),
            "ClientCommand::Inject reaches the turn's inject source"
        );

        // 5) A `Submit` maps to an AgentRequest against the current session id.
        handle
            .commands
            .send(ClientCommand::Submit {
                text: "hello".to_string(),
            })
            .await
            .unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(5), prompt_rx.recv())
            .await
            .expect("submit forwarded within timeout")
        {
            Some(AgentRequest::Prompt { session_id, prompt }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(prompt, "hello");
            }
            Some(AgentRequest::Compact { .. }) => panic!("expected Prompt, got Compact"),
            None => panic!("expected Prompt, got nothing"),
            _ => panic!("expected Prompt, got SetModel/ReloadConfig/ReloadSkills"),
        }

        // 6) SetSession retargets the id; a later Submit lands in the new
        //    session, and "/compact" maps to Compact.
        handle
            .commands
            .send(ClientCommand::SetSession {
                session_id: "s2".to_string(),
            })
            .await
            .unwrap();
        handle
            .commands
            .send(ClientCommand::Submit {
                text: "/compact".to_string(),
            })
            .await
            .unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(5), prompt_rx.recv())
            .await
            .expect("compact forwarded within timeout")
        {
            Some(AgentRequest::Compact { session_id }) => assert_eq!(session_id, "s2"),
            Some(AgentRequest::Prompt { .. }) => panic!("expected Compact, got Prompt"),
            None => panic!("expected Compact, got nothing"),
            _ => panic!("expected Compact, got SetModel/ReloadConfig/ReloadSkills"),
        }

        // 7) `ListSessions` answers with the project's sessions, current one
        //    ("s2", set above) excluded; the seeded prior session is listed.
        handle
            .commands
            .send(ClientCommand::ListSessions)
            .await
            .unwrap();
        match handle.outbound.recv().await {
            Some(Outbound::Sessions(list)) => {
                assert!(list.iter().any(|s| s.id == "older-session"));
                assert!(
                    !list.iter().any(|s| s.id == "s2"),
                    "current session excluded"
                );
            }
            other => panic!("expected Outbound::Sessions, got {other:?}"),
        }

        // 8) `ResumeSession` replays the chosen session's transcript, then
        //    re-snapshots with the retargeted id; a later submit lands there.
        handle
            .commands
            .send(ClientCommand::ResumeSession {
                session_id: "older-session".to_string(),
            })
            .await
            .unwrap();
        match handle.outbound.recv().await {
            Some(Outbound::Transcript { session_id, blocks }) => {
                assert_eq!(session_id, "older-session");
                assert!(matches!(
                    blocks.as_slice(),
                    [TranscriptBlock::User { text }] if text == "hi from older"
                ));
            }
            other => panic!("expected Outbound::Transcript, got {other:?}"),
        }
        assert!(matches!(
            handle.outbound.recv().await,
            Some(Outbound::Snapshot(s)) if s.session_id == "older-session"
        ));
        handle
            .commands
            .send(ClientCommand::Submit {
                text: "next".to_string(),
            })
            .await
            .unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(5), prompt_rx.recv())
            .await
            .expect("submit after resume forwarded")
        {
            Some(AgentRequest::Prompt { session_id, .. }) => {
                assert_eq!(
                    session_id, "older-session",
                    "submit targets the resumed session"
                )
            }
            Some(_) => panic!("expected Prompt against resumed session, got another request"),
            None => panic!("expected Prompt against resumed session, got nothing"),
        }

        // 9) An explicit `Shutdown` ends the driver loop (not just an EOF drop).
        handle.commands.send(ClientCommand::Shutdown).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), driver)
            .await
            .expect("Shutdown ends the driver within the timeout")
            .expect("driver task joins cleanly");
        drop(handle);
    }

    #[tokio::test]
    async fn connect_wizard_drives_pickers_through_the_seam() {
        // `/connect` is engine-driven for the out-of-process frontend: a
        // submitted "/connect" opens the provider picker, and each reply
        // advances `ConnectFlow` to the next picker. Drive provider → API key
        // (masked) → model, then cancel — covering the routing without the
        // disk-writing final step (so no HOME mutation / coverage env-race).
        use crate::console::picker::{PickerAnswer, PickerRequest};
        let (_agent_tx, agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (_tool_tx, tool_rx) = mpsc::channel::<PickerRequest>(4);
        let (cancel_tx, _cancel_rx) = mpsc::channel::<()>(8);
        let (prompt_tx, _prompt_rx) = mpsc::channel::<AgentRequest>(8);
        let active_inject: crate::console::keys::ActiveInject =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let (local_port, mut handle) = local_tui(8);
        let mut acceptor = Acceptor::new();
        acceptor.attach(Box::new(local_port));
        let hub = FrontendHub::new(
            "s1".to_string(),
            String::new(),
            String::new(),
            String::new(),
            Vec::new(),
            acceptor,
            RequestBroker::new(),
        );
        let store = tempfile::TempDir::new().unwrap();
        let driver = tokio::spawn(drive_frontend_core(
            hub,
            agent_rx,
            tool_rx,
            cancel_tx,
            active_inject,
            prompt_tx,
            crate::permissions::runtime::PermissionState::new(crate::permissions::Mode::Off),
            std::sync::Arc::new(crate::skills::SkillRegistry::load(
                None,
                std::path::Path::new("/nonexistent-skills-test"),
                std::collections::HashSet::new(),
            )),
            crate::mcp::McpRegistry::empty(),
            "s1".to_string(),
            store.path().to_path_buf(),
        ));

        // Drain the greeting snapshot.
        assert!(matches!(
            handle.outbound.recv().await,
            Some(Outbound::Snapshot(_))
        ));

        // Recv the next Outbound, expecting a Request; return (id, questions).
        macro_rules! next_request {
            () => {
                match handle.outbound.recv().await {
                    Some(Outbound::Request(req)) => req,
                    other => panic!("expected Outbound::Request, got {other:?}"),
                }
            };
        }
        macro_rules! reply_single {
            ($id:expr, $text:expr) => {
                handle
                    .commands
                    .send(ClientCommand::Reply {
                        id: $id,
                        answer: ReplyAnswer::Answered(vec![PickerAnswer::Single(
                            $text.to_string(),
                        )]),
                    })
                    .await
                    .unwrap();
            };
        }

        // 1) "/connect" opens the provider picker.
        handle
            .commands
            .send(ClientCommand::Submit {
                text: "/connect".to_string(),
            })
            .await
            .unwrap();
        let provider_req = next_request!();
        assert_eq!(provider_req.questions[0].kind, "connect");
        assert!(provider_req.questions[0]
            .options
            .iter()
            .any(|o| o.label == "OpenAI"));

        // 2) Picking a key-required provider opens the masked API-key field.
        reply_single!(provider_req.id, "OpenAI");
        let key_req = next_request!();
        assert!(
            key_req.questions[0].text_input,
            "API-key step is text-input"
        );
        assert!(key_req.questions[0].mask, "API-key field is masked");

        // 3) Entering the key opens the model picker (no disk write yet).
        reply_single!(key_req.id, "sk-test");
        let model_req = next_request!();
        assert!(
            !model_req.questions[0].options.is_empty(),
            "model picker lists the provider's models"
        );

        // 4) Cancelling at the model step aborts with a notice — nothing persisted.
        handle
            .commands
            .send(ClientCommand::Reply {
                id: model_req.id,
                answer: ReplyAnswer::Cancelled,
            })
            .await
            .unwrap();
        assert!(matches!(
            handle.outbound.recv().await,
            Some(Outbound::Event(e)) if matches!(*e, AgentEvent::Notice { .. })
        ));

        handle.commands.send(ClientCommand::Shutdown).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), driver)
            .await
            .expect("Shutdown ends the driver")
            .expect("driver joins cleanly");
        drop(handle);
    }

    #[test]
    fn quote_session_id_keeps_generated_ids_bare_and_quotes_unsafe() {
        // Generated ids (`session-<ts>-<hex>`) print bare, matching the example.
        assert_eq!(
            quote_session_id("session-1700000000-ab12cd34"),
            "session-1700000000-ab12cd34"
        );
        // User-supplied `--resume <id>` reaches the hint verbatim; spaces and
        // shell metacharacters get single-quoted so a paste stays one argument.
        assert_eq!(quote_session_id("my work"), "'my work'");
        assert_eq!(quote_session_id("a;rm -rf x"), "'a;rm -rf x'");
        assert_eq!(quote_session_id("it's"), r"'it'\''s'");
        assert_eq!(quote_session_id(""), "''");
    }

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

    #[test]
    fn transcript_blocks_maps_user_assistant_and_attaches_tool_results() {
        use crate::{ToolCall, ToolCallFunction};
        let plain = |role: &str, content: &str| Message {
            role: role.to_string(),
            content: Some(content.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        };
        let mut assistant = assistant_msg("on it", Some("let me think"));
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call-1".to_string(),
            r#type: "function".to_string(),
            function: ToolCallFunction {
                name: "bash".to_string(),
                arguments: "ls".to_string(),
            },
        }]);
        let mut tool_result = plain("tool", r#"{"result":"file.txt","is_error":false}"#);
        tool_result.tool_call_id = Some("call-1".to_string());

        let blocks = transcript_blocks(vec![plain("user", "do it"), assistant, tool_result]);
        // user + reasoning (before the reply) + assistant + tool.
        assert!(matches!(&blocks[0], TranscriptBlock::User { text } if text == "do it"));
        assert!(
            matches!(&blocks[1], TranscriptBlock::Reasoning { text } if text == "let me think")
        );
        assert!(matches!(&blocks[2], TranscriptBlock::Assistant { text } if text == "on it"));
        match &blocks[3] {
            TranscriptBlock::Tool { name, args, result } => {
                assert_eq!(name, "bash");
                assert_eq!(args, "ls");
                assert_eq!(result.content, "file.txt");
                assert!(!result.is_error);
            }
            _ => panic!("expected the tool call's result attached to its block"),
        }
        assert_eq!(blocks.len(), 4, "reasoning + reply + tool, no stray blocks");
    }

    #[test]
    fn cursor_read_timeout_is_classified_transient() {
        // The exact string crossterm 0.27 returns when a DSR (`ESC[6n`) reply
        // doesn't arrive within its ~2s window. This must be treated as
        // recoverable so a slow-pty hiccup skips a frame instead of killing
        // the whole TUI.
        let e =
            std::io::Error::other("The cursor position could not be read within a normal duration");
        assert!(transient_cursor_read_error(&e));
    }

    #[test]
    fn real_io_errors_stay_fatal() {
        // A genuinely broken terminal/pipe must still propagate — only the
        // cursor-read timeout is swallowed.
        let pipe = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        assert!(!transient_cursor_read_error(&pipe));
        let other = std::io::Error::other("some other backend failure");
        assert!(!transient_cursor_read_error(&other));
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
                ..HookSpec::default()
            }],
            ..Default::default()
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
