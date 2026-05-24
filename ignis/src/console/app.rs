use std::path::PathBuf;
use std::time::Instant;

use super::{
    format_duration, next_selection, slash_suggestions, SelectionDirection, SlashCommand, SPINNERS,
};
use crate::types::AgentEvent;

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Idle,
    Thinking,    // LLM is generating
    ToolRunning, // tool execution in progress
}

pub(crate) struct App {
    pub(crate) provider: String,
    pub(crate) model: String,
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

    pub(crate) mode: Mode,
    pub(crate) tick: u64,
    pub(crate) stream_start: Option<Instant>,
    pub(crate) current_chunk_idx: Option<usize>,

    pub(crate) scroll: u16,
    pub(crate) max_scroll: u16,
    pub(crate) user_scrolled: bool, // user manually scrolled up

    pub(crate) should_quit: bool,
    pub(crate) error_flash: Option<(String, Instant)>,
    pub(crate) exit_pending: bool,

    /// Token budget the context-usage % is measured against (the auto-compact
    /// threshold). Estimated, not exact.
    pub(crate) context_window: usize,
}

impl App {
    pub(crate) fn new(provider: String, model: String, session_id: String, cwd: PathBuf) -> Self {
        Self {
            provider,
            model,
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
            mode: Mode::Idle,
            tick: 0,
            stream_start: None,
            current_chunk_idx: None,
            scroll: 0,
            max_scroll: 0,
            user_scrolled: false,
            should_quit: false,
            error_flash: None,
            exit_pending: false,
            context_window: 120_000,
        }
    }

    pub(crate) fn set_context_window(&mut self, window: usize) {
        self.context_window = window;
    }

    /// Estimated share of the context budget used by the transcript (chars/4),
    /// capped at 100. Doubles as "% until auto-compaction".
    pub(crate) fn context_pct(&self) -> u8 {
        if self.context_window == 0 {
            return 0;
        }
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
        let tokens = chars / 4;
        ((tokens * 100 / self.context_window).min(100)) as u8
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
            }
            AgentEvent::TurnStart => {}
            AgentEvent::MessageStart { .. } => {
                self.blocks.push(UIBlock::Assistant(String::new()));
                self.current_chunk_idx = Some(self.blocks.len() - 1);
                self.auto_scroll();
            }
            AgentEvent::MessageUpdate { delta } => {
                if let Some(i) = self.current_chunk_idx {
                    if let Some(UIBlock::Assistant(ref mut s)) = self.blocks.get_mut(i) {
                        s.push_str(&delta);
                    }
                }
                self.auto_scroll();
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
                self.auto_scroll();
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
                self.auto_scroll();
            }
            AgentEvent::TurnEnd => {}
            AgentEvent::AgentEnd => {
                self.mode = Mode::Idle;
                self.current_chunk_idx = None;
                self.stream_start = None;
            }
        }
    }

    pub(crate) fn auto_scroll(&mut self) {
        if !self.user_scrolled {
            self.scroll = self.max_scroll;
        }
    }

    pub(crate) fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
        self.user_scrolled = self.scroll < self.max_scroll;
    }

    pub(crate) fn scroll_down(&mut self, n: u16) {
        self.scroll = (self.scroll + n).min(self.max_scroll);
        if self.scroll >= self.max_scroll {
            self.user_scrolled = false;
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

    pub(crate) fn submit(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        if text.is_empty() || self.mode != Mode::Idle {
            return None;
        }
        self.exit_pending = false;
        self.history.push(text.clone());
        self.history_idx = None;
        self.blocks.push(UIBlock::User(text.clone()));
        self.input.clear();
        self.cursor = 0;
        self.user_scrolled = false;
        self.auto_scroll();
        Some(text)
    }

    pub(crate) fn add_assistant_notice(&mut self, text: String) {
        self.exit_pending = false;
        self.session_picker = None;
        self.blocks.push(UIBlock::Assistant(text));
        self.auto_scroll();
    }

    pub(crate) fn start_new_session(&mut self, session_id: String) {
        self.exit_pending = false;
        self.session_id = session_id;
        self.blocks.clear();
        self.current_chunk_idx = None;
        self.scroll = 0;
        self.max_scroll = 0;
        self.user_scrolled = false;
        self.history_idx = None;
        self.session_picker = None;
        self.add_assistant_notice(format!("Started new session `{}`.", self.session_id));
    }

    pub(crate) fn show_session_picker(&mut self, sessions: Vec<crate::session::SessionMeta>) {
        self.exit_pending = false;
        self.session_picker = Some(SessionPicker {
            sessions,
            selected: 0,
        });
        self.user_scrolled = false;
        self.auto_scroll();
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

    pub(crate) fn render_session_history(
        &mut self,
        session_id: String,
        messages: Vec<crate::Message>,
    ) {
        self.exit_pending = false;
        self.session_id = session_id.clone();
        self.blocks.clear();
        self.current_chunk_idx = None;
        self.session_picker = None;
        self.scroll = 0;
        self.max_scroll = 0;
        self.user_scrolled = false;

        if messages.is_empty() {
            self.add_assistant_notice(format!("Resumed empty session `{}`.", session_id));
            return;
        }

        for message in messages {
            match message.role.as_str() {
                "user" => {
                    if let Some(content) = message.content {
                        self.blocks.push(UIBlock::User(content));
                    }
                }
                "assistant" => {
                    let mut content = message.content.unwrap_or_default();
                    if content.is_empty() {
                        if let Some(reasoning) = &message.reasoning_content {
                            if !reasoning.is_empty() {
                                content = format!("<thinking>\n{}\n</thinking>", reasoning);
                            }
                        }
                    }
                    if content.is_empty() {
                        if let Some(tool_calls) = message.tool_calls {
                            let names = tool_calls
                                .iter()
                                .map(|tc| tc.function.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            content = format!("Called tool(s): {}", names);
                        }
                    }
                    if !content.is_empty() {
                        self.blocks.push(UIBlock::Assistant(content));
                    }
                }
                "tool" => {
                    let name = message.name.unwrap_or_else(|| "tool".to_string());
                    let content = message.content.unwrap_or_default();
                    self.blocks
                        .push(UIBlock::Assistant(format!("Tool `{}`: {}", name, content)));
                }
                _ => {}
            }
        }
        self.add_assistant_notice(format!("Resumed session `{}`.", session_id));
    }

    pub(crate) fn slash_suggestions(&self) -> Vec<SlashCommand> {
        slash_suggestions(&self.input)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_suggestions_show_all_commands_for_slash() {
        let suggestions = slash_suggestions("/");

        assert_eq!(
            suggestions
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            vec!["/resume", "/clear", "/compact"]
        );
    }

    #[test]
    fn slash_suggestions_filter_by_command_name_or_description() {
        assert_eq!(slash_suggestions("/res")[0].name, "/resume");
        assert_eq!(slash_suggestions("/list")[0].name, "/resume");
        // `/new` is merged into `/clear`: typing it still surfaces /clear via
        // its description ("Start a new session").
        assert_eq!(slash_suggestions("/new")[0].name, "/clear");
        assert_eq!(slash_suggestions("/clear")[0].name, "/clear");
    }

    #[test]
    fn slash_suggestions_stop_after_first_argument() {
        assert!(slash_suggestions("/resume default").is_empty());
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
    fn next_selection_wraps_in_both_directions() {
        assert_eq!(next_selection(0, 2, SelectionDirection::Previous), 1);
        assert_eq!(next_selection(1, 2, SelectionDirection::Next), 0);
        assert_eq!(next_selection(0, 0, SelectionDirection::Next), 0);
    }
}
