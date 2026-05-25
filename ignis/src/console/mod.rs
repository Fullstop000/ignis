use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{backend::CrosstermBackend, style::Color, Terminal, TerminalOptions, Viewport};
use std::io;
use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::console::app::{App, Mode};
use crate::console::render::draw;
use crate::session::SessionManager;
use crate::storage::{FileStorage, SessionStorage};
use crate::{AgentEvent, Message, Session};

/// Shared handle to the *current* prompt run's inject sender. `Some` iff a prompt
/// run is live and accepting `Ctrl+S` injects (and `Ctrl+C` cancels); `None`
/// during idle / `/compact` / provider setup.
pub(crate) type ActiveInject =
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<String>>>>;

// ==========================================
// Color Palette
// ==========================================
pub(crate) const BG: Color = Color::Rgb(17, 17, 27);
pub(crate) const SURFACE: Color = Color::Rgb(24, 24, 37);
pub(crate) const SURFACE_2: Color = Color::Rgb(30, 30, 46);
pub(crate) const BORDER: Color = Color::Rgb(49, 50, 68);
pub(crate) const BORDER_ACTIVE: Color = Color::Rgb(137, 180, 250);
pub(crate) const TEXT: Color = Color::Rgb(205, 214, 244);
pub(crate) const TEXT_DIM: Color = Color::Rgb(108, 112, 134);
pub(crate) const SUBTEXT: Color = Color::Rgb(147, 153, 178);
pub(crate) const ACCENT: Color = Color::Rgb(137, 180, 250); // blue
pub(crate) const LAVENDER: Color = Color::Rgb(180, 190, 254);
pub(crate) const GREEN: Color = Color::Rgb(166, 227, 161);
pub(crate) const RED: Color = Color::Rgb(243, 139, 168);
pub(crate) const YELLOW: Color = Color::Rgb(249, 226, 175);
pub(crate) const PEACH: Color = Color::Rgb(250, 179, 135);
pub(crate) const TEAL: Color = Color::Rgb(148, 226, 213);
pub(crate) const MAUVE: Color = Color::Rgb(203, 166, 247);
pub(crate) const CODE_BG: Color = Color::Rgb(30, 30, 46);
// Solid diff backgrounds (added / removed lines), dark tints of green / red.
pub(crate) const DIFF_ADD_BG: Color = Color::Rgb(25, 46, 36);
pub(crate) const DIFF_DEL_BG: Color = Color::Rgb(51, 29, 37);

pub(crate) const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Playful status verbs cycled while the model is generating (Claude Code
/// style), for a livelier "Thinking" indicator. First entry is the default at t=0.
pub(crate) const THINKING_VERBS: &[&str] = &[
    "Thinking",
    "Pondering",
    "Noodling",
    "Cogitating",
    "Ruminating",
    "Marinating",
    "Percolating",
    "Nebulizing",
    "Conjuring",
    "Brewing",
    "Simmering",
    "Tinkering",
    "Scheming",
    "Synthesizing",
    "Incubating",
    "Galaxy-braining",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/resume",
        description: "List and resume sessions",
    },
    SlashCommand {
        name: "/clear",
        description: "Start a new session",
    },
    SlashCommand {
        name: "/compact",
        description: "Summarize earlier history to free up context",
    },
    SlashCommand {
        name: "/copy",
        description: "Copy the last assistant message to clipboard",
    },
    SlashCommand {
        name: "/model",
        description: "Switch model and reasoning effort",
    },
];

// ==========================================
// UI State

pub mod app;
pub mod clipboard;
pub mod highlight;
pub mod markdown;
pub mod render;

pub(crate) enum AgentRequest {
    Prompt {
        session_id: String,
        prompt: String,
    },
    Compact {
        session_id: String,
    },
    /// Switch the active provider/model/effort for subsequent prompts.
    SetModel {
        provider: String,
        model: String,
        effort: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SelectionDirection {
    Previous,
    Next,
}

// ==========================================
// Helpers
// ==========================================

pub(crate) fn format_duration(ms: u128) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Human-friendly token count: `999`, `1.5k`, `120k`.
pub(crate) fn format_tokens(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// Compact context-window label: `128K`, `256K`, `1M`. Providers quote windows
/// in both binary (262144 = "256K") and decimal (200000 = "200K", 1000000 =
/// "1M") units, so prefer whichever lands on a clean number.
pub(crate) fn format_context(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if n != 0 && n.is_multiple_of(MIB) {
        format!("{}M", n / MIB)
    } else if n != 0 && n.is_multiple_of(1024) {
        format!("{}K", n / 1024) // binary, e.g. 262144 -> 256K
    } else if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}K", (n as f64 / 1000.0).round() as u64) // decimal, e.g. 200000 -> 200K
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // Take whole chars, never a byte slice — `&s[..max]` panics mid-codepoint.
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Make arbitrary text (tool output, file contents, pasted input) safe to feed
/// to ratatui: a literal `\t` desyncs layout (the terminal advances to a tab
/// stop, ratatui assumes width 1) and other control chars (CR, ANSI escapes)
/// corrupt the screen. Expand tabs to spaces and drop the rest.
pub(crate) fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn slash_suggestions(input: &str) -> Vec<SlashCommand> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') || trimmed.contains(' ') {
        return Vec::new();
    }

    let query = trimmed.trim_start_matches('/').to_ascii_lowercase();
    let mut matches: Vec<(usize, usize, SlashCommand)> = SLASH_COMMANDS
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(idx, command)| {
            if query.is_empty() {
                return Some((0, idx, command));
            }
            let name = command.name.trim_start_matches('/').to_ascii_lowercase();
            let description = command.description.to_ascii_lowercase();
            if name.starts_with(&query) {
                Some((0, idx, command))
            } else if name.contains(&query) {
                Some((1, idx, command))
            } else if description.contains(&query) {
                Some((2, idx, command))
            } else {
                None
            }
        })
        .collect();
    matches.sort_by_key(|(rank, idx, _)| (*rank, *idx));
    matches.into_iter().map(|(_, _, command)| command).collect()
}

pub(crate) fn next_selection(current: usize, len: usize, direction: SelectionDirection) -> usize {
    if len == 0 {
        return 0;
    }
    match direction {
        SelectionDirection::Previous => {
            if current == 0 {
                len - 1
            } else {
                current - 1
            }
        }
        SelectionDirection::Next => (current + 1) % len,
    }
}

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

pub async fn run_console(
    provider_name: String,
    model_name: String,
    session_id: String,
    system_prompt: String,
    storage_dir: std::path::PathBuf,
    cwd: PathBuf,
    config: crate::config::Config,
) -> Result<(), anyhow::Error> {
    let mut app = App::new(provider_name, model_name, session_id, cwd.clone());
    // Context windows: config override → cached models.dev → compaction threshold.
    // The cache loads instantly; refresh runs in the background for next launch.
    let catalog = crate::models::catalog::load();
    app.fallback_context_window = config.compaction.threshold_tokens;
    app.set_context_window(
        config
            .active_context(&catalog)
            .map(|c| c as usize)
            .unwrap_or(config.compaction.threshold_tokens),
    );
    app.set_model_options(config.model_options(&catalog), config.active_effort());
    tokio::spawn(crate::models::catalog::refresh_if_stale());

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
    let active_inject: ActiveInject = std::sync::Arc::new(std::sync::Mutex::new(None));
    let active_inject_runner = active_inject.clone();
    let session_manager = SessionManager::new(storage_dir.clone());

    let agent_system_prompt = system_prompt;
    let agent_storage_dir = storage_dir.clone();
    let ui_storage_dir = storage_dir;
    let agent_cwd = cwd;
    let mut agent_config = config;

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

            crate::tools::register_native_tools(&mut session, &agent_cwd, &agent_config);

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
        // Wake on either the next frame deadline or an incoming agent event.
        tokio::select! {
            _ = tokio::time::sleep(frame) => {}
            Some(ev) = agent_rx.recv() => app.handle_event(ev),
        }

        // Drain any other pending agent events and key input — state only, no draw.
        while let Ok(ev) = agent_rx.try_recv() {
            app.handle_event(ev);
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

/// Apply a text-editing keystroke to the input. Shared by idle and busy modes.
/// Returns true if the key was consumed.
fn apply_edit_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            app.clear_exit_hint();
            app.input.clear();
            app.cursor = 0;
            app.reset_slash_selection();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            app.clear_exit_hint();
            app.cursor = 0;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            app.clear_exit_hint();
            app.cursor = app.input.len();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) if app.cursor > 0 => {
            app.clear_exit_hint();
            let before = &app.input[..app.cursor];
            let trimmed = before.trim_end();
            let new_end = trimmed
                .rfind(|c: char| c.is_whitespace())
                .map(|i| i + 1)
                .unwrap_or(0);
            app.input = format!("{}{}", &app.input[..new_end], &app.input[app.cursor..]);
            app.cursor = new_end;
        }
        (m, KeyCode::Char('j'))
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER) =>
        {
            app.clear_exit_hint();
            app.insert_char('\n');
            app.reset_slash_selection();
        }
        (_, KeyCode::Char(c)) => {
            app.clear_exit_hint();
            app.insert_char(c);
            app.reset_slash_selection();
        }
        (_, KeyCode::Backspace) if app.cursor > 0 => {
            app.clear_exit_hint();
            app.backspace();
            app.reset_slash_selection();
        }
        (_, KeyCode::Delete) if app.cursor < app.input.len() => {
            app.clear_exit_hint();
            app.delete_forward();
            app.reset_slash_selection();
        }
        (_, KeyCode::Left) if app.cursor > 0 => {
            app.clear_exit_hint();
            app.move_left();
        }
        (_, KeyCode::Right) if app.cursor < app.input.len() => {
            app.clear_exit_hint();
            app.move_right();
        }
        (_, KeyCode::Home) => {
            app.clear_exit_hint();
            app.cursor = 0;
        }
        (_, KeyCode::End) => {
            app.clear_exit_hint();
            app.cursor = app.input.len();
        }
        _ => return false,
    }
    true
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    prompt_tx: &mpsc::Sender<AgentRequest>,
    cancel_tx: &mpsc::Sender<()>,
    active_inject: &ActiveInject,
    session_manager: &SessionManager,
    storage_dir: &std::path::Path,
) {
    // Global
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            app.clear_exit_hint();
            app.session_picker = None;
            app.model_picker = None;
            return;
        }
        (m, KeyCode::Char('d')) if m.contains(KeyModifiers::CONTROL) => {
            app.request_exit();
            return;
        }
        (m, KeyCode::Char('c'))
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER) =>
        {
            app.clear_exit_hint();
            // Only cancellable while a prompt run is live. The resulting AgentEnd
            // drives the state transition + drain (keeps the queue).
            if active_inject.lock().unwrap().is_some() {
                let _ = cancel_tx.try_send(());
            }
            return;
        }
        _ => {}
    }

    // Ctrl+S: steer the live turn. Busy only — when idle there is no turn to
    // steer and the idle UI hides the queue (a queued item would vanish and only
    // fire at some later turn), so idle Ctrl+S is a no-op. We still `return` so it
    // never falls through to the idle handler and types a literal 's'.
    if matches!(key.code, KeyCode::Char('s')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.mode != Mode::Idle {
            let text = app.input.trim().to_string();
            if !text.is_empty() {
                let sender = active_inject.lock().unwrap().clone();
                match sender {
                    Some(tx) => match tx.try_send(text.clone()) {
                        Ok(()) => {
                            app.pending_injects.push(text);
                            app.input.clear();
                            app.cursor = 0;
                            app.reset_slash_selection();
                        }
                        Err(_) => {
                            app.error_flash = Some((
                                "Couldn't steer — try again".to_string(),
                                std::time::Instant::now(),
                            ));
                        }
                    },
                    // Busy but no live prompt run (e.g. /compact): queue it — the
                    // queue is visible while busy and drains at the next AgentEnd.
                    None => {
                        app.enqueue(text);
                        app.input.clear();
                        app.cursor = 0;
                        app.reset_slash_selection();
                    }
                }
            }
        }
        return;
    }

    // While busy: type to queue / steer. No pickers, no slash menu.
    if app.mode != Mode::Idle {
        match (key.modifiers, key.code) {
            (_, KeyCode::Enter) => {
                let text = app.input.trim().to_string();
                if !text.is_empty() {
                    app.enqueue(text);
                    app.input.clear();
                    app.cursor = 0;
                    app.reset_slash_selection();
                }
            }
            (_, KeyCode::Up) if app.input.is_empty() && !app.queue.is_empty() => {
                app.recall_last_queued();
            }
            (_, KeyCode::Up) => app.history_prev(),
            (_, KeyCode::Down) => app.history_next(),
            _ => {
                apply_edit_key(app, key);
            }
        }
        return;
    }

    if app.model_picker.is_some() {
        match key.code {
            KeyCode::Enter => {
                if let Some((provider, model, effort)) = app.apply_model_selection() {
                    let _ = prompt_tx.try_send(AgentRequest::SetModel {
                        provider: provider.clone(),
                        model: model.clone(),
                        effort: effort.clone(),
                    });
                    if let Err(e) =
                        crate::state::persist_model_selection(&provider, &model, effort.as_deref())
                    {
                        app.add_assistant_notice(format!("Switched (not saved: {e})"));
                    } else {
                        let suffix = effort
                            .as_deref()
                            .map(|e| format!(" · effort {e}"))
                            .unwrap_or_default();
                        app.add_assistant_notice(format!("Model → {provider}/{model}{suffix}"));
                    }
                }
                return;
            }
            KeyCode::Up => {
                app.select_model_picker(SelectionDirection::Previous);
                return;
            }
            KeyCode::Down => {
                app.select_model_picker(SelectionDirection::Next);
                return;
            }
            KeyCode::Left => {
                app.cycle_effort(SelectionDirection::Previous);
                return;
            }
            KeyCode::Right => {
                app.cycle_effort(SelectionDirection::Next);
                return;
            }
            KeyCode::Char(_) => {
                app.model_picker = None;
            }
            _ => {}
        }
    }

    if app.session_picker.is_some() {
        match key.code {
            KeyCode::Enter => {
                if let Some(session_id) = app.selected_session_id() {
                    let storage = FileStorage::new(storage_dir.to_path_buf());
                    match storage.load_session(&session_id).await {
                        Ok(messages) => app.render_session_history(session_id, messages),
                        Err(e) => app.add_assistant_notice(format!(
                            "Failed to load session `{}`: {}",
                            session_id, e
                        )),
                    }
                } else {
                    app.add_assistant_notice("No sessions found.".to_string());
                }
                return;
            }
            KeyCode::Up => {
                app.select_session_picker(SelectionDirection::Previous);
                return;
            }
            KeyCode::Down => {
                app.select_session_picker(SelectionDirection::Next);
                return;
            }
            KeyCode::Char(_) => {
                app.session_picker = None;
            }
            _ => {}
        }
    }

    match (key.modifiers, key.code) {
        (_, KeyCode::Enter) => {
            let text = if let Some(cmd) = app.selected_slash_command() {
                app.input = cmd;
                app.submit().unwrap_or_default()
            } else {
                app.submit().unwrap_or_default()
            };
            if !text.is_empty() {
                let command = text.split_whitespace().next().unwrap_or("");
                let arg_count = text.split_whitespace().count();
                if command == "/resume" && arg_count == 1 {
                    let mut sessions = session_manager.list();
                    if !sessions.iter().any(|s| s.id == app.session_id) {
                        let user_count = app
                            .blocks
                            .iter()
                            .filter(|b| matches!(b, crate::console::app::UIBlock::User(_)))
                            .count();
                        let preview = app
                            .blocks
                            .iter()
                            .find_map(|b| match b {
                                crate::console::app::UIBlock::User(text) => Some(text.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        sessions.push(crate::session::SessionMeta {
                            id: app.session_id.clone(),
                            message_count: user_count,
                            last_modified: u64::MAX,
                            preview,
                            start_dir: Some(app.cwd.to_string_lossy().to_string()),
                        });
                        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
                    }
                    app.show_session_picker(sessions);
                } else if command == "/clear" && arg_count == 1 {
                    let new_id = crate::session::SessionManager::create_id();
                    // Create an empty session file so /sessions can see it
                    let storage = crate::storage::FileStorage::new(storage_dir.to_path_buf());
                    let _ = storage.save_session(&new_id, &[], None).await;
                    app.start_new_session(new_id);
                } else if command == "/compact" && arg_count == 1 {
                    app.turn_in_flight = true;
                    let _ = prompt_tx
                        .send(AgentRequest::Compact {
                            session_id: app.session_id.clone(),
                        })
                        .await;
                } else if command == "/copy" && arg_count == 1 {
                    app.copy_last_assistant_message();
                } else if command == "/model" && arg_count == 1 {
                    app.show_model_picker();
                } else if text.starts_with('/') {
                    app.add_assistant_notice(format!("Unknown command `{}`.", command));
                } else {
                    app.turn_in_flight = true;
                    let _ = prompt_tx
                        .send(AgentRequest::Prompt {
                            session_id: app.session_id.clone(),
                            prompt: text,
                        })
                        .await;
                }
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            app.clear_exit_hint();
            app.input.clear();
            app.cursor = 0;
            app.reset_slash_selection();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            app.clear_exit_hint();
            app.cursor = 0;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            app.clear_exit_hint();
            app.cursor = app.input.len();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) if app.cursor > 0 => {
            app.clear_exit_hint();
            // Delete word backward
            let before = &app.input[..app.cursor];
            let trimmed = before.trim_end();
            let new_end = trimmed
                .rfind(|c: char| c.is_whitespace())
                .map(|i| i + 1)
                .unwrap_or(0);
            app.input = format!("{}{}", &app.input[..new_end], &app.input[app.cursor..]);
            app.cursor = new_end;
        }
        (_, KeyCode::Up) if !app.slash_suggestions().is_empty() => {
            app.select_slash_suggestion(SelectionDirection::Previous);
        }
        (_, KeyCode::Up) => {
            app.history_prev();
        }
        (_, KeyCode::Down) if !app.slash_suggestions().is_empty() => {
            app.select_slash_suggestion(SelectionDirection::Next);
        }
        (_, KeyCode::Down) => {
            app.history_next();
        }
        (m, KeyCode::Char('j'))
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER) =>
        {
            // Ctrl/Cmd+J inserts a newline (Enter still submits).
            app.clear_exit_hint();
            app.insert_char('\n');
            app.reset_slash_selection();
        }
        (_, KeyCode::Char(c)) => {
            app.clear_exit_hint();
            app.insert_char(c);
            app.reset_slash_selection();
        }
        (_, KeyCode::Backspace) if app.cursor > 0 => {
            app.clear_exit_hint();
            app.backspace();
            app.reset_slash_selection();
        }
        (_, KeyCode::Delete) if app.cursor < app.input.len() => {
            app.clear_exit_hint();
            app.delete_forward();
            app.reset_slash_selection();
        }
        (_, KeyCode::Left) if app.cursor > 0 => {
            app.clear_exit_hint();
            app.move_left();
        }
        (_, KeyCode::Right) if app.cursor < app.input.len() => {
            app.clear_exit_hint();
            app.move_right();
        }
        (_, KeyCode::Home) => {
            app.clear_exit_hint();
            app.cursor = 0;
        }
        (_, KeyCode::End) => {
            app.clear_exit_hint();
            app.cursor = app.input.len();
        }
        _ => {}
    }
}
