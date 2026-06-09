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

/// Editor-style cursor / paste ops the input box always honors (both at idle
/// and, via the busy-mode fallthrough, while a turn runs). Mutations go through
/// the typed `app.composer` surface so the cursor/input UTF-8 invariant can't be
/// violated from here; `app` only owns the cross-cutting exit-hint + slash reset.
/// Returns true if the key was consumed.
fn apply_edit_key(app: &mut App, key: KeyEvent) -> bool {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            app.clear_exit_hint();
            app.composer.clear();
            app.reset_slash_selection();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            app.clear_exit_hint();
            app.composer.cursor_home();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            app.clear_exit_hint();
            app.composer.cursor_end();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) if app.composer.cursor > 0 => {
            app.clear_exit_hint();
            app.composer.delete_word_back();
        }
        (m, KeyCode::Char('j'))
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::SUPER) =>
        {
            app.clear_exit_hint();
            app.composer.insert_char('\n');
            app.reset_slash_selection();
        }
        (_, KeyCode::Char(c)) => {
            app.clear_exit_hint();
            app.composer.insert_char(c);
            app.reset_slash_selection();
        }
        (_, KeyCode::Backspace) if app.composer.cursor > 0 => {
            app.clear_exit_hint();
            app.composer.backspace();
            app.reset_slash_selection();
        }
        (_, KeyCode::Delete) if app.composer.cursor < app.composer.input.len() => {
            app.clear_exit_hint();
            app.composer.delete_forward();
            app.reset_slash_selection();
        }
        (_, KeyCode::Left) if app.composer.cursor > 0 => {
            app.clear_exit_hint();
            app.composer.move_left();
        }
        (_, KeyCode::Right) if app.composer.cursor < app.composer.input.len() => {
            app.clear_exit_hint();
            app.composer.move_right();
        }
        (_, KeyCode::Home) => {
            app.clear_exit_hint();
            app.composer.cursor_home();
        }
        (_, KeyCode::End) => {
            app.clear_exit_hint();
            app.composer.cursor_end();
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

/// Dispatch a multi-select **toggle** picker (`/skills`, `/mcp`): ↑/↓ move the
/// highlight, Enter/Space toggle the highlighted row's `[x]/[ ]` checkbox (the
/// checkbox is the feedback — no transcript notice per toggle), Esc closes it,
/// and any other char closes it *and* falls through so the char still types.
/// `$select`/`$toggle` are this picker's `App` methods; `$field` is its
/// `Option<…Picker>`. The model and session pickers don't use this — they carry
/// bespoke behavior (effort cycling, List/Detail drill-in).
macro_rules! toggle_picker {
    ($app:ident, $key:ident, $field:ident, $select:ident, $toggle:ident) => {
        if $app.$field.is_some() {
            match $key.code {
                KeyCode::Up => {
                    $app.$select(SelectionDirection::Previous);
                    return;
                }
                KeyCode::Down => {
                    $app.$select(SelectionDirection::Next);
                    return;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    $app.$toggle();
                    return;
                }
                KeyCode::Esc => {
                    $app.$field = None;
                    return;
                }
                KeyCode::Char(_) => {
                    $app.$field = None;
                }
                _ => {}
            }
        }
    };
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
        use crate::console::connect::ConnectAdvance;
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
                if app.connect.is_active() {
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
            if app.settings_panel.is_some() {
                app.close_settings();
                return;
            }
            app.session_picker = None;
            app.model_picker = None;
            app.skill_picker = None;
            return;
        }
        (m, KeyCode::Char('d')) if m.contains(KeyModifiers::CONTROL) => {
            app.request_exit();
            return;
        }
        // Ctrl+O: collapse/expand reasoning. Global so it works mid-thought
        // (re-renders the live preview) and at idle (re-renders past thoughts).
        // No-op while a picker owns the screen — toggling re-commits the whole
        // transcript (a /resume-style wipe) which would scribble behind the
        // open picker. Still consumed so it never types a literal 'o' into one.
        (m, KeyCode::Char('o')) if m.contains(KeyModifiers::CONTROL) => {
            if !crate::console::render::layout::picker_open(app) {
                app.clear_exit_hint();
                app.toggle_reasoning_expanded();
            }
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
            let text = app
                .composer
                .expand_pastes(app.composer.input.trim().to_string());
            if !text.is_empty() {
                let sender = active_inject.lock().unwrap().clone();
                match sender {
                    Some(tx) => match tx.try_send(text.clone()) {
                        Ok(()) => {
                            app.pending_injects.push(text);
                            app.composer.clear();
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
                        app.composer.clear();
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
                    app.composer.set_text(cmd);
                }
                let text = app
                    .composer
                    .expand_pastes(app.composer.input.trim().to_string());
                if !text.is_empty() {
                    app.enqueue(text);
                    app.composer.clear();
                    app.reset_slash_selection();
                }
            }
            (_, KeyCode::Up) if !app.slash_suggestions().is_empty() => {
                app.select_slash_suggestion(SelectionDirection::Previous);
            }
            (_, KeyCode::Down) if !app.slash_suggestions().is_empty() => {
                app.select_slash_suggestion(SelectionDirection::Next);
            }
            (_, KeyCode::Up) if app.composer.input.is_empty() && !app.queue.is_empty() => {
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

    if let Some(tab) = app.settings_panel.as_ref().map(|p| p.tab) {
        use crate::console::app::SettingsTab;
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                app.settings_switch_tab(SelectionDirection::Next);
                return;
            }
            KeyCode::BackTab | KeyCode::Left => {
                app.settings_switch_tab(SelectionDirection::Previous);
                return;
            }
            KeyCode::Up => {
                app.settings_move(SelectionDirection::Previous);
                return;
            }
            KeyCode::Down => {
                app.settings_move(SelectionDirection::Next);
                return;
            }
            // Space toggles a footer segment on the Statusline tab.
            KeyCode::Char(' ') if tab == SettingsTab::Statusline => {
                app.settings_toggle_statusline();
                return;
            }
            KeyCode::Enter => {
                match tab {
                    // Statusline: Enter toggles like Space (stays open).
                    SettingsTab::Statusline => app.settings_toggle_statusline(),
                    // Stats is read-only — Enter just closes.
                    SettingsTab::Stats => app.close_settings(),
                }
                return;
            }
            // Typing dismisses the panel, then falls through so the char lands
            // in the composer (matches the other slash pickers' behavior).
            KeyCode::Char(_) => {
                app.close_settings();
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

    // `r`/`R` hot-reloads the skill registry from disk while the picker stays
    // open, so newly added / edited / removed SKILL.md files appear without a
    // restart. Must run before `toggle_picker!`, which closes the picker on any
    // char. Hand the runner the *same* rebuilt registry so both keep sharing one
    // `Arc` — later toggles stay live, as they were before the reload.
    if app.skill_picker.is_some() && matches!(key.code, KeyCode::Char('r' | 'R')) {
        let count = app.reload_skills(dirs::home_dir().as_deref());
        if let Some(registry) = app.skills.clone() {
            let _ = prompt_tx.send(AgentRequest::ReloadSkills(registry)).await;
        }
        if count == 0 {
            // A 0-row picker with a live selection index is a bug; close it and
            // leave a breadcrumb in scrollback instead.
            app.skill_picker = None;
            app.add_assistant_notice("No skills found after reload.".to_string());
        }
        return;
    }
    toggle_picker!(
        app,
        key,
        skill_picker,
        select_skill_picker,
        toggle_selected_skill
    );
    toggle_picker!(
        app,
        key,
        mcp_picker,
        select_mcp_picker,
        toggle_selected_mcp_server
    );

    // Idle-specific arms first (submit + history/slash navigation); every other
    // key falls through to `apply_edit_key`, the single home of the editor arms
    // (Ctrl+U/A/E/W, Char, Backspace, Delete, arrows, Home/End) — so the editor
    // behavior is defined once and shared with the busy-mode fallthrough.
    match (key.modifiers, key.code) {
        (_, KeyCode::Enter) => {
            if let Some(cmd) = app.selected_slash_command() {
                app.composer.set_text(cmd);
            }
            let text = app.submit().unwrap_or_default();
            if !text.is_empty() {
                submit_text(app, text, prompt_tx, picker_tx, notice_tx, storage_dir).await;
            }
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
        // Editor keys (and PgUp/PgDn etc., which `apply_edit_key` ignores so the
        // terminal's own native scrollback handles them).
        _ => {
            apply_edit_key(app, key);
        }
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

    // Each newly-dispatched line starts with no display override. Only the
    // `/skill-name` branch sets one; clearing here drops any override stranded
    // by a prior turn whose `UserPromptCommitted` never fired (a blocked
    // `UserPromptSubmit` hook), so it can't mislabel this line.
    app.pending_user_display = None;

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
            // The picker is for jumping to a *different* session; resuming the
            // one you're already in is a no-op, so drop the current session.
            records.retain(|r| r.session_id != app.session_id);
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
        "/settings" if arg_count == 1 => app.show_settings_panel(),
        cmd if is_skill_command(app, cmd) => {
            let name = cmd.trim_start_matches('/').to_string();
            let reg = app.skills.clone().unwrap();
            if let Some(skill) = reg.get_enabled(&name) {
                let args = text[cmd.len()..].trim();
                let mut prompt = build_skill_prompt(&skill.name, &skill.body, args);
                if let Some(note) = skill.resources_note() {
                    prompt.push_str(&note);
                }
                // The model gets the full skill body (`prompt`); the transcript
                // shows the command the user typed. Without this override the
                // committed prompt — the whole body — would render as the user
                // turn (CC/Codex show the compact invocation, not the expansion).
                app.pending_user_display = Some(text.trim().to_string());
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

/// `/hooks` — list or reload the in-memory hook registry.
///
///   `/hooks`        list every registered hook (the chain for each
///                    event, the program + argv, and the per-hook timeout)
///   `/hooks list`   alias of bare `/hooks`
///   `/hooks reload` re-read `~/.ignis/hooks.json` from disk
///
/// Listing reflects the registry as it stands in memory (i.e. what the
/// session is actually running) — it is not a live probe of the JSON
/// file. Reload first if you just edited it.
async fn handle_hooks_command(app: &mut App, text: &str) {
    let mut parts = text.split_whitespace();
    let _ = parts.next(); // "/hooks"
    let sub = parts.next().unwrap_or("");

    match sub {
        "" | "list" => list_hooks(app).await,
        "reload" => reload_hooks(app).await,
        _ => app.add_assistant_notice(
            "Usage: /hooks [list] | /hooks reload — list the in-memory hook \
             chains or re-read ~/.ignis/hooks.json from disk."
                .to_string(),
        ),
    }
}

/// Render the current hook chains into a single multi-line notice.
async fn list_hooks(app: &mut App) {
    let Some(reg) = app.hooks.clone() else {
        app.add_assistant_notice("[err] hooks registry unavailable in this session.".to_string());
        return;
    };
    app.add_assistant_notice(reg.format_list().await);
}

/// Reload `~/.ignis/hooks.json` into the shared registry. Posts an
/// `[info]` or `[err]` line to scrollback. The security reminder rides
/// along on success so the user keeps noticing it every time they
/// touch the file.
async fn reload_hooks(app: &mut App) {
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

    fn app_with_skill(name: &str, body: &str) -> App {
        let tmp = crate::util::unique_temp_dir("ignis-keys-skill");
        let cwd = tmp.join("proj");
        let dir = cwd.join(".ignis/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\n---\n{body}"),
        )
        .unwrap();
        let reg = crate::skills::SkillRegistry::load(None, &cwd, std::collections::HashSet::new());
        std::fs::remove_dir_all(&tmp).ok();
        let mut app = test_app();
        app.skills = Some(std::sync::Arc::new(reg));
        app
    }

    #[tokio::test]
    async fn skill_command_sends_full_body_but_displays_typed_command() {
        // The model must receive the expanded skill body; the transcript must
        // show only the compact `/skill-name args` the user typed.
        let mut app = app_with_skill("react", "React body instructions.");
        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        submit_text(
            &mut app,
            "/react fix it".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        match p_rx.try_recv().expect("expected a Prompt request") {
            AgentRequest::Prompt { prompt, .. } => assert!(
                prompt.contains("React body instructions."),
                "model receives the full expanded skill body"
            ),
            _ => panic!("expected Prompt"),
        }
        assert_eq!(
            app.pending_user_display.as_deref(),
            Some("/react fix it"),
            "transcript will render the typed command, not the body"
        );
    }

    #[tokio::test]
    async fn stale_display_override_is_cleared_on_next_dispatch() {
        // If a prior /skill turn's UserPromptCommitted never fired (a blocked
        // UserPromptSubmit hook), its override must not survive to mislabel the
        // next prompt. Dispatch clears it up front.
        let mut app = test_app();
        app.pending_user_display = Some("/react stale".to_string());
        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        submit_text(
            &mut app,
            "hello".to_string(),
            &p_tx,
            &pk_tx,
            &n_tx,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(
            app.pending_user_display.is_none(),
            "a stale override is cleared at the top of dispatch"
        );
        match p_rx.try_recv().expect("expected a Prompt request") {
            AgentRequest::Prompt { prompt, .. } => assert_eq!(prompt, "hello"),
            _ => panic!("expected Prompt"),
        }
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

    #[tokio::test]
    async fn r_in_skill_picker_hands_runner_the_same_registry_arc() {
        // Regression (codex review on #157): reload must not let the UI and the
        // runner diverge onto separate registries. Pressing `r` rebuilds
        // `App.skills` and sends the runner the SAME `Arc`, so a later toggle
        // (interior mutability) stays visible to the next prompt — the shared-
        // registry behavior that held before any reload.
        use std::collections::HashSet;
        use std::sync::Arc;
        let tmp = crate::util::unique_temp_dir("ignis-keys-skill-reload");
        let dir = tmp.join(".ignis/skills/demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: demo\n---\nbody").unwrap();

        let mut app = test_app();
        app.cwd = tmp.clone();
        app.skills = Some(Arc::new(crate::skills::SkillRegistry::load(
            None,
            &tmp,
            HashSet::new(),
        )));
        app.show_skill_picker();

        let (p_tx, mut p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        let (c_tx, _c_rx) = mpsc::channel::<()>(1);
        let inject: ActiveInject = std::sync::Arc::new(std::sync::Mutex::new(None));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &p_tx,
            &c_tx,
            &inject,
            std::path::Path::new("/tmp"),
            &pk_tx,
            &n_tx,
        )
        .await;

        let ui_reg = app
            .skills
            .clone()
            .expect("UI keeps a registry after reload");
        match p_rx
            .try_recv()
            .expect("pressing r must send ReloadSkills to the runner")
        {
            AgentRequest::ReloadSkills(runner_reg) => assert!(
                Arc::ptr_eq(&runner_reg, &ui_reg),
                "runner must receive the SAME registry Arc the UI holds"
            ),
            other => panic!(
                "expected ReloadSkills, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert!(app.skill_picker.is_some(), "picker stays open after reload");

        std::fs::remove_dir_all(&tmp).ok();
    }

    async fn press_ctrl_o(app: &mut App) {
        let (p_tx, _p_rx, pk_tx, _pk_rx, n_tx, _n_rx) = channels();
        let (c_tx, _c_rx) = mpsc::channel::<()>(1);
        let inject: ActiveInject = std::sync::Arc::new(std::sync::Mutex::new(None));
        handle_key(
            app,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &p_tx,
            &c_tx,
            &inject,
            std::path::Path::new("/tmp"),
            &pk_tx,
            &n_tx,
        )
        .await;
    }

    #[tokio::test]
    async fn ctrl_o_toggles_reasoning_at_idle() {
        let mut app = test_app();
        assert!(!app.reasoning_expanded);
        press_ctrl_o(&mut app).await;
        assert!(app.reasoning_expanded, "ctrl+o expands");
        press_ctrl_o(&mut app).await;
        assert!(!app.reasoning_expanded, "ctrl+o collapses again");
    }

    #[tokio::test]
    async fn ctrl_o_is_noop_while_a_picker_is_open() {
        // A slash picker owns the screen; toggling would re-commit the
        // transcript behind it, so Ctrl+O must do nothing (but stay consumed).
        let mut app = test_app();
        app.show_settings_panel();
        press_ctrl_o(&mut app).await;
        assert!(
            !app.reasoning_expanded,
            "ctrl+o must not toggle behind an open picker"
        );
    }
}
