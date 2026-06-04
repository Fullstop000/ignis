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
use crate::storage::{FileStorage, SessionStorage};

/// Shared handle to the *current* prompt run's inject sender. `Some` iff a prompt
/// run is live and accepting `Ctrl+S` injects (and `Ctrl+C` cancels); `None`
/// during idle / `/compact` / provider setup.
pub(crate) type ActiveInject =
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<String>>>>;

/// Hint shown when the user tries to do anything that needs a provider in
/// no-provider mode. Kept short because the welcome banner already explains
/// the situation — this just says what to type next.
const NO_PROVIDER_HINT: &str = "Run /connect first.";

/// Built-in commands that route work to the LLM and therefore need an active
/// provider. New provider-needing commands go here (one place to update).
/// `/connect` itself is NOT here — it's the way out of no-provider mode.
const PROVIDER_REQUIRED_BUILTINS: &[&str] = &["/compact", "/copy", "/model"];

/// Provider-independent built-ins that bypass the no-provider gate. These were
/// dispatched *before* the gate in the original ladder, so they must run even
/// when a same-named skill is registered — `/connect` above all, since it is
/// the only way out of no-provider mode.
const PROVIDER_GATE_EXEMPT: &[&str] = &["/sessions", "/clear", "/connect"];

/// `true` iff submitting `command` in no-provider mode should be blocked
/// with [`NO_PROVIDER_HINT`]. Covers: plain prompts to the agent (no slash),
/// the built-in LLM commands above, and any `/skill-name` registered in the
/// skill registry (skill commands run the agent with an injected prompt).
fn requires_provider(app: &App, command: &str) -> bool {
    if !command.starts_with('/') {
        return true;
    }
    if PROVIDER_REQUIRED_BUILTINS.contains(&command) {
        return true;
    }
    is_skill_command(app, command)
}

/// `true` iff `command` (a leading-slash token like `/foo`) names a registered
/// skill — used both to gate no-provider mode and to route the skill's prompt.
fn is_skill_command(app: &App, command: &str) -> bool {
    app.skills
        .as_deref()
        .is_some_and(|r| r.all().iter().any(|s| format!("/{}", s.name) == command))
}

/// Returns true if the key was consumed.
fn apply_edit_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            app.clear_exit_hint();
            app.input.clear();
            app.cursor = 0;
            app.pending_pastes.clear();
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

/// Route a bracketed-paste event. An open inline picker (e.g. an API key typed
/// into `/connect`) takes the text inline; otherwise it goes to the composer,
/// where a multi-line block collapses into a `[ pasted-text#N … ]` chip.
pub(crate) fn handle_paste(app: &mut App, data: String) {
    if let Some(picker) = app.inline_picker.as_mut() {
        picker.paste_text(&data);
    } else {
        app.handle_paste(data);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    prompt_tx: &mpsc::Sender<AgentRequest>,
    cancel_tx: &mpsc::Sender<()>,
    active_inject: &ActiveInject,
    storage_dir: &std::path::Path,
    picker_tx: &mpsc::Sender<crate::console::picker::PickerRequest>,
    notice_tx: &mpsc::Sender<String>,
) {
    // Inline (tool-initiated) picker captures ALL keys while open, including
    // ESC and Ctrl+C — must come before global handlers and the busy-mode
    // gate, because the picker is the only thing the user is interacting with.
    if let Some(state) = app.inline_picker.as_mut() {
        use crate::console::app::ConnectAdvance;
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
                // If the user cancelled mid-`/connect`, drop the draft so the
                // next /connect starts clean. Tool-initiated cancels (no
                // draft) are no-ops here.
                app.cancel_connect();
            }
            KeyOutcome::Done(answers) => {
                if let Some(mut picker) = app.inline_picker.take() {
                    if let Some(reply) = picker.reply.take() {
                        let _ = reply.send(PickerResponse::Answered(answers.clone()));
                    }
                }
                // Multi-step `/connect`: route the answer back into the draft
                // state machine. The advance returns the next picker to open
                // (steps 1 and 2) or signals success (step 3 persisted).
                if app.connect_draft.is_some() {
                    match app.advance_connect(answers) {
                        ConnectAdvance::NextPicker(req) => {
                            let _ = picker_tx.send(req).await;
                        }
                        ConnectAdvance::Saved => {
                            // The agent loop's in-memory config is stale —
                            // it doesn't know about the api_key we just
                            // wrote. Reload from disk so the next prompt
                            // resolves with the new credentials. `send` over
                            // `try_send`: a full prompt queue should backpressure
                            // here, not silently drop the reload (the user would
                            // see a misleading "✓ Connected" followed by a
                            // stale-config error on their next prompt).
                            let _ = prompt_tx.send(AgentRequest::ReloadConfig).await;
                        }
                        ConnectAdvance::Failed => {}
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
            // Only cancellable while a prompt run is live. The resulting TurnEnd
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
            let text = app.expand_pastes(app.input.trim().to_string());
            if !text.is_empty() {
                let sender = active_inject.lock().unwrap().clone();
                match sender {
                    Some(tx) => match tx.try_send(text.clone()) {
                        Ok(()) => {
                            app.pending_injects.push(text);
                            app.input.clear();
                            app.cursor = 0;
                            app.pending_pastes.clear();
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
                    // queue is visible while busy and drains at the next TurnEnd.
                    None => {
                        app.enqueue(text);
                        app.input.clear();
                        app.cursor = 0;
                        app.pending_pastes.clear();
                        app.reset_slash_selection();
                    }
                }
            }
        }
        return;
    }

    // While busy: type to queue / steer. Slash autocomplete IS shown (queued
    // slash commands now run on drain), but other pickers stay closed.
    if app.mode != Mode::Idle {
        match (key.modifiers, key.code) {
            (_, KeyCode::Enter) => {
                // Enter on a highlighted slash suggestion commits the suggestion
                // into the input first, so the queued line is the full command
                // (e.g. "/com" + Enter → "/compact" queued).
                if let Some(cmd) = app.selected_slash_command() {
                    app.input = cmd;
                }
                let text = app.expand_pastes(app.input.trim().to_string());
                if !text.is_empty() {
                    app.enqueue(text);
                    app.input.clear();
                    app.cursor = 0;
                    app.pending_pastes.clear();
                    app.reset_slash_selection();
                }
            }
            (_, KeyCode::Up) if !app.slash_suggestions().is_empty() => {
                app.select_slash_suggestion(SelectionDirection::Previous);
            }
            (_, KeyCode::Down) if !app.slash_suggestions().is_empty() => {
                app.select_slash_suggestion(SelectionDirection::Next);
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

    if let Some(picker) = app.session_picker.as_ref() {
        let in_detail = picker.is_detail();
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
            KeyCode::Up if !in_detail => {
                app.select_session_picker(SelectionDirection::Previous);
                return;
            }
            KeyCode::Down if !in_detail => {
                app.select_session_picker(SelectionDirection::Next);
                return;
            }
            // → drills the highlighted row into the detail panel (no-op if
            // already there or if the row has no persisted JSONL yet).
            KeyCode::Right if !in_detail => {
                if let Some(p) = app.session_picker.as_mut() {
                    p.enter_detail();
                }
                return;
            }
            // ← / Esc: pop Detail back to List, or close the picker from List.
            KeyCode::Left if in_detail => {
                if let Some(p) = app.session_picker.as_mut() {
                    p.exit_detail();
                }
                return;
            }
            KeyCode::Esc => {
                if in_detail {
                    if let Some(p) = app.session_picker.as_mut() {
                        p.exit_detail();
                    }
                } else {
                    app.session_picker = None;
                }
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
                submit_text(app, text, prompt_tx, picker_tx, notice_tx, storage_dir).await;
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            app.clear_exit_hint();
            app.input.clear();
            app.cursor = 0;
            app.pending_pastes.clear();
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
        (m, KeyCode::Home) if m.contains(KeyModifiers::CONTROL) => {
            app.scroll_transcript_to_top();
        }
        (m, KeyCode::End) if m.contains(KeyModifiers::CONTROL) => {
            app.scroll_transcript_to_bottom();
        }
        // Empty input → End follows the transcript to the bottom (matches the
        // "(End to follow)" hint in `render_transcript`); PgUp/PgDn-when-empty
        // uses the same idea. Falls through to cursor-end when the user is
        // editing input.
        (_, KeyCode::End) if app.input.is_empty() => {
            app.scroll_transcript_to_bottom();
        }
        (_, KeyCode::Home) => {
            app.clear_exit_hint();
            app.cursor = 0;
        }
        (_, KeyCode::End) => {
            app.clear_exit_hint();
            app.cursor = app.input.len();
        }
        // Scroll the transcript when the input is empty so PgUp/PgDn don't
        // collide with multi-line editing. 10 lines per press matches CC.
        (_, KeyCode::PageUp) if app.input.is_empty() => {
            app.scroll_transcript_up(10);
        }
        (_, KeyCode::PageDown) if app.input.is_empty() => {
            // 10 lines per press matches CC. Pass the real visible-row count
            // (last-recorded by render_transcript) so the "at natural bottom"
            // detection in scroll_transcript_down re-enables auto-follow
            // correctly on tall terminals.
            let visible = app.transcript_visible_rows.max(1);
            app.scroll_transcript_down(10, visible);
        }
        _ => {}
    }
}

/// Dispatch a submitted line — either typed at idle (Enter) or drained
/// from the busy-mode queue at the next `TurnEnd`. Routes built-in slash
/// commands, skill commands, and plain prompts. Centralizing this is what
/// lets queued slash commands (e.g. `/compact`, `/model`) actually run on
/// drain instead of being sent to the LLM as a literal user message.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn submit_text(
    app: &mut App,
    text: String,
    prompt_tx: &mpsc::Sender<AgentRequest>,
    picker_tx: &mpsc::Sender<crate::console::picker::PickerRequest>,
    notice_tx: &mpsc::Sender<String>,
    storage_dir: &std::path::Path,
) {
    let command = text.split_whitespace().next().unwrap_or("");
    let arg_count = text.split_whitespace().count();

    // No-provider gate: block anything that would talk to the LLM (plain
    // prompts, the built-in LLM commands, skill commands) until /connect runs.
    // PROVIDER_GATE_EXEMPT commands bypass it even when a same-named skill is
    // registered (they were dispatched before this gate in the original ladder).
    if app.provider.is_empty()
        && !PROVIDER_GATE_EXEMPT.contains(&command)
        && requires_provider(app, command)
    {
        app.add_assistant_notice(NO_PROVIDER_HINT.to_string());
        return;
    }

    match command {
        "/sessions" if arg_count == 1 => {
            let projects_dir = match dirs::home_dir() {
                Some(h) => h.join(".ignis/projects"),
                None => {
                    app.add_assistant_notice("Could not locate home directory.".to_string());
                    return;
                }
            };
            let mut records = crate::cli::sessions::walk_sessions(
                &projects_dir,
                crate::cli::sessions::Scope::Current,
                &app.cwd,
            )
            .unwrap_or_default();
            if !records.iter().any(|r| r.session_id == app.session_id) {
                let user_count = app
                    .blocks
                    .iter()
                    .filter(|b| matches!(b, crate::console::app::UIBlock::User(_)))
                    .count() as u64;
                records.push(crate::cli::sessions::SessionRecord {
                    session_id: app.session_id.clone(),
                    project_slug: crate::session::project_slug(&app.cwd),
                    project_start_dir: Some(app.cwd.to_string_lossy().to_string()),
                    last_modified: Some(u64::MAX),
                    user_queries: user_count,
                    ..Default::default()
                });
            }
            records.sort_by_key(|r| std::cmp::Reverse(r.last_modified.unwrap_or(0)));
            app.show_session_picker(records, projects_dir);
        }
        "/clear" if arg_count == 1 => {
            let new_id = crate::session::SessionManager::create_id();
            let storage = crate::storage::FileStorage::new(storage_dir.to_path_buf());
            let _ = storage.save_session(&new_id, &[], None).await;
            app.start_new_session(new_id);
        }
        "/connect" if arg_count == 1 => {
            if let Some(req) = app.start_connect() {
                let _ = picker_tx.send(req).await;
            }
        }
        "/compact" if arg_count == 1 => {
            app.turn_in_flight = true;
            let _ = prompt_tx
                .send(AgentRequest::Compact {
                    session_id: app.session_id.clone(),
                })
                .await;
        }
        "/copy" if arg_count == 1 => app.copy_last_assistant_message(),
        "/model" if arg_count == 1 => app.show_model_picker(),
        "/skills" if arg_count == 1 => app.show_skill_picker(),
        "/mcp" if arg_count == 1 => app.show_mcp_picker(),
        "/afk" if arg_count == 1 => handle_afk_toggle(app, picker_tx, notice_tx).await,
        "/telemetry" if arg_count == 1 => handle_telemetry_picker(app, picker_tx, notice_tx).await,
        "/hooks" => handle_hooks_command(app, &text).await,
        cmd if is_skill_command(app, cmd) => {
            let name = cmd.trim_start_matches('/').to_string();
            let reg = app.skills.clone().unwrap();
            if let Some(skill) = reg.get_enabled(&name) {
                let args = text[cmd.len()..].trim();
                let mut prompt = build_skill_prompt(&skill.name, &skill.body, args);
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
        }
        _ if text.starts_with('/') => {
            app.add_assistant_notice(format!("Unknown command `{}`.", command));
        }
        _ => {
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

/// `/hooks` — currently only the `reload` subcommand. Re-reads
/// `~/.ignis/hooks.json`, swaps the parsed config into the shared
/// registry, and posts an `[info]` or `[err]` line to scrollback. The
/// security reminder rides along on success so the user keeps noticing
/// it every time they touch the file.
async fn handle_hooks_command(app: &mut App, text: &str) {
    let mut parts = text.split_whitespace();
    let _ = parts.next(); // "/hooks"
    let sub = parts.next().unwrap_or("");
    if sub != "reload" {
        app.add_assistant_notice(
            "Usage: /hooks reload — re-reads ~/.ignis/hooks.json into memory.".to_string(),
        );
        return;
    }
    let Some(reg) = app.hooks.clone() else {
        app.add_assistant_notice("[err] hooks registry unavailable in this session.".to_string());
        return;
    };
    let Some(home) = dirs::home_dir() else {
        app.add_assistant_notice("[err] could not locate home directory.".to_string());
        return;
    };
    match reg.reload(&home).await {
        Ok(count) => app.add_assistant_notice(format!(
            "[info] reloaded {count} hook{plural} \u{00b7} run unsandboxed; audit before installing",
            plural = if count == 1 { "" } else { "s" }
        )),
        Err(e) => app.add_assistant_notice(format!("[err] /hooks reload: {e}")),
    }
}

/// `/telemetry` — open a TUI picker to enable or disable OpenTelemetry
/// export. Writes the choice to `~/.ignis/config.toml` (takes effect on
/// next restart) and shows the updated status.
async fn handle_telemetry_picker(
    app: &mut App,
    picker_tx: &mpsc::Sender<crate::console::picker::PickerRequest>,
    notice_tx: &mpsc::Sender<String>,
) {
    use crate::console::picker::{
        PickerAnswer, PickerOption, PickerQuestion, PickerRequest, PickerResponse,
    };
    use tokio::sync::oneshot;

    if app.inline_picker.is_some() {
        app.add_assistant_notice("/telemetry: another picker is open; close it first.".to_string());
        return;
    }

    let s = crate::telemetry::state_snapshot();
    let currently = if s.enabled { "On" } else { "Off" };

    let (reply_tx, reply_rx) = oneshot::channel();
    let request = PickerRequest {
        questions: vec![PickerQuestion {
            question: format!("Telemetry is currently {}. Enable or disable?", currently),
            kind: "telemetry".to_string(),
            header: "Telemetry".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: false,
            mask: false,
            options: vec![
                PickerOption {
                    label: "On".to_string(),
                    description: "Enable OpenTelemetry export (default). Spans and metrics are \
                                  sent to the configured OTLP endpoint."
                        .to_string(),
                    preview: None,
                },
                PickerOption {
                    label: "Off".to_string(),
                    description: "Disable OpenTelemetry export. No telemetry data will be sent."
                        .to_string(),
                    preview: None,
                },
            ],
        }],
        reply: reply_tx,
    };

    if picker_tx.send(request).await.is_err() {
        app.add_assistant_notice("/telemetry: picker channel closed".to_string());
        return;
    }

    // Spawn so the key handler returns immediately; persist on reply.
    let notice_tx_clone = notice_tx.clone();
    tokio::spawn(async move {
        if let Ok(PickerResponse::Answered(answers)) = reply_rx.await {
            if let Some(PickerAnswer::Single(label)) = answers.first() {
                let enable = label == "On";
                persist_telemetry_setting(enable);
                let _ = notice_tx_clone
                    .send(format!(
                        "Telemetry → {}.",
                        if enable { "On" } else { "Off" }
                    ))
                    .await;
            }
        }
    });
}

/// Write the `[telemetry] enabled` flag to `~/.ignis/config.toml`.
fn persist_telemetry_setting(enable: bool) {
    let config_path = match dirs::home_dir() {
        Some(h) => h.join(".ignis/config.toml"),
        None => return,
    };

    let content = if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(_) => return,
        }
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut = match content.parse() {
        Ok(d) => d,
        Err(_) => return,
    };

    let telemetry = doc.entry("telemetry").or_insert(toml_edit::table());
    if let Some(table) = telemetry.as_table_mut() {
        table.set_implicit(false);
        table["enabled"] = toml_edit::value(enable);
    }

    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&config_path, doc.to_string());
}

/// `/afk` toggle handler. When AFK is currently on, flips it off immediately
/// (disabling strictly increases safety, no confirmation needed). When off,
/// opens a 2-option picker asking *which level* of AFK to enable: fully
/// unattended (dismiss `ask_user`, hard-deny safety floor) or hands-free
/// (auto-approve tools, but still answer `ask_user` and floor still prompts).
async fn handle_afk_toggle(
    app: &mut App,
    picker_tx: &mpsc::Sender<crate::console::picker::PickerRequest>,
    notice_tx: &mpsc::Sender<String>,
) {
    use crate::console::picker::{
        PickerAnswer, PickerOption, PickerQuestion, PickerRequest, PickerResponse,
    };
    use crate::permissions::Mode;
    use tokio::sync::oneshot;

    const FULLY_UNATTENDED: &str = "Fully unattended (dismiss questions)";
    const HANDS_FREE: &str = "Hands-free, keep questions";

    let Some(perms) = app.permissions.clone() else {
        app.add_assistant_notice("AFK: permission state not attached".to_string());
        return;
    };
    // Asymmetric gate: any AFK level → off fires immediately.
    if perms.mode() != Mode::Off {
        perms.set_mode(Mode::Off);
        let _ = crate::state::persist_permission_mode(None);
        app.add_assistant_notice("AFK disabled.".to_string());
        return;
    }
    if app.inline_picker.is_some() {
        // A picker is already up — don't race over it.
        app.add_assistant_notice("/afk: another picker is open; close it first.".to_string());
        return;
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let request = PickerRequest {
        questions: vec![PickerQuestion {
            question: "Enable AFK — how should the agent run while you're away?".to_string(),
            kind: "afk".to_string(),
            header: "AFK".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: false,
            mask: false,
            options: vec![
                PickerOption {
                    label: FULLY_UNATTENDED.to_string(),
                    description: "Auto-approve every tool call. `ask_user` is auto-dismissed so \
                                  the model proceeds on its best judgment. `rm -rf /` and \
                                  protected-path edits hard-deny — there's no one here to \
                                  confirm them. For CI, overnight, or one-shot runs."
                        .to_string(),
                    preview: None,
                },
                PickerOption {
                    label: HANDS_FREE.to_string(),
                    description: "Auto-approve tool calls so the picker stops interrupting you, \
                                  but the model can still consult you via `ask_user`, and \
                                  dangerous patterns (`rm -rf /`, edits to `.git`/`.ignis`/\
                                  shell init) still prompt for confirmation. For when you're \
                                  at the keyboard and want flow."
                        .to_string(),
                    preview: None,
                },
            ],
        }],
        reply: reply_tx,
    };
    if picker_tx.send(request).await.is_err() {
        app.add_assistant_notice("/afk: picker channel closed".to_string());
        return;
    }
    // Spawn so the key handler returns immediately; set the chosen mode on reply.
    let perms_for_reply = perms.clone();
    let notice_tx_clone = notice_tx.clone();
    tokio::spawn(async move {
        if let Ok(PickerResponse::Answered(answers)) = reply_rx.await {
            if let Some(PickerAnswer::Single(label)) = answers.first() {
                let chosen = match label.as_str() {
                    FULLY_UNATTENDED => Some(Mode::FullyUnattended),
                    HANDS_FREE => Some(Mode::HandsFree),
                    _ => None,
                };
                if let Some(mode) = chosen {
                    perms_for_reply.set_mode(mode);
                    let _ = crate::state::persist_permission_mode(Some(mode.as_str()));
                    let notice = match mode {
                        Mode::FullyUnattended => "AFK → Fully unattended.",
                        Mode::HandsFree => "AFK → Hands-free.",
                        Mode::Off => "AFK disabled.",
                    };
                    let _ = notice_tx_clone.send(notice.to_string()).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::format::AgentRequest;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn test_app() -> App {
        App::new(
            "test-provider".to_string(),
            "test-model".to_string(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        )
    }

    #[allow(clippy::type_complexity)]
    fn channels() -> (
        mpsc::Sender<AgentRequest>,
        mpsc::Receiver<AgentRequest>,
        mpsc::Sender<crate::console::picker::PickerRequest>,
        mpsc::Receiver<crate::console::picker::PickerRequest>,
        mpsc::Sender<String>,
        mpsc::Receiver<String>,
    ) {
        let (p_tx, p_rx) = mpsc::channel(8);
        let (pk_tx, pk_rx) = mpsc::channel(4);
        let (n_tx, n_rx) = mpsc::channel(8);
        (p_tx, p_rx, pk_tx, pk_rx, n_tx, n_rx)
    }

    #[tokio::test]
    async fn queued_compact_routes_to_compact_request_not_prompt() {
        // Bug fix: a `/compact` typed while busy used to be enqueued and then
        // sent to the LLM as a plain user message on drain. `submit_text` is
        // now the shared dispatcher for Enter and drain, so it must route
        // `/compact` to `AgentRequest::Compact`.
        let mut app = test_app();
        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        submit_text(
            &mut app,
            "/compact".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        match p_rx.try_recv().expect("expected a Compact request") {
            AgentRequest::Compact { .. } => {}
            other => panic!("expected Compact, got {:?}", std::mem::discriminant(&other)),
        }
        assert!(app.turn_in_flight, "/compact marks the turn in flight");
    }

    #[tokio::test]
    async fn queued_model_opens_picker_without_sending_prompt() {
        // `/model` is a local-only command — drain must open the picker
        // (or emit "No models configured." when empty), never send the
        // literal string to the LLM.
        let mut app = test_app();
        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        submit_text(
            &mut app,
            "/model".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(
            p_rx.try_recv().is_err(),
            "/model must not push anything onto the agent request channel"
        );
        assert!(!app.turn_in_flight, "/model does not start a turn");
    }

    #[tokio::test]
    async fn plain_text_routes_to_prompt_request() {
        let mut app = test_app();
        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        submit_text(
            &mut app,
            "hello world".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        match p_rx.try_recv().expect("expected a Prompt request") {
            AgentRequest::Prompt { prompt, .. } => assert_eq!(prompt, "hello world"),
            _ => panic!("expected Prompt"),
        }
        assert!(app.turn_in_flight);
    }

    fn last_notice(app: &App) -> Option<String> {
        app.blocks.iter().rev().find_map(|b| match b {
            crate::console::app::UIBlock::Assistant(t) => Some(t.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn no_provider_gate_blocks_prompts_and_llm_commands() {
        // The no-provider gate is hoisted to the top of submit_text. With no
        // provider, plain prompts and LLM commands (e.g. /model) must be
        // blocked with the connect hint and never reach the agent or open a
        // picker.
        let mut app = test_app();
        app.provider = String::new();
        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();

        submit_text(
            &mut app,
            "hi there".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(
            p_rx.try_recv().is_err(),
            "plain prompt must not reach the agent with no provider"
        );
        assert!(!app.turn_in_flight);
        assert_eq!(last_notice(&app).as_deref(), Some(NO_PROVIDER_HINT));

        submit_text(
            &mut app,
            "/model".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(
            app.model_picker.is_none(),
            "/model must be gated, not opened, with no provider"
        );
        assert_eq!(last_notice(&app).as_deref(), Some(NO_PROVIDER_HINT));
    }

    #[tokio::test]
    async fn unknown_slash_command_is_reported_not_sent() {
        // A `/`-prefixed token that matches no command and no skill falls to
        // the unknown-command arm — a notice, never a prompt to the agent.
        let mut app = test_app();
        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        submit_text(
            &mut app,
            "/nope".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(
            p_rx.try_recv().is_err(),
            "unknown command must not be sent to the agent"
        );
        assert!(!app.turn_in_flight);
        assert_eq!(
            last_notice(&app).as_deref(),
            Some("Unknown command `/nope`.")
        );
    }
    #[tokio::test]
    async fn connect_bypasses_no_provider_gate_even_when_a_connect_skill_exists() {
        // Regression: the hoisted gate must not block /connect when the user
        // has a skill literally named `connect` (skill names don't reserve it).
        // /connect is the only way out of no-provider mode — gating it would
        // lock the user out of ever configuring a provider from the TUI.
        use std::collections::HashSet;
        use std::sync::Arc;
        let tmp = crate::util::unique_temp_dir("ignis-keys-connect-skill");
        let dir = tmp.join(".ignis/skills/connect");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: connect\n---\nbody").unwrap();
        let reg = crate::skills::SkillRegistry::load(None, &tmp, HashSet::new());
        assert!(
            reg.all().iter().any(|s| s.name == "connect"),
            "fixture must register a `connect` skill"
        );

        let mut app = test_app();
        app.provider = String::new();
        app.skills = Some(Arc::new(reg));
        let (p_tx, _p_rx, pk_tx, mut pk_rx, n_tx, _n_rx) = channels();

        submit_text(
            &mut app,
            "/connect".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;

        assert!(
            pk_rx.try_recv().is_ok(),
            "/connect must start the connect flow, not be blocked by the gate"
        );
        assert_ne!(last_notice(&app).as_deref(), Some(NO_PROVIDER_HINT));

        std::fs::remove_dir_all(&tmp).ok();
    }
}
