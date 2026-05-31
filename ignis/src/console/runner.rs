//! Console event loop. Owns the terminal handle, the background-agent
//! channels (`prompt_tx`/`agent_rx`/`cancel_tx`/`picker_tx`/inject), and the
//! per-frame draw + key-poll cycle. Everything stateful about the live UI
//! flows through here; `App` is the in-memory model the loop drives.
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{backend::CrosstermBackend, Terminal, TerminalOptions, Viewport};
use std::io;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::console::app::App;
use crate::console::format::AgentRequest;
use crate::console::inline_picker;
use crate::console::keys::{handle_key, ActiveInject};
use crate::console::render::{self, draw};
use crate::session::SessionManager;
use crate::{AgentEvent, Message, Session};

/// Create an inline-viewport terminal over stdout. Recreated when the live band
/// needs to grow/shrink (the inline height is fixed per `Terminal`).
fn make_inline_terminal(height: u16) -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height.max(2)),
        },
    )
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

    // Render inline in the normal buffer (no alternate screen, no mouse capture):
    // finished transcript blocks are pushed into the terminal's real scrollback
    // via `insert_before`, so tmux/native scroll can page through history. Only a
    // small live band (input + status + footer) is repainted.
    enable_raw_mode()?;
    let term_rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24);
    let mut cur_vh = render::live_height(&app, term_rows);
    let mut terminal = make_inline_terminal(cur_vh)?;

    // Welcome banner: committed once to scrollback.
    {
        let w = terminal.size()?.width;
        let lines = render::welcome_lines(&app);
        let h = render::block_height(&lines, w).max(1);
        terminal.insert_before(h, |buf| render::render_block_into(buf, &lines))?;
    }

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (prompt_tx, mut prompt_rx) = mpsc::channel::<AgentRequest>(8);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(8);
    // Tool → console: the `ask_user` tool sends a PickerRequest when the model
    // wants to ask the user something mid-turn. Capacity 4 — pickers serialize
    // (one open at a time); the buffer just decouples send from console drain.
    let (picker_tx, mut picker_rx) = mpsc::channel::<crate::console::picker::PickerRequest>(4);
    let picker_tx_runner = picker_tx.clone();
    let active_inject: ActiveInject = std::sync::Arc::new(std::sync::Mutex::new(None));
    let active_inject_runner = active_inject.clone();
    let session_manager = SessionManager::new(storage_dir.clone());

    let agent_system_prompt = system_prompt;
    let agent_storage_dir = storage_dir.clone();
    let ui_storage_dir = storage_dir;
    let agent_cwd = cwd;
    let mut agent_config = config;

    let ui_skill_registry = skill_registry.clone();
    let runner_skill_registry = skill_registry.clone();
    app.skills = Some(ui_skill_registry);

    let runner_mcp_registry = mcp_registry.clone();
    app.mcp = Some(mcp_registry);

    let runner_permissions = permissions.clone();
    app.permissions = Some(permissions);

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
                    let _ = agent_tx.send(AgentEvent::AgentEnd).await;
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
                    let _ = agent_tx.send(AgentEvent::AgentEnd).await;
                    log::error!("Session open error: {}", e);
                    continue;
                }
            };
            session.set_compaction(agent_config.compaction.clone());

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
                                let _ = agent_tx.send(AgentEvent::AgentEnd).await;
                                log::error!("Agent error: {}", e);
                            }
                        }
                        _ = cancel_rx.recv() => {
                            let _ = agent_tx.send(AgentEvent::AgentEnd).await;
                        }
                    }
                    *active_inject_runner.lock().unwrap() = None;
                }
                None => {
                    // /compact: summarize earlier history and report a notice.
                    let _ = agent_tx.send(AgentEvent::AgentStart).await;
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
                    let _ = agent_tx.send(AgentEvent::AgentEnd).await;
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
            Some(ev) = agent_rx.recv() => app.handle_event(ev),
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
            app.handle_event(ev);
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

        // Edge-triggered: exactly one queued prompt per turn-end (AgentEnd).
        if app.take_turn_just_ended() {
            if let Some(text) = app.take_queued_front() {
                app.push_user_prompt(text.clone());
                app.turn_in_flight = true;
                let _ = prompt_tx
                    .send(AgentRequest::Prompt {
                        session_id: app.session_id.clone(),
                        prompt: text,
                    })
                    .await;
            }
        }

        while event::poll(std::time::Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                handle_key(
                    &mut app,
                    key,
                    &prompt_tx,
                    &cancel_tx,
                    &active_inject,
                    &session_manager,
                    &ui_storage_dir,
                    &picker_tx,
                )
                .await;
            }
        }

        if app.should_quit {
            break;
        }

        // The inline viewport height is fixed per terminal, so recreate it when
        // the live band needs to grow/shrink (pickers, slash menu, taller input).
        let term_rows = terminal.size()?.height;
        let want_vh = render::live_height(&app, term_rows);
        if want_vh != cur_vh {
            // Clear the current live band first: this erases its rows and parks
            // the cursor at the band's top, so the rebuilt viewport re-anchors
            // there instead of leaving stale rows or pushing live UI into
            // scrollback.
            terminal.clear()?;
            terminal = make_inline_terminal(want_vh)?;
            cur_vh = want_vh;
        }

        // Flush newly-finalized leading blocks into the terminal scrollback.
        let width = terminal.size()?.width;
        while app.committed < app.blocks.len() && app.block_done(app.committed) {
            let lines = render::block_lines(&app.blocks[app.committed], app.tick, &app.cwd, width);
            if !lines.is_empty() {
                let h = render::block_height(&lines, width).max(1);
                terminal.insert_before(h, |buf| render::render_block_into(buf, &lines))?;
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

    disable_raw_mode()?;
    terminal.show_cursor()?;
    // Drop below the live band so the shell prompt starts on a fresh line.
    execute!(terminal.backend_mut(), crossterm::style::Print("\n"))?;
    Ok(())
}
