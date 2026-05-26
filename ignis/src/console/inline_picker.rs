//! State machine + ratatui renderer for the tool-initiated `ask_user`
//! picker. Lives in a fixed band at the bottom of the inline viewport (the
//! same band /model and /skills use), but is opened by the agent loop via the
//! shared `picker_tx` mpsc channel rather than by a slash command.
//!
//! After the last question is answered (or ESC is pressed), the console takes
//! the response out of this state, fires it on the oneshot reply, commits a
//! 1-line trace to scrollback, and clears `App.inline_picker`.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::picker::{PickerAnswer, PickerQuestion, PickerRequest, PickerResponse};
use crate::tools::ask_user::{MAX_OTHER_LEN, OTHER_LABEL};

// The picker reuses the existing app palette. Importing concrete colors
// here keeps the renderer self-contained; the names mirror the ones in
// console/app.rs (BG/TEXT/ACCENT/MAUVE/TEXT_DIM).
use super::{
    ACCENT, BG, BORDER as BORDER_DIM, GREEN as OK_GREEN, MAUVE, RED as ERR_RED, TEXT, TEXT_DIM,
};

/// What `on_key` tells the console to do.
#[derive(Debug)]
pub(crate) enum KeyOutcome {
    /// State changed (or didn't) — re-render and keep going.
    Continue,
    /// User pressed ESC — caller should send `Cancelled` and clear the picker.
    Cancel,
    /// User finished the last question — caller should send `Answered(_)` and
    /// clear the picker. The vec is also handed back for the scrollback trace.
    Done(Vec<PickerAnswer>),
}

pub(crate) struct InlinePickerState {
    pub(crate) questions: Vec<PickerQuestion>,
    /// One per already-answered question; length == `current` while open.
    answers: Vec<PickerAnswer>,
    /// Index of the question being answered right now.
    current: usize,
    /// 0..=current_question.options.len(); the last index is the virtual
    /// "Other" row.
    cursor: usize,
    /// Multi-select only; len == current_question.options.len().
    toggled: Vec<bool>,
    /// Free-text buffer for the Other row. In multi-select, Other is
    /// considered "included" iff this buffer is non-empty — no separate
    /// toggle flag, so the user can freely type spaces into it.
    other_buf: String,
    /// Taken on `Done` / `Cancel` so the caller can `send` on it.
    pub(crate) reply: Option<tokio::sync::oneshot::Sender<PickerResponse>>,
}

impl InlinePickerState {
    pub(crate) fn new(request: PickerRequest) -> Self {
        let first_opts = request.questions[0].options.len();
        Self {
            questions: request.questions,
            answers: Vec::new(),
            current: 0,
            cursor: 0,
            toggled: vec![false; first_opts],
            other_buf: String::new(),
            reply: Some(request.reply),
        }
    }

    pub(crate) fn current_question(&self) -> &PickerQuestion {
        &self.questions[self.current]
    }
    pub(crate) fn total(&self) -> usize {
        self.questions.len()
    }
    pub(crate) fn current_index(&self) -> usize {
        self.current
    }
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }
    pub(crate) fn other_buf(&self) -> &str {
        &self.other_buf
    }
    pub(crate) fn is_multi(&self) -> bool {
        self.current_question().multi_select
    }
    pub(crate) fn is_toggled(&self, opt_idx: usize) -> bool {
        self.toggled.get(opt_idx).copied().unwrap_or(false)
    }
    /// Multi-select: Other is "included" iff its free-text buffer has any
    /// non-whitespace content. No separate toggle key — keeps space free to
    /// be typed into the buffer.
    pub(crate) fn other_included(&self) -> bool {
        !self.other_buf.trim().is_empty()
    }
    pub(crate) fn other_focused(&self) -> bool {
        self.cursor == self.current_question().options.len()
    }
    /// Highlighted option's preview text, if any. Returns None when Other is
    /// focused (Other has no preview).
    pub(crate) fn focused_preview(&self) -> Option<&str> {
        let q = self.current_question();
        q.options
            .get(self.cursor)
            .and_then(|o| o.preview.as_deref())
    }

    /// Apply a key event; returns what the caller should do.
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        // ESC always cancels regardless of focus.
        if matches!(key.code, KeyCode::Esc) {
            return KeyOutcome::Cancel;
        }
        // Ctrl-C also cancels — same expectation as everywhere else in the TUI.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return KeyOutcome::Cancel;
        }
        let opts_n = self.current_question().options.len();
        let last = opts_n; // Other row index
        match key.code {
            KeyCode::Up => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                KeyOutcome::Continue
            }
            KeyCode::Down => {
                if self.cursor < last {
                    self.cursor += 1;
                }
                KeyOutcome::Continue
            }
            KeyCode::Enter => self.try_advance(),
            // When Other has focus, typing (incl. space) goes into the buffer.
            // Must come BEFORE the multi-select space toggle so users can type
            // multi-word answers like "another approach" into Other.
            KeyCode::Char(c) if self.other_focused() => {
                // Strip control chars; cap length.
                if !c.is_control() && self.other_buf.len() + c.len_utf8() <= MAX_OTHER_LEN {
                    self.other_buf.push(c);
                }
                KeyOutcome::Continue
            }
            KeyCode::Char(' ') if self.is_multi() && self.cursor < opts_n => {
                self.toggled[self.cursor] = !self.toggled[self.cursor];
                KeyOutcome::Continue
            }
            KeyCode::Backspace if self.other_focused() => {
                self.other_buf.pop();
                KeyOutcome::Continue
            }
            _ => KeyOutcome::Continue,
        }
    }

    /// Try to commit the current question's selection and advance. Returns
    /// `Continue` if the user hasn't picked anything yet (no-op), `Done` when
    /// the LAST question is answered, or `Continue` after advancing to the
    /// next question.
    fn try_advance(&mut self) -> KeyOutcome {
        let q = self.current_question();
        let pick: Option<PickerAnswer> = if q.multi_select {
            let mut picks: Vec<String> = q
                .options
                .iter()
                .enumerate()
                .filter(|(i, _)| self.toggled[*i])
                .map(|(_, o)| o.label.clone())
                .collect();
            // Multi-select: include Other iff buffer has content. No separate
            // toggle key — space is reserved for typing into Other.
            if !self.other_buf.trim().is_empty() {
                picks.push(self.other_buf.trim().to_string());
            }
            if picks.is_empty() {
                None
            } else {
                Some(PickerAnswer::Multi(picks))
            }
        } else if self.other_focused() {
            let text = self.other_buf.trim();
            if text.is_empty() {
                None
            } else {
                Some(PickerAnswer::Single(text.to_string()))
            }
        } else {
            Some(PickerAnswer::Single(q.options[self.cursor].label.clone()))
        };

        let Some(answer) = pick else {
            return KeyOutcome::Continue;
        };
        self.answers.push(answer);
        if self.current + 1 >= self.questions.len() {
            // All done — drain answers out.
            let done = std::mem::take(&mut self.answers);
            return KeyOutcome::Done(done);
        }
        // Advance to the next question, reset cursor/toggled/Other.
        self.current += 1;
        let next_opts = self.current_question().options.len();
        self.cursor = 0;
        self.toggled = vec![false; next_opts];
        self.other_buf.clear();
        KeyOutcome::Continue
    }
}

// ---------- renderer ----------

/// Lines for the picker band (used by render::draw when inline_picker is open).
pub(crate) fn render_inline_picker(lines: &mut Vec<Line<'static>>, state: &InlinePickerState) {
    let q = state.current_question();
    // Header strip
    let progress = if state.total() > 1 {
        format!(" · {}/{}", state.current_index() + 1, state.total())
    } else {
        String::new()
    };
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "ask_user",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            q.header.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(progress, Style::default().fg(TEXT_DIM)),
    ]));
    // Question text — kept tight against the options it controls.
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(q.question.clone(), Style::default().fg(TEXT)),
    ]));

    // Options
    let opts_n = q.options.len();
    let cursor = state.cursor();
    let multi = state.is_multi();
    for (idx, opt) in q.options.iter().enumerate() {
        let selected = idx == cursor;
        let prefix = if multi {
            if state.is_toggled(idx) {
                "[x] "
            } else {
                "[ ] "
            }
        } else if selected {
            "▸ "
        } else {
            "  "
        };
        let row_style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let desc_style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT_DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {prefix}"), row_style),
            Span::styled(opt.label.clone(), row_style.add_modifier(Modifier::BOLD)),
            Span::styled(" — ", desc_style),
            Span::styled(opt.description.clone(), desc_style),
        ]));
    }
    // Other row
    let other_selected = cursor == opts_n;
    let other_prefix = if multi {
        if state.other_included() {
            "[x] "
        } else {
            "[ ] "
        }
    } else if other_selected {
        "▸ "
    } else {
        "  "
    };
    let other_row_style = if other_selected {
        Style::default().fg(BG).bg(ACCENT)
    } else {
        Style::default().fg(TEXT)
    };
    let other_buf = state.other_buf();
    if other_selected {
        lines.push(Line::from(vec![
            Span::styled(format!("  {other_prefix}"), other_row_style),
            Span::styled(OTHER_LABEL, other_row_style.add_modifier(Modifier::ITALIC)),
            Span::styled("  ", other_row_style),
            Span::styled(other_buf.to_string(), other_row_style),
            Span::styled("_", other_row_style.add_modifier(Modifier::SLOW_BLINK)),
        ]));
    } else {
        // Show the buffer too when Other is toggled but not focused (multi).
        let suffix = if multi && state.other_included() && !other_buf.is_empty() {
            format!(": {other_buf}")
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {other_prefix}"), other_row_style),
            Span::styled(
                format!("{OTHER_LABEL}{suffix}"),
                other_row_style.add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    // Preview block when the highlighted option has one
    if let Some(preview) = state.focused_preview() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Preview:",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        )));
        for line in preview.lines() {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(line.to_string(), Style::default().fg(TEXT_DIM)),
            ]));
        }
    }

    // Footer
    lines.push(Line::from(""));
    let footer = if state.other_focused() {
        if multi {
            "  type text · space toggle · ↵ confirm · esc cancel"
        } else {
            "  type text · ↵ confirm · esc cancel"
        }
    } else if multi {
        "  ↑/↓ navigate · space toggle · ↵ confirm · esc cancel"
    } else {
        "  ↑/↓ navigate · ↵ select · esc cancel"
    };
    lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(TEXT_DIM),
    )));
}

/// Height (rows) the picker needs given its current state. Used by
/// `render::live_height` so the inline viewport recreates at the right size.
pub(crate) fn picker_height(state: &InlinePickerState) -> u16 {
    // Layout: blank · header · question · options · Other · [preview block] ·
    // blank · footer.
    let opts = state.current_question().options.len() as u16;
    let preview_rows = state.focused_preview().map_or(0, |p| {
        2 + p.lines().count() as u16 /* blank + "Preview:" + N */
    });
    3 + opts + 1 + preview_rows + 1 + 1
}

/// One-line trace committed to scrollback after the picker closes. Single-line
/// answers stay on one line; multi answers are joined with ", "; cancellation
/// gets a red marker.
pub(crate) fn trace_lines(
    questions: &[PickerQuestion],
    response: &PickerResponse,
) -> Vec<Line<'static>> {
    match response {
        PickerResponse::Cancelled => vec![Line::from(vec![
            Span::styled("  ✗ ", Style::default().fg(ERR_RED)),
            Span::styled(
                "ask_user",
                Style::default().fg(ERR_RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · cancelled by user", Style::default().fg(TEXT_DIM)),
        ])],
        PickerResponse::Answered(answers) => {
            let mut out: Vec<Line<'static>> = Vec::with_capacity(answers.len());
            for (q, a) in questions.iter().zip(answers) {
                let answer_text = match a {
                    PickerAnswer::Single(s) => s.clone(),
                    PickerAnswer::Multi(v) => v.join(", "),
                };
                out.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(OK_GREEN)),
                    Span::styled(
                        "ask_user",
                        Style::default().fg(OK_GREEN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" · ", Style::default().fg(BORDER_DIM)),
                    Span::styled(
                        q.header.clone(),
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(": ", Style::default().fg(TEXT_DIM)),
                    Span::styled(answer_text, Style::default().fg(TEXT)),
                ]));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{PickerOption, PickerQuestion};

    fn make_request(
        qs: Vec<PickerQuestion>,
    ) -> (
        InlinePickerState,
        tokio::sync::oneshot::Receiver<PickerResponse>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let req = PickerRequest {
            questions: qs,
            reply: tx,
        };
        (InlinePickerState::new(req), rx)
    }

    fn q(question: &str, header: &str, multi: bool, labels: &[&str]) -> PickerQuestion {
        PickerQuestion {
            question: question.to_string(),
            header: header.to_string(),
            multi_select: multi,
            options: labels
                .iter()
                .map(|l| PickerOption {
                    label: l.to_string(),
                    description: format!("desc-{l}"),
                    preview: None,
                })
                .collect(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn down_arrow_advances_cursor_and_stops_at_other() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        assert_eq!(s.cursor(), 0);
        let _ = s.on_key(key(KeyCode::Down));
        assert_eq!(s.cursor(), 1);
        let _ = s.on_key(key(KeyCode::Down));
        assert_eq!(s.cursor(), 2); // Other
        assert!(s.other_focused());
        let _ = s.on_key(key(KeyCode::Down));
        assert_eq!(s.cursor(), 2); // saturates
    }

    #[test]
    fn up_arrow_stops_at_top() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        let _ = s.on_key(key(KeyCode::Up));
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn esc_cancels() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        assert!(matches!(s.on_key(key(KeyCode::Esc)), KeyOutcome::Cancel));
    }

    #[test]
    fn ctrl_c_cancels() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(s.on_key(ev), KeyOutcome::Cancel));
    }

    #[test]
    fn enter_single_select_returns_done() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["alpha", "beta"])]);
        let _ = s.on_key(key(KeyCode::Down));
        let outcome = s.on_key(key(KeyCode::Enter));
        match outcome {
            KeyOutcome::Done(ans) => {
                assert_eq!(ans.len(), 1);
                assert_eq!(ans[0], PickerAnswer::Single("beta".to_string()));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn space_toggles_only_in_multi() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        let _ = s.on_key(key(KeyCode::Char(' ')));
        assert!(!s.is_toggled(0), "space should not toggle in single-select");

        let (mut s, _rx) = make_request(vec![q("Q?", "h", true, &["a", "b"])]);
        let _ = s.on_key(key(KeyCode::Char(' ')));
        assert!(s.is_toggled(0));
        let _ = s.on_key(key(KeyCode::Char(' ')));
        assert!(!s.is_toggled(0));
    }

    #[test]
    fn multi_select_enter_requires_at_least_one_pick() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", true, &["a", "b"])]);
        // No toggles → enter is a no-op
        let out = s.on_key(key(KeyCode::Enter));
        assert!(matches!(out, KeyOutcome::Continue));
        // Toggle one → enter Done
        let _ = s.on_key(key(KeyCode::Char(' ')));
        match s.on_key(key(KeyCode::Enter)) {
            KeyOutcome::Done(ans) => {
                assert_eq!(ans[0], PickerAnswer::Multi(vec!["a".to_string()]));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn other_single_select_typed_text_becomes_answer() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        // Move down to Other
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down));
        assert!(s.other_focused());
        // Empty Other → enter no-op
        assert!(matches!(
            s.on_key(key(KeyCode::Enter)),
            KeyOutcome::Continue
        ));
        // Type
        for c in "my-thing".chars() {
            let _ = s.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(s.other_buf(), "my-thing");
        // Enter → Done with the typed text
        match s.on_key(key(KeyCode::Enter)) {
            KeyOutcome::Done(ans) => {
                assert_eq!(ans[0], PickerAnswer::Single("my-thing".to_string()));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn other_multi_select_auto_includes_when_buffer_nonempty() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", true, &["a", "b"])]);
        // Move to Other and type a multi-word answer (with a space). Space
        // must reach the buffer — it must NOT toggle a checkbox.
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down));
        for c in "another approach".chars() {
            let _ = s.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(s.other_buf(), "another approach");
        assert!(s.other_included());
        match s.on_key(key(KeyCode::Enter)) {
            KeyOutcome::Done(ans) => {
                assert_eq!(
                    ans[0],
                    PickerAnswer::Multi(vec!["another approach".to_string()])
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn backspace_to_empty_buffer_drops_other_inclusion() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", true, &["a", "b"])]);
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down));
        for c in "ab".chars() {
            let _ = s.on_key(key(KeyCode::Char(c)));
        }
        assert!(s.other_included(), "non-empty buffer → included");
        let _ = s.on_key(key(KeyCode::Backspace));
        let _ = s.on_key(key(KeyCode::Backspace));
        assert_eq!(s.other_buf(), "");
        assert!(!s.other_included(), "empty buffer → not included");
    }

    #[test]
    fn two_question_flow_advances_between_questions() {
        let (mut s, _rx) = make_request(vec![
            q("Q1?", "h1", false, &["x", "y"]),
            q("Q2?", "h2", false, &["p", "q"]),
        ]);
        assert_eq!(s.current_index(), 0);
        let _ = s.on_key(key(KeyCode::Enter)); // pick "x"
        assert_eq!(s.current_index(), 1);
        assert_eq!(s.cursor(), 0); // reset
        let _ = s.on_key(key(KeyCode::Down)); // move to "q"
        match s.on_key(key(KeyCode::Enter)) {
            KeyOutcome::Done(ans) => {
                assert_eq!(ans.len(), 2);
                assert_eq!(ans[0], PickerAnswer::Single("x".to_string()));
                assert_eq!(ans[1], PickerAnswer::Single("q".to_string()));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn other_buf_caps_at_max_len() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down));
        // Try to type way more than MAX_OTHER_LEN
        for _ in 0..(MAX_OTHER_LEN + 1000) {
            let _ = s.on_key(key(KeyCode::Char('x')));
        }
        assert_eq!(s.other_buf().len(), MAX_OTHER_LEN);
    }

    #[test]
    fn control_chars_in_other_buf_are_ignored() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Char('\t')));
        let _ = s.on_key(key(KeyCode::Char('a')));
        let _ = s.on_key(key(KeyCode::Char('\u{007F}')));
        let _ = s.on_key(key(KeyCode::Char('b')));
        assert_eq!(s.other_buf(), "ab");
    }
}
