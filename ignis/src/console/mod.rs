use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, style::Color, Terminal};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::console::app::{App, Mode};
use crate::console::render::draw;
use crate::session::SessionManager;
use crate::storage::{FileStorage, SessionStorage};
use crate::{AgentEvent, Message, Session};

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

pub(crate) const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
        name: "/new",
        description: "Start a new session",
    },
    SlashCommand {
        name: "/clear",
        description: "Alias for /new",
    },
    SlashCommand {
        name: "/compact",
        description: "Summarize earlier history to free up context",
    },
];

// ==========================================
// UI State

pub mod app;
pub mod markdown;
pub mod render;

pub(crate) enum AgentRequest {
    Prompt { session_id: String, prompt: String },
    Compact { session_id: String },
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

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
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

pub async fn run_console(
    provider_name: String,
    model_name: String,
    session_id: String,
    system_prompt: String,
    storage_dir: std::path::PathBuf,
    cwd: PathBuf,
    config: crate::config::Config,
) -> Result<(), anyhow::Error> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(provider_name, model_name, session_id, cwd.clone());

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (prompt_tx, mut prompt_rx) = mpsc::channel::<AgentRequest>(8);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(8);
    let session_manager = SessionManager::new(storage_dir.clone());

    let agent_system_prompt = system_prompt;
    let agent_storage_dir = storage_dir.clone();
    let ui_storage_dir = storage_dir;
    let agent_cwd = cwd;
    let agent_config = config;

    // Background agent runner
    tokio::spawn(async move {
        while let Some(request) = prompt_rx.recv().await {
            let (session_id, prompt) = match request {
                AgentRequest::Prompt { session_id, prompt } => (session_id, Some(prompt)),
                AgentRequest::Compact { session_id } => (session_id, None),
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

            crate::tools::register_native_tools(
                &mut session,
                &agent_cwd,
                agent_config.web_search.clone(),
            );
            let ext_dirs = crate::tools::plugin::default_extension_dirs();
            let plugins = crate::tools::plugin::load_extensions(&ext_dirs);
            for plugin in plugins {
                session.register_tool(Arc::new(plugin));
            }

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
        while event::poll(std::time::Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                handle_key(
                    &mut app,
                    key,
                    &prompt_tx,
                    &cancel_tx,
                    &session_manager,
                    &ui_storage_dir,
                )
                .await;
            }
        }

        if app.should_quit {
            break;
        }

        // Coalesced redraw: at most once per frame interval.
        if last_draw.elapsed() >= frame {
            app.tick_update();
            terminal.draw(|f| draw(f, &mut app))?;
            last_draw = std::time::Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    prompt_tx: &mpsc::Sender<AgentRequest>,
    cancel_tx: &mpsc::Sender<()>,
    session_manager: &SessionManager,
    storage_dir: &std::path::Path,
) {
    // Global
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            app.clear_exit_hint();
            app.session_picker = None;
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
            if app.mode != Mode::Idle {
                let _ = cancel_tx.try_send(());
                app.mode = Mode::Idle;
                app.current_chunk_idx = None;
                app.stream_start = None;
                app.add_assistant_notice("Cancelled.".to_string());
            }
            return;
        }
        (m, KeyCode::Up) if m.contains(KeyModifiers::SHIFT) => {
            app.scroll_up(3);
            return;
        }
        (m, KeyCode::Down) if m.contains(KeyModifiers::SHIFT) => {
            app.scroll_down(3);
            return;
        }
        (_, KeyCode::PageUp) => {
            app.scroll_up(15);
            return;
        }
        (_, KeyCode::PageDown) => {
            app.scroll_down(15);
            return;
        }
        _ => {}
    }

    if app.mode != Mode::Idle {
        return;
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
                } else if (command == "/new" || command == "/clear") && arg_count == 1 {
                    let new_id = crate::session::SessionManager::create_id();
                    // Create an empty session file so /sessions can see it
                    let storage = crate::storage::FileStorage::new(storage_dir.to_path_buf());
                    let _ = storage.save_session(&new_id, &[], None).await;
                    app.start_new_session(new_id);
                } else if command == "/compact" && arg_count == 1 {
                    let _ = prompt_tx
                        .send(AgentRequest::Compact {
                            session_id: app.session_id.clone(),
                        })
                        .await;
                } else if text.starts_with('/') {
                    app.add_assistant_notice(format!("Unknown command `{}`.", command));
                } else {
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
        (_, KeyCode::Char(c)) => {
            app.clear_exit_hint();
            app.input.insert(app.cursor, c);
            app.cursor += 1;
            app.reset_slash_selection();
        }
        (_, KeyCode::Backspace) if app.cursor > 0 => {
            app.clear_exit_hint();
            app.cursor -= 1;
            app.input.remove(app.cursor);
            app.reset_slash_selection();
        }
        (_, KeyCode::Delete) if app.cursor < app.input.len() => {
            app.clear_exit_hint();
            app.input.remove(app.cursor);
            app.reset_slash_selection();
        }
        (_, KeyCode::Left) if app.cursor > 0 => {
            app.clear_exit_hint();
            app.cursor -= 1;
        }
        (_, KeyCode::Right) if app.cursor < app.input.len() => {
            app.clear_exit_hint();
            app.cursor += 1;
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
