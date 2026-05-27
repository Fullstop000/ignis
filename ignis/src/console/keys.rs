//! Keyboard input dispatch — the per-key handler the runner pumps. Splits
//! into two layers: the small `apply_edit_key` (editor-style cursor / paste
//! ops the input box always honors) and the big `handle_key` (mode-aware
//! routing: inline picker → global ESC/Ctrl-D/Ctrl-C → slash pickers →
//! busy-mode queue/steer → idle input).
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::console::app::{App, Mode};
use crate::console::format::{AgentRequest, SelectionDirection};
use crate::console::inline_picker;
use crate::console::slash::build_skill_prompt;
use crate::session::SessionManager;
use crate::storage::{FileStorage, SessionStorage};

/// Shared handle to the *current* prompt run's inject sender. `Some` iff a prompt
/// run is live and accepting `Ctrl+S` injects (and `Ctrl+C` cancels); `None`
/// during idle / `/compact` / provider setup.
pub(crate) type ActiveInject =
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<String>>>>;

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

pub(crate) async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    prompt_tx: &mpsc::Sender<AgentRequest>,
    cancel_tx: &mpsc::Sender<()>,
    active_inject: &ActiveInject,
    session_manager: &SessionManager,
    storage_dir: &std::path::Path,
) {
    // Inline (tool-initiated) picker captures ALL keys while open, including
    // ESC and Ctrl+C — must come before global handlers and the busy-mode
    // gate, because the picker is the only thing the user is interacting with.
    if let Some(state) = app.inline_picker.as_mut() {
        use crate::console::picker::PickerResponse;
        use inline_picker::KeyOutcome;
        let outcome = state.on_key(key);
        match outcome {
            KeyOutcome::Continue => {}
            KeyOutcome::Cancel => {
                if let Some(mut picker) = app.inline_picker.take() {
                    if let Some(reply) = picker.reply.take() {
                        let _ = reply.send(PickerResponse::Cancelled);
                    }
                }
            }
            KeyOutcome::Done(answers) => {
                if let Some(mut picker) = app.inline_picker.take() {
                    if let Some(reply) = picker.reply.take() {
                        let _ = reply.send(PickerResponse::Answered(answers));
                    }
                }
            }
        }
        // No live-trace flush: the tool call's UIBlock::Tool will commit through
        // block_lines → ask_user_resume_trace shortly after this, which renders
        // the same compact trace. Avoiding the double-emit Codex flagged (P2).
        return;
    }

    // Global
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            app.clear_exit_hint();
            app.session_picker = None;
            app.model_picker = None;
            app.skill_picker = None;
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

    if app.skill_picker.is_some() {
        match key.code {
            KeyCode::Up => {
                app.select_skill_picker(SelectionDirection::Previous);
                return;
            }
            KeyCode::Down => {
                app.select_skill_picker(SelectionDirection::Next);
                return;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                // The picker's [x]/[ ] checkbox is the feedback; don't emit a
                // notice into the transcript on every toggle.
                app.toggle_selected_skill();
                return;
            }
            KeyCode::Esc => {
                app.skill_picker = None;
                return;
            }
            KeyCode::Char(_) => {
                app.skill_picker = None;
            }
            _ => {}
        }
    }

    if app.mcp_picker.is_some() {
        match key.code {
            KeyCode::Up => {
                app.select_mcp_picker(SelectionDirection::Previous);
                return;
            }
            KeyCode::Down => {
                app.select_mcp_picker(SelectionDirection::Next);
                return;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                app.toggle_selected_mcp_server();
                return;
            }
            KeyCode::Esc => {
                app.mcp_picker = None;
                return;
            }
            KeyCode::Char(_) => {
                app.mcp_picker = None;
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
                } else if command == "/skills" && arg_count == 1 {
                    app.show_skill_picker();
                } else if command == "/mcp" && arg_count == 1 {
                    app.show_mcp_picker();
                } else if command.starts_with('/')
                    && app
                        .skills
                        .as_deref()
                        .map(|r| r.all().iter().any(|s| format!("/{}", s.name) == command))
                        .unwrap_or(false)
                {
                    let name = command.trim_start_matches('/').to_string();
                    let reg = app.skills.clone().unwrap();
                    if let Some(skill) = reg.get_enabled(&name) {
                        let args = text[command.len()..].trim();
                        let mut prompt = build_skill_prompt(&skill.name, &skill.body, args);
                        // Bundled-file skills get their directory + file list so
                        // the model can read referenced resources (no-op for
                        // pure-instruction skills).
                        if let Some(note) = skill.resources_note() {
                            prompt.push_str(&note);
                        }
                        app.turn_in_flight = true;
                        let _ = prompt_tx
                            .send(AgentRequest::Prompt {
                                session_id: app.session_id.clone(),
                                prompt,
                            })
                            .await;
                    } else {
                        app.add_assistant_notice(format!(
                            "Skill '{name}' is disabled. Enable it with /skills."
                        ));
                    }
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
