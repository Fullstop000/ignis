use std::path::PathBuf;
use std::time::Instant;

use super::{
    format_duration, next_selection, slash_suggestions, SelectionDirection, SlashCommand, SPINNERS,
    THINKING_VERBS,
};
use crate::AgentEvent;

/// Clipboard function type: takes text, returns Ok/Err.
type ClipFn = for<'a> fn(&'a str) -> Result<(), String>;

/// Decode a persisted tool message's content `{"result": <str>, "is_error": <bool>}`
/// back into the (display text, is_error) the live UI shows. Falls back to the
/// raw string if it isn't the expected JSON shape.
pub(crate) fn parse_tool_result(content: &str) -> (String, bool) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(result) = v.get("result").and_then(|r| r.as_str()) {
            let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            return (result.to_string(), is_error);
        }
    }
    (content.to_string(), false)
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) status: ToolStatus,
    pub(crate) started_at: Instant,
    pub(crate) elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub(crate) enum ToolStatus {
    Pending,
    Success(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub(crate) enum UIBlock {
    User(String),
    Assistant(String),
    Tool(ToolCallEntry),
}

#[derive(Debug, Clone)]
pub(crate) struct SessionPicker {
    pub(crate) sessions: Vec<crate::session::SessionMeta>,
    pub(crate) selected: usize,
}

/// `/skills` picker state. Rows come from `App.skills` registry `all()`.
#[derive(Debug, Clone)]
pub(crate) struct SkillPicker {
    pub(crate) selected: usize,
}

/// `/model` picker state. Options live on `App.model_options`; this tracks the
/// highlighted row and, for a reasoning-capable model, the chosen effort level.
#[derive(Debug, Clone)]
pub(crate) struct ModelPicker {
    pub(crate) selected: usize,
    /// Index into the selected option's `effort_levels` (ignored if empty).
    pub(crate) effort_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Idle,
    Thinking,    // LLM is generating
    ToolRunning, // tool execution in progress
}

pub(crate) struct App {
    pub(crate) provider: String,
    pub(crate) model: String,
    /// Active reasoning effort level (shown in the footer); `None` = not set.
    pub(crate) effort: Option<String>,
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,

    pub(crate) blocks: Vec<UIBlock>,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) history: Vec<String>,
    pub(crate) history_idx: Option<usize>,
    pub(crate) saved_input: String, // saved input when browsing history
    pub(crate) slash_selection: usize,
    pub(crate) session_picker: Option<SessionPicker>,
    /// Choices for the `/model` picker, flattened from config.
    pub(crate) model_options: Vec<crate::models::ModelOption>,
    pub(crate) model_picker: Option<ModelPicker>,
    pub(crate) skill_picker: Option<SkillPicker>,

    pub(crate) mode: Mode,
    pub(crate) tick: u64,
    pub(crate) stream_start: Option<Instant>,
    pub(crate) current_chunk_idx: Option<usize>,
    /// Output chars streamed in the current turn (for live token estimate).
    pub(crate) stream_chars: usize,
    /// Real token usage from the most recent completed turn (provider-reported).
    pub(crate) last_usage: Option<crate::Usage>,

    /// Number of leading blocks already flushed to the terminal's scrollback
    /// (committed via `insert_before`). The rest live in the in-memory transcript
    /// and are flushed once finalized.
    pub(crate) committed: usize,

    pub(crate) should_quit: bool,
    pub(crate) error_flash: Option<(String, Instant)>,
    pub(crate) exit_pending: bool,

    /// Token budget the context-usage % is measured against (the active model's
    /// window, or the fallback below). Estimated, not exact.
    pub(crate) context_window: usize,
    /// Window to use when the active model's context is unknown (the compaction
    /// threshold) — so the gauge never sticks to a previous model's window.
    pub(crate) fallback_context_window: usize,

    /// Prompts typed while busy, waiting to fire after the current turn (FIFO).
    pub(crate) queue: Vec<String>,
    /// Inject messages sent to the live turn but not yet confirmed via
    /// `UserInjected`; reconciled on `AgentEnd` (leftovers → front of `queue`).
    pub(crate) pending_injects: Vec<String>,
    /// Set by `handle_event` on `AgentEnd`; the main loop drains one queued item
    /// per turn-end (edge-triggered, never level-triggered on `mode == Idle`).
    pub(crate) turn_just_ended: bool,
    /// True from the moment a prompt/compact is dispatched until its `AgentEnd`.
    /// Used to tell a real turn-end (drain the queue) from a stray/duplicate
    /// `AgentEnd` — `mode` can't, since an early failure ends a turn that never
    /// reached `AgentStart` (still `Idle`).
    pub(crate) turn_in_flight: bool,

    /// Clipboard function, injectable for testing.
    pub(crate) clipboard_fn: ClipFn,

    /// Skill registry for slash autocomplete; `None` when no skills are loaded.
    pub(crate) skills: Option<std::sync::Arc<crate::skills::SkillRegistry>>,
}

impl App {
    pub(crate) fn new(provider: String, model: String, session_id: String, cwd: PathBuf) -> Self {
        Self {
            provider,
            model,
            effort: None,
            session_id,
            cwd,
            blocks: Vec::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            slash_selection: 0,
            session_picker: None,
            model_options: Vec::new(),
            model_picker: None,
            skill_picker: None,
            mode: Mode::Idle,
            tick: 0,
            stream_start: None,
            current_chunk_idx: None,
            stream_chars: 0,
            last_usage: None,
            committed: 0,
            should_quit: false,
            error_flash: None,
            exit_pending: false,
            context_window: 120_000,
            fallback_context_window: 120_000,
            queue: Vec::new(),
            pending_injects: Vec::new(),
            turn_just_ended: false,
            turn_in_flight: false,
            clipboard_fn: super::clipboard::set_clipboard,
            skills: None,
        }
    }

    pub(crate) fn set_context_window(&mut self, window: usize) {
        self.context_window = window;
    }

    /// Estimated tokens used by the whole transcript (chars/4). Estimate, not
    /// the provider's exact count.
    pub(crate) fn context_tokens(&self) -> usize {
        let chars: usize = self
            .blocks
            .iter()
            .map(|b| match b {
                UIBlock::User(t) | UIBlock::Assistant(t) => t.len(),
                UIBlock::Tool(e) => {
                    e.arguments.len()
                        + match &e.status {
                            ToolStatus::Success(s) | ToolStatus::Error(s) => s.len(),
                            ToolStatus::Pending => 0,
                        }
                }
            })
            .sum();
        chars / 4
    }

    /// Context tokens and % for the footer. Prefers the provider's real
    /// last-turn input size; falls back to the chars/4 estimate.
    pub(crate) fn context_usage(&self) -> (u64, u8) {
        let tokens = match self.last_usage {
            Some(u) if u.input_tokens > 0 => u.input_tokens,
            _ => self.context_tokens() as u64,
        };
        let pct = (tokens as usize * 100)
            .checked_div(self.context_window)
            .map(|p| p.min(100))
            .unwrap_or(0) as u8;
        (tokens, pct)
    }

    /// Estimated output tokens streamed so far in the current turn (chars/4).
    pub(crate) fn stream_tokens(&self) -> usize {
        self.stream_chars / 4
    }

    /// Estimated output tokens/second for the current turn (0 until ~0.5s in).
    pub(crate) fn stream_rate(&self) -> usize {
        match self.stream_start {
            Some(t) => {
                let secs = t.elapsed().as_secs_f64();
                if secs >= 0.5 {
                    (self.stream_tokens() as f64 / secs).round() as usize
                } else {
                    0
                }
            }
            None => 0,
        }
    }

    /// Current "Thinking" verb, cycling every 3s while generating.
    pub(crate) fn thinking_label(&self) -> &'static str {
        let secs = self
            .stream_start
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        THINKING_VERBS[(secs as usize / 3) % THINKING_VERBS.len()]
    }

    pub(crate) fn spinner(&self) -> &str {
        SPINNERS[(self.tick as usize / 10) % SPINNERS.len()]
    }

    pub(crate) fn elapsed_str(&self) -> String {
        match self.stream_start {
            Some(t) => format_duration(t.elapsed().as_millis()),
            None => String::new(),
        }
    }

    pub(crate) fn handle_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AgentStart => {
                self.mode = Mode::Thinking;
                self.stream_start = Some(Instant::now());
                self.stream_chars = 0;
            }
            AgentEvent::TurnStart => {}
            AgentEvent::MessageStart { .. } => {
                self.blocks.push(UIBlock::Assistant(String::new()));
                self.current_chunk_idx = Some(self.blocks.len() - 1);
            }
            AgentEvent::MessageUpdate { delta } => {
                self.stream_chars += delta.len();
                if let Some(i) = self.current_chunk_idx {
                    if let Some(UIBlock::Assistant(ref mut s)) = self.blocks.get_mut(i) {
                        s.push_str(&delta);
                    }
                }
            }
            AgentEvent::MessageEnd { .. } => {
                self.current_chunk_idx = None;
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                self.mode = Mode::ToolRunning;
                self.blocks.push(UIBlock::Tool(ToolCallEntry {
                    id: tool_call_id,
                    name: tool_name,
                    arguments,
                    status: ToolStatus::Pending,
                    started_at: Instant::now(),
                    elapsed_ms: 0,
                }));
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
            } => {
                for b in self.blocks.iter_mut().rev() {
                    if let UIBlock::Tool(ref mut t) = b {
                        if t.id == tool_call_id {
                            t.elapsed_ms = t.started_at.elapsed().as_millis();
                            t.status = if result.is_error {
                                ToolStatus::Error(result.content)
                            } else {
                                ToolStatus::Success(result.content)
                            };
                            break;
                        }
                    }
                }
                self.mode = Mode::Thinking;
            }
            AgentEvent::TurnEnd => {}
            AgentEvent::Usage(usage) => {
                // Real provider-reported usage for the latest turn.
                self.last_usage = Some(usage);
            }
            AgentEvent::AgentEnd => {
                self.mode = Mode::Idle;
                self.current_chunk_idx = None;
                self.stream_start = None;
                // Only the AgentEnd of a turn we actually dispatched drains the
                // queue. This catches both a duplicate AgentEnd (e.g. a persist
                // error after the agent loop already emitted one) and a turn that
                // ended *before* AgentStart (provider-build / session-open error,
                // which still leaves `mode == Idle`).
                if self.turn_in_flight {
                    self.turn_in_flight = false;
                    self.turn_just_ended = true;
                    // Any inject that lost the end-of-turn race fires next turn.
                    if !self.pending_injects.is_empty() {
                        let mut stranded = std::mem::take(&mut self.pending_injects);
                        stranded.append(&mut self.queue);
                        self.queue = stranded;
                    }
                }
            }
            AgentEvent::UserInjected { text } => {
                // Events arrive on one ordered channel and the agent emits
                // `UserInjected` in the same FIFO order injects were sent, so the
                // front of `pending_injects` is this confirmation. (Empty when an
                // inject was drained at run-start, not via Ctrl+S — still shown.)
                if !self.pending_injects.is_empty() {
                    self.pending_injects.remove(0);
                }
                self.push_user_prompt(text);
            }
        }
    }

    /// Whether block `i` is finalized and safe to flush to scrollback:
    /// user prompts always are; assistant blocks once they stop streaming; tool
    /// calls once they leave the pending state.
    pub(crate) fn block_done(&self, i: usize) -> bool {
        match self.blocks.get(i) {
            Some(UIBlock::User(_)) => true,
            Some(UIBlock::Assistant(_)) => self.current_chunk_idx != Some(i),
            Some(UIBlock::Tool(t)) => !matches!(t.status, ToolStatus::Pending),
            None => false,
        }
    }

    /// Byte offset of the char boundary one character left of the cursor.
    /// `cursor` indexes `input` by byte, so movement must step whole UTF-8
    /// chars — a naive `cursor -= 1` lands mid-character and panics on slice.
    fn prev_char_boundary(&self) -> usize {
        self.input[..self.cursor]
            .chars()
            .next_back()
            .map_or(0, |c| self.cursor - c.len_utf8())
    }

    /// Byte offset of the char boundary one character right of the cursor.
    fn next_char_boundary(&self) -> usize {
        self.input[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    pub(crate) fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_char_boundary();
        self.input.remove(prev);
        self.cursor = prev;
    }

    pub(crate) fn delete_forward(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    pub(crate) fn move_left(&mut self) {
        self.cursor = self.prev_char_boundary();
    }

    pub(crate) fn move_right(&mut self) {
        self.cursor = self.next_char_boundary();
    }

    /// Push a user prompt into the transcript + input history (shared by submit
    /// and the queue drain). Does not send anything.
    pub(crate) fn push_user_prompt(&mut self, text: String) {
        self.exit_pending = false;
        self.history.push(text.clone());
        self.history_idx = None;
        self.blocks.push(UIBlock::User(text));
    }

    /// Queue a prompt typed while busy (no transcript block until it fires).
    pub(crate) fn enqueue(&mut self, text: String) {
        self.exit_pending = false;
        self.queue.push(text);
    }

    /// Pop the next queued prompt (FIFO) for the drain.
    pub(crate) fn take_queued_front(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    /// Move the most recent queued prompt back into the input for editing.
    pub(crate) fn recall_last_queued(&mut self) -> bool {
        match self.queue.pop() {
            Some(text) => {
                self.input = text;
                self.cursor = self.input.len();
                true
            }
            None => false,
        }
    }

    /// Read and clear the edge-trigger flag set on `AgentEnd`.
    pub(crate) fn take_turn_just_ended(&mut self) -> bool {
        std::mem::take(&mut self.turn_just_ended)
    }

    pub(crate) fn submit(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.mode != Mode::Idle {
            return None;
        }
        self.push_user_prompt(text.clone());
        self.input.clear();
        self.cursor = 0;
        Some(text)
    }

    pub(crate) fn add_assistant_notice(&mut self, text: String) {
        self.exit_pending = false;
        self.session_picker = None;
        self.blocks.push(UIBlock::Assistant(text));
    }

    pub(crate) fn start_new_session(&mut self, session_id: String) {
        self.exit_pending = false;
        self.session_id = session_id;
        self.blocks.clear();
        self.committed = 0;
        self.current_chunk_idx = None;
        self.history_idx = None;
        self.last_usage = None;
        self.session_picker = None;
        self.add_assistant_notice(format!("Started new session `{}`.", self.session_id));
    }

    pub(crate) fn show_session_picker(&mut self, sessions: Vec<crate::session::SessionMeta>) {
        self.exit_pending = false;
        self.session_picker = Some(SessionPicker {
            sessions,
            selected: 0,
        });
    }

    pub(crate) fn select_session_picker(&mut self, direction: SelectionDirection) -> bool {
        self.exit_pending = false;
        let Some(picker) = &mut self.session_picker else {
            return false;
        };
        if picker.sessions.is_empty() {
            return false;
        }
        picker.selected = next_selection(picker.selected, picker.sessions.len(), direction);
        true
    }

    pub(crate) fn selected_session_id(&self) -> Option<String> {
        self.session_picker
            .as_ref()
            .and_then(|picker| picker.sessions.get(picker.selected))
            .map(|session| session.id.clone())
    }

    /// Supply the `/model` picker choices and the active effort level.
    pub(crate) fn set_model_options(
        &mut self,
        options: Vec<crate::models::ModelOption>,
        effort: Option<String>,
    ) {
        self.model_options = options;
        self.effort = effort;
    }

    pub(crate) fn show_model_picker(&mut self) {
        self.exit_pending = false;
        if self.model_options.is_empty() {
            self.add_assistant_notice("No models configured.".to_string());
            return;
        }
        let selected = self
            .model_options
            .iter()
            .position(|o| o.provider == self.provider && o.model == self.model)
            .unwrap_or(0);
        let effort_idx = self
            .effort
            .as_deref()
            .and_then(|e| {
                self.model_options[selected]
                    .effort_levels
                    .iter()
                    .position(|l| l == e)
            })
            .unwrap_or(0);
        self.model_picker = Some(ModelPicker {
            selected,
            effort_idx,
        });
    }

    pub(crate) fn select_model_picker(&mut self, direction: SelectionDirection) {
        self.exit_pending = false;
        let len = self.model_options.len();
        if len == 0 {
            return;
        }
        let Some(picker) = &mut self.model_picker else {
            return;
        };
        picker.selected = next_selection(picker.selected, len, direction);
        let sel = picker.selected;
        // `picker` borrow ends above; clamp effort to the new model's levels.
        let levels = self.model_options[sel].effort_levels.len();
        if let Some(picker) = &mut self.model_picker {
            if picker.effort_idx >= levels {
                picker.effort_idx = 0;
            }
        }
    }

    pub(crate) fn cycle_effort(&mut self, direction: SelectionDirection) {
        self.exit_pending = false;
        let Some(picker) = &self.model_picker else {
            return;
        };
        let (sel, cur) = (picker.selected, picker.effort_idx);
        let levels = self.model_options[sel].effort_levels.len();
        if levels == 0 {
            return;
        }
        let idx = next_selection(cur, levels, direction);
        if let Some(picker) = &mut self.model_picker {
            picker.effort_idx = idx;
        }
    }

    /// Apply the highlighted selection: update the displayed provider/model/effort,
    /// close the picker, and return `(provider, model, effort)` to act on.
    pub(crate) fn apply_model_selection(&mut self) -> Option<(String, String, Option<String>)> {
        let picker = self.model_picker.take()?;
        let opt = self.model_options.get(picker.selected)?.clone();
        let effort = if opt.effort_levels.is_empty() {
            None
        } else {
            opt.effort_levels.get(picker.effort_idx).cloned()
        };
        self.provider = opt.provider.clone();
        self.model = opt.model.clone();
        self.effort = effort.clone();
        // Retarget the footer's context gauge to the new model's window, falling
        // back when it's unknown so the % isn't measured against the old model.
        self.context_window = opt
            .context
            .map(|c| c as usize)
            .unwrap_or(self.fallback_context_window);
        Some((opt.provider, opt.model, effort))
    }

    pub(crate) fn show_skill_picker(&mut self) {
        self.exit_pending = false;
        match self.skills.as_deref() {
            Some(reg) if !reg.is_empty() => {
                self.skill_picker = Some(SkillPicker { selected: 0 });
            }
            _ => self.add_assistant_notice(
                "No skills found. Add one at ~/.ignis/skills/<name>/SKILL.md".to_string(),
            ),
        }
    }

    pub(crate) fn select_skill_picker(&mut self, direction: SelectionDirection) {
        let len = self.skills.as_deref().map(|r| r.all().len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        if let Some(picker) = &mut self.skill_picker {
            picker.selected = next_selection(picker.selected, len, direction);
        }
    }

    /// Toggle the highlighted skill. Returns `(name, now_enabled)` for a notice.
    pub(crate) fn toggle_selected_skill(&mut self) -> Option<(String, bool)> {
        let picker = self.skill_picker.as_ref()?;
        let reg = self.skills.as_deref()?;
        let name = reg.all().get(picker.selected)?.name.clone();
        let now_enabled = reg.toggle(&name);
        Some((name, now_enabled))
    }

    pub(crate) fn render_session_history(
        &mut self,
        session_id: String,
        messages: Vec<crate::Message>,
    ) {
        self.exit_pending = false;
        self.session_id = session_id.clone();
        self.blocks.clear();
        self.committed = 0;
        self.current_chunk_idx = None;
        self.session_picker = None;
        self.last_usage = None;

        if messages.is_empty() {
            self.add_assistant_notice(format!("Resumed empty session `{}`.", session_id));
            return;
        }

        for message in messages {
            match message.role.as_str() {
                "user" => {
                    if let Some(content) = message.content.filter(|c| !c.is_empty()) {
                        self.blocks.push(UIBlock::User(content));
                    }
                }
                "assistant" => {
                    if let Some(content) = message.content.filter(|c| !c.is_empty()) {
                        self.blocks.push(UIBlock::Assistant(content));
                    } else if let Some(reasoning) =
                        message.reasoning_content.filter(|r| !r.is_empty())
                    {
                        self.blocks
                            .push(UIBlock::Assistant(format!("💭 {reasoning}")));
                    }
                    // Reconstruct tool blocks from this turn's calls; the
                    // following `tool` messages fill in each result by id.
                    if let Some(tool_calls) = message.tool_calls {
                        for tc in tool_calls {
                            self.blocks.push(UIBlock::Tool(ToolCallEntry {
                                id: tc.id,
                                name: tc.function.name,
                                arguments: tc.function.arguments,
                                status: ToolStatus::Success(String::new()),
                                started_at: Instant::now(),
                                elapsed_ms: 0,
                            }));
                        }
                    }
                }
                "tool" => {
                    // Persisted tool content is {"result": <str>, "is_error": <bool>};
                    // parse it and attach to the matching pending tool block.
                    let (result, is_error) =
                        parse_tool_result(message.content.as_deref().unwrap_or(""));
                    let status = if is_error {
                        ToolStatus::Error(result)
                    } else {
                        ToolStatus::Success(result)
                    };
                    let idx = message.tool_call_id.as_deref().and_then(|id| {
                        self.blocks
                            .iter()
                            .rposition(|b| matches!(b, UIBlock::Tool(t) if t.id == id))
                    });
                    match idx {
                        Some(i) => {
                            if let UIBlock::Tool(t) = &mut self.blocks[i] {
                                t.status = status;
                            }
                        }
                        None => {
                            // Orphaned result (no matching call) — show it standalone.
                            self.blocks.push(UIBlock::Tool(ToolCallEntry {
                                id: String::new(),
                                name: message.name.unwrap_or_else(|| "tool".to_string()),
                                arguments: String::new(),
                                status,
                                started_at: Instant::now(),
                                elapsed_ms: 0,
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
        self.add_assistant_notice(format!("Resumed session `{}`.", session_id));
    }

    pub(crate) fn slash_suggestions(&self) -> Vec<SlashCommand> {
        slash_suggestions(&self.input, self.skills.as_deref())
    }

    pub(crate) fn reset_slash_selection(&mut self) {
        let suggestions_len = self.slash_suggestions().len();
        if suggestions_len == 0 {
            self.slash_selection = 0;
        } else {
            self.slash_selection = self.slash_selection.min(suggestions_len - 1);
        }
    }

    pub(crate) fn select_slash_suggestion(&mut self, direction: SelectionDirection) -> bool {
        self.exit_pending = false;
        let suggestions = self.slash_suggestions();
        if suggestions.is_empty() {
            return false;
        }

        self.slash_selection = next_selection(self.slash_selection, suggestions.len(), direction);
        true
    }

    /// Returns the currently selected slash suggestion name, if any.
    pub(crate) fn selected_slash_command(&self) -> Option<String> {
        let suggestions = self.slash_suggestions();
        if suggestions.is_empty() {
            return None;
        }
        let idx = self.slash_selection.min(suggestions.len() - 1);
        Some(suggestions[idx].name.to_string())
    }

    pub(crate) fn history_prev(&mut self) {
        self.exit_pending = false;
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.saved_input = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.input = self.history[idx].clone();
        self.cursor = self.input.len();
    }

    pub(crate) fn history_next(&mut self) {
        self.exit_pending = false;
        let idx = match self.history_idx {
            None => return,
            Some(i) => i,
        };
        if idx + 1 >= self.history.len() {
            self.history_idx = None;
            self.input = self.saved_input.clone();
        } else {
            self.history_idx = Some(idx + 1);
            self.input = self.history[idx + 1].clone();
        }
        self.cursor = self.input.len();
    }

    /// Update pending tool elapsed times each tick
    pub(crate) fn tick_update(&mut self) {
        self.tick += 1;
        // Clear expired error flashes (3s)
        if let Some((_, t)) = &self.error_flash {
            if t.elapsed().as_secs() >= 3 {
                self.error_flash = None;
            }
        }
    }

    pub(crate) fn request_exit(&mut self) {
        if self.exit_pending {
            self.should_quit = true;
        } else {
            self.exit_pending = true;
        }
    }

    pub(crate) fn clear_exit_hint(&mut self) {
        self.exit_pending = false;
    }

    pub(crate) fn copy_last_assistant_message(&mut self) {
        let text = self.blocks.iter().rev().find_map(|b| match b {
            UIBlock::Assistant(text) => Some(text.clone()),
            _ => None,
        });

        match text {
            Some(content) => match (self.clipboard_fn)(&content) {
                Ok(()) => self.add_assistant_notice("Copied to clipboard.".to_string()),
                Err(e) => self.add_assistant_notice(format!("Copy failed: {e}")),
            },
            None => self.add_assistant_notice("Nothing to copy.".to_string()),
        }
    }
}

#[cfg(test)]
mod copy_tests {
    use super::{App, UIBlock};
    use std::path::PathBuf;

    /// A clipboard mock that always succeeds.
    fn mock_clipboard(_text: &str) -> Result<(), String> {
        Ok(())
    }

    fn test_app() -> App {
        let mut app = App::new(
            "p".to_string(),
            "m".to_string(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        );
        app.clipboard_fn = mock_clipboard;
        app
    }

    #[test]
    fn copy_finds_last_assistant_message() {
        let mut app = test_app();
        app.blocks.push(UIBlock::User("user msg".to_string()));
        app.blocks
            .push(UIBlock::Assistant("first reply".to_string()));
        app.blocks.push(UIBlock::User("user msg 2".to_string()));
        app.blocks
            .push(UIBlock::Assistant("last reply".to_string()));

        app.copy_last_assistant_message();

        let last = app.blocks.last().unwrap();
        assert!(
            matches!(last, UIBlock::Assistant(text) if text == "Copied to clipboard."),
            "Expected success notice, got {:?}",
            last
        );
    }

    #[test]
    fn copy_skips_tool_blocks() {
        let mut app = test_app();
        app.blocks.push(UIBlock::User("user msg".to_string()));
        app.blocks.push(UIBlock::Assistant("reply".to_string()));
        app.blocks.push(UIBlock::Tool(super::ToolCallEntry {
            id: "1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            status: super::ToolStatus::Success("ok".to_string()),
            started_at: std::time::Instant::now(),
            elapsed_ms: 0,
        }));

        app.copy_last_assistant_message();

        let last = app.blocks.last().unwrap();
        assert!(
            matches!(last, UIBlock::Assistant(text) if text == "Copied to clipboard."),
            "Expected copy of 'reply', got {:?}",
            last
        );
    }

    #[test]
    fn copy_when_nothing_to_copy() {
        let mut app = test_app();
        app.blocks.push(UIBlock::User("user only".to_string()));

        app.copy_last_assistant_message();

        let last = app.blocks.last().unwrap();
        assert!(
            matches!(last, UIBlock::Assistant(text) if text == "Nothing to copy."),
            "Expected 'Nothing to copy.' notice, got {:?}",
            last
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_suggestions_show_all_commands_for_slash() {
        let suggestions = slash_suggestions("/", None);

        assert_eq!(
            suggestions
                .iter()
                .map(|command| command.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["/resume", "/clear", "/compact", "/copy", "/model", "/skills"]
        );
    }

    #[test]
    fn slash_suggestions_filter_by_command_name_or_description() {
        assert_eq!(slash_suggestions("/res", None)[0].name.as_ref(), "/resume");
        assert_eq!(slash_suggestions("/list", None)[0].name.as_ref(), "/resume");
        // `/new` is merged into `/clear`: typing it still surfaces /clear via
        // its description ("Start a new session").
        assert_eq!(slash_suggestions("/new", None)[0].name.as_ref(), "/clear");
        assert_eq!(slash_suggestions("/clear", None)[0].name.as_ref(), "/clear");
    }

    #[test]
    fn slash_suggestions_stop_after_first_argument() {
        assert!(slash_suggestions("/resume default", None).is_empty());
    }

    fn test_app() -> App {
        App::new(
            "p".to_string(),
            "m".to_string(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn switching_models_retargets_context_window() {
        let mut app = test_app();
        app.fallback_context_window = 100_000;
        app.context_window = 500_000; // pretend a prior known window
        app.set_model_options(
            vec![
                crate::models::ModelOption {
                    provider: "x".to_string(),
                    model: "big".to_string(),
                    effort_levels: vec![],
                    context: Some(1_000_000),
                },
                crate::models::ModelOption {
                    provider: "x".to_string(),
                    model: "unknown".to_string(),
                    effort_levels: vec![],
                    context: None,
                },
            ],
            None,
        );
        // Switch to a known-context model → gauge uses its window.
        app.model_picker = Some(ModelPicker {
            selected: 0,
            effort_idx: 0,
        });
        app.apply_model_selection();
        assert_eq!(app.context_window, 1_000_000);
        // Switch to an unknown-context model → gauge resets to the fallback,
        // not the previous model's window.
        app.model_picker = Some(ModelPicker {
            selected: 1,
            effort_idx: 0,
        });
        app.apply_model_selection();
        assert_eq!(app.context_window, 100_000);
    }

    fn picker_app() -> App {
        let mut app = App::new(
            "deepseek".to_string(),
            "deepseek-v4-flash".to_string(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        );
        let opt = |provider: &str, model: &str, levels: &[&str]| crate::models::ModelOption {
            provider: provider.to_string(),
            model: model.to_string(),
            effort_levels: levels.iter().map(|s| s.to_string()).collect(),
            context: None,
        };
        app.set_model_options(
            vec![
                opt("deepseek", "deepseek-v4-flash", &[]),
                opt("deepseek", "deepseek-v4-pro", &["high", "max"]),
                opt("kimi-code", "kimi-for-coding", &[]),
            ],
            None,
        );
        app
    }

    #[test]
    fn show_model_picker_highlights_active_model() {
        let mut app = picker_app();
        app.show_model_picker();
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn cycle_effort_wraps_and_is_inert_for_non_reasoning_models() {
        let mut app = picker_app();
        app.show_model_picker();
        // Move to deepseek-v4-pro (has levels high/max).
        app.select_model_picker(SelectionDirection::Next);
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 1);
        app.cycle_effort(SelectionDirection::Next); // high -> max
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 1);
        app.cycle_effort(SelectionDirection::Next); // wraps max -> high
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 0);
        // Moving to kimi-code (no levels) clamps effort and cycling no-ops.
        app.cycle_effort(SelectionDirection::Next); // -> max (idx 1)
        app.select_model_picker(SelectionDirection::Next);
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 0);
        app.cycle_effort(SelectionDirection::Next);
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 0);
    }

    #[test]
    fn apply_model_selection_returns_choice_and_updates_display() {
        let mut app = picker_app();
        app.show_model_picker();
        app.select_model_picker(SelectionDirection::Next); // deepseek-v4-pro
        app.cycle_effort(SelectionDirection::Next); // max
        let (provider, model, effort) = app.apply_model_selection().unwrap();
        assert_eq!(
            (provider.as_str(), model.as_str(), effort.as_deref()),
            ("deepseek", "deepseek-v4-pro", Some("max"))
        );
        assert_eq!(app.provider, "deepseek");
        assert_eq!(app.model, "deepseek-v4-pro");
        assert_eq!(app.effort.as_deref(), Some("max"));
        assert!(app.model_picker.is_none());
    }

    #[test]
    fn apply_non_reasoning_model_clears_effort() {
        let mut app = picker_app();
        app.effort = Some("max".to_string()); // a stale prior effort
        app.show_model_picker(); // selected = deepseek-v4-flash (no levels)
        let (_, model, effort) = app.apply_model_selection().unwrap();
        assert_eq!(model, "deepseek-v4-flash");
        assert_eq!(effort, None);
        assert_eq!(app.effort, None);
    }

    #[test]
    fn editing_multibyte_chars_keeps_cursor_on_boundary() {
        // Regression: typing a CJK char (3 bytes) used to advance the cursor by
        // 1 byte, landing mid-character and panicking on the next slice/render.
        let mut app = test_app();
        for c in "中a文".chars() {
            app.insert_char(c);
            assert!(
                app.input.is_char_boundary(app.cursor),
                "cursor must stay on a char boundary after insert"
            );
        }
        assert_eq!(app.input, "中a文");
        assert_eq!(app.cursor, app.input.len());

        // Slicing at the cursor (what draw_input does) must not panic.
        let _ = &app.input[..app.cursor];

        // Walk left across every char, then delete them.
        for _ in 0..3 {
            app.move_left();
            assert!(app.input.is_char_boundary(app.cursor));
        }
        assert_eq!(app.cursor, 0);

        app.cursor = app.input.len();
        app.backspace(); // removes "文"
        assert_eq!(app.input, "中a");
        assert!(app.input.is_char_boundary(app.cursor));
        assert_eq!(app.cursor, app.input.len());
    }

    #[test]
    fn sanitize_expands_tabs_and_drops_control_chars() {
        use super::super::sanitize;
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("a\r\nb"), "ab"); // CR and LF dropped (lines split upstream)
        assert_eq!(sanitize("x\x1b[31my"), "x[31my"); // ESC dropped
        assert_eq!(sanitize("正常 text"), "正常 text"); // multibyte untouched
    }

    #[test]
    fn format_tokens_is_human_friendly() {
        use super::super::format_tokens;
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(120_000), "120.0k");
    }

    #[test]
    fn truncate_is_char_safe() {
        use super::super::truncate;
        // Must not panic slicing mid-codepoint, and counts chars not bytes.
        assert_eq!(truncate("中文字测试", 3), "中文字…");
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn next_selection_wraps_in_both_directions() {
        assert_eq!(next_selection(0, 2, SelectionDirection::Previous), 1);
        assert_eq!(next_selection(1, 2, SelectionDirection::Next), 0);
        assert_eq!(next_selection(0, 0, SelectionDirection::Next), 0);
    }

    #[test]
    fn enqueue_appends_without_touching_blocks() {
        let mut app = test_app();
        app.enqueue("first".to_string());
        app.enqueue("second".to_string());
        assert_eq!(app.queue, vec!["first", "second"]);
        // Queueing must NOT push a transcript block (ordering correctness).
        assert!(app.blocks.is_empty());
    }

    #[test]
    fn take_queued_front_is_fifo() {
        let mut app = test_app();
        app.enqueue("a".to_string());
        app.enqueue("b".to_string());
        assert_eq!(app.take_queued_front(), Some("a".to_string()));
        assert_eq!(app.take_queued_front(), Some("b".to_string()));
        assert_eq!(app.take_queued_front(), None);
    }

    #[test]
    fn recall_last_queued_moves_last_item_into_input() {
        let mut app = test_app();
        app.enqueue("older".to_string());
        app.enqueue("newest".to_string());
        assert!(app.recall_last_queued());
        assert_eq!(app.input, "newest");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(app.queue, vec!["older"]);
        // Empty queue → no-op.
        app.input.clear();
        app.queue.clear();
        assert!(!app.recall_last_queued());
        assert_eq!(app.input, "");
    }

    #[test]
    fn agent_end_sets_flag_and_reconciles_pending_injects() {
        let mut app = test_app();
        app.turn_in_flight = true;
        app.mode = Mode::Thinking;
        app.pending_injects = vec!["stranded".to_string()];
        app.queue = vec!["queued".to_string()];
        app.handle_event(AgentEvent::AgentEnd);
        assert_eq!(app.mode, Mode::Idle);
        assert!(app.take_turn_just_ended());
        // Second take clears it.
        assert!(!app.take_turn_just_ended());
        // Stranded inject prepended ahead of existing queue.
        assert_eq!(app.queue, vec!["stranded", "queued"]);
        assert!(app.pending_injects.is_empty());
    }

    #[test]
    fn agent_end_drains_even_without_agent_start() {
        // A provider-build / session-open error ends the turn *before* AgentStart,
        // so `mode` is still Idle at AgentEnd. The drain must still fire (keyed on
        // turn_in_flight, not mode) or a queued prompt after a failed one stalls.
        let mut app = test_app();
        app.turn_in_flight = true; // dispatched, but no AgentStart arrived
        assert_eq!(app.mode, Mode::Idle);
        app.handle_event(AgentEvent::AgentEnd);
        assert!(
            app.take_turn_just_ended(),
            "a turn that failed before AgentStart still drains the queue"
        );
    }

    #[test]
    fn user_injected_pushes_block_records_history_pops_pending() {
        let mut app = test_app();
        app.pending_injects = vec!["steer".to_string()];
        app.handle_event(AgentEvent::UserInjected {
            text: "steer".to_string(),
        });
        assert!(matches!(app.blocks.last(), Some(UIBlock::User(t)) if t == "steer"));
        assert_eq!(app.history.last().map(|s| s.as_str()), Some("steer"));
        assert!(app.pending_injects.is_empty());
    }

    #[test]
    fn duplicate_agent_end_does_not_double_drain() {
        // A persistence error after the agent loop already emitted AgentEnd can
        // produce a second AgentEnd. Only the first (dispatched turn) arms the
        // drain; the duplicate must not, or the queue would drain twice.
        let mut app = test_app();
        app.turn_in_flight = true;
        app.handle_event(AgentEvent::AgentEnd);
        assert!(app.take_turn_just_ended(), "real turn-end arms the drain");
        app.handle_event(AgentEvent::AgentEnd); // duplicate, no turn in flight
        assert!(
            !app.take_turn_just_ended(),
            "duplicate AgentEnd must not re-arm the drain"
        );
    }

    #[test]
    fn user_injected_with_empty_pending_still_pushes() {
        // An inject drained at run-start (not via Ctrl+S) has no pending entry,
        // but must still appear in the transcript + history without panicking.
        let mut app = test_app();
        assert!(app.pending_injects.is_empty());
        app.handle_event(AgentEvent::UserInjected {
            text: "from-start".to_string(),
        });
        assert!(matches!(app.blocks.last(), Some(UIBlock::User(t)) if t == "from-start"));
        assert_eq!(app.history.last().map(|s| s.as_str()), Some("from-start"));
    }

    #[test]
    fn skill_picker_toggles_selected() {
        use std::sync::Arc;
        let _env = crate::util::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = crate::util::unique_temp_dir("ignis-app-skillpicker");
        let cwd = tmp.join("proj");
        for n in ["alpha", "beta"] {
            let dir = cwd.join(".ignis/skills").join(n);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\nname: {n}\n---\nbody")).unwrap();
        }
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = App::new("p".into(), "m".into(), "s".into(), cwd.clone());
        app.skills = Some(Arc::new(crate::skills::SkillRegistry::load(
            None,
            &cwd,
            std::collections::HashSet::new(),
        )));
        app.show_skill_picker();
        assert!(app.skill_picker.is_some());
        let (name, now) = app.toggle_selected_skill().unwrap();
        assert_eq!(name, "alpha"); // row 0, sorted
        assert!(!now); // disabled
        assert!(!app.skills.as_deref().unwrap().is_enabled("alpha"));

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
