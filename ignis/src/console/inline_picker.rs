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

use crate::console::picker::{PickerAnswer, PickerQuestion, PickerRequest, PickerResponse};
use crate::tools::ask_user::{MAX_OTHER_LEN, OTHER_LABEL};

// The picker reuses the existing app palette. Importing concrete colors
// here keeps the renderer self-contained; the names mirror the ones in
// console/app.rs (BG/TEXT/ACCENT/MAUVE/TEXT_DIM).
use super::{ACCENT, BORDER as BORDER_DIM, MAUVE, TEXT, TEXT_DIM};

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
    /// True if any option in the current question carries a `preview` field.
    /// Triggers the split layout — the preview block lives in a bordered pane
    /// on the right, with the option list on the left.
    pub(crate) fn has_any_preview(&self) -> bool {
        self.current_question()
            .options
            .iter()
            .any(|o| o.preview.is_some())
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
        let q = self.current_question();
        let opts_n = q.options.len();
        // Last selectable index. When `allow_other` is false the picker has no
        // free-text row (permission/AFK pickers use this) so the cursor can
        // never reach what would have been the Other position.
        let last = if q.allow_other { opts_n } else { opts_n - 1 };
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
            let other = self.other_buf.trim();
            if !other.is_empty() {
                picks.push(other.to_string());
            }
            (!picks.is_empty()).then_some(PickerAnswer::Multi(picks))
        } else if self.other_focused() {
            let text = self.other_buf.trim();
            (!text.is_empty()).then(|| PickerAnswer::Single(text.to_string()))
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

/// Blank + header strip + question. Used by both layouts (single-column and
/// split-with-preview) so they share an identical top section.
pub(crate) fn header_lines(state: &InlinePickerState) -> Vec<Line<'static>> {
    let q = state.current_question();
    let progress = if state.total() > 1 {
        format!(" · {}/{}", state.current_index() + 1, state.total())
    } else {
        String::new()
    };
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("◆ ", Style::default().fg(MAUVE)),
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
        ]),
        Line::from(Span::styled(q.question.clone(), Style::default().fg(TEXT))),
    ]
}

/// Blank + footer hint. Shared by both layouts.
pub(crate) fn footer_lines(state: &InlinePickerState) -> Vec<Line<'static>> {
    let multi = state.is_multi();
    // When Other has focus, space is routed into the free-text buffer (NOT a
    // toggle), so the hint must not advertise a toggle key — Other's
    // inclusion is derived from buffer-non-empty in multi-select mode.
    let footer = if state.other_focused() {
        "type text · ↵ confirm · esc cancel"
    } else if multi {
        "↑/↓ navigate · space toggle · ↵ confirm · esc cancel"
    } else {
        "↑/↓ navigate · ↵ select · esc cancel"
    };
    vec![
        Line::from(""),
        Line::from(Span::styled(footer, Style::default().fg(TEXT_DIM))),
    ]
}

/// Lines for the picker band (used by render::draw when inline_picker is open).
pub(crate) fn render_inline_picker(lines: &mut Vec<Line<'static>>, state: &InlinePickerState) {
    let q = state.current_question();
    lines.extend(header_lines(state));

    // Options (CC-style stacked: title row + description row indented).
    // Everything sits flush left to match CC's reference layout — no extra
    // gutter that ratatui's wrap pipeline sometimes collapses inconsistently.
    let opts_n = q.options.len();
    let cursor = state.cursor();
    let multi = state.is_multi();
    let max_number_width = (opts_n + 1).to_string().len(); // "10" → 2, "9" → 1
    let desc_indent = 2 /*cursor col*/ + max_number_width + 2 /*". "*/;
    let desc_indent_str = " ".repeat(desc_indent);

    for (idx, opt) in q.options.iter().enumerate() {
        let selected = idx == cursor;
        let (clean_label, recommended) = split_recommended(&opt.label);
        let cursor_glyph = if selected { "> " } else { "  " };
        let number_str = format!("{:>w$}. ", idx + 1, w = max_number_width);
        let title_color = if selected { ACCENT } else { TEXT };
        let mut title_spans: Vec<Span<'static>> = vec![
            Span::styled(cursor_glyph, Style::default().fg(ACCENT)),
            Span::styled(number_str, Style::default().fg(TEXT_DIM)),
        ];
        if multi {
            // Multi-select keeps a checkbox between the number and the title.
            let cb = if state.is_toggled(idx) {
                "[x] "
            } else {
                "[ ] "
            };
            title_spans.push(Span::styled(cb, Style::default().fg(TEXT_DIM)));
        }
        title_spans.push(Span::styled(
            clean_label,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ));
        if recommended {
            title_spans.push(Span::styled("  ", Style::default()));
            title_spans.extend(recommended_badge());
        }
        lines.push(Line::from(title_spans));
        // Description row in dim, indented under the title text (the indent
        // is baked into the span content because ratatui's wrap pipeline
        // doesn't reliably preserve a leading plain-style whitespace span).
        if !opt.description.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("{desc_indent_str}{}", opt.description),
                Style::default().fg(TEXT_DIM),
            )));
        }
    }

    // The separator + Other row only render when the question opts in. The
    // permission and AFK pickers set `allow_other = false` — the option set
    // is closed by design (Approve once / Approve session / Deny etc.).
    if !q.allow_other {
        // Footer still needs to render even when we skip the Other block.
        lines.extend(footer_lines(state));
        return;
    }

    // Horizontal separator between the regular options and the "Other" row,
    // matching the CC pattern (image 2).
    lines.push(Line::from(Span::styled(
        "─".repeat(42),
        Style::default().fg(BORDER_DIM),
    )));

    // Other row — always single-line, no description.
    let other_selected = cursor == opts_n;
    let other_cursor = if other_selected { "> " } else { "  " };
    let other_number = format!("{:>w$}. ", opts_n + 1, w = max_number_width);
    let other_color = if other_selected { ACCENT } else { TEXT };
    let other_buf = state.other_buf();
    let mut other_spans: Vec<Span<'static>> = vec![
        Span::styled(other_cursor, Style::default().fg(ACCENT)),
        Span::styled(other_number, Style::default().fg(TEXT_DIM)),
    ];
    if multi {
        let cb = if state.other_included() {
            "[x] "
        } else {
            "[ ] "
        };
        other_spans.push(Span::styled(cb, Style::default().fg(TEXT_DIM)));
    }
    if other_selected {
        // Focused Other shows label + live buffer + blinking caret.
        other_spans.push(Span::styled(
            OTHER_LABEL,
            Style::default()
                .fg(other_color)
                .add_modifier(Modifier::ITALIC),
        ));
        if !other_buf.is_empty() {
            other_spans.push(Span::styled("  ", Style::default()));
            other_spans.push(Span::styled(
                other_buf.to_string(),
                Style::default().fg(TEXT),
            ));
        }
        other_spans.push(Span::styled(
            "_",
            Style::default()
                .fg(TEXT_DIM)
                .add_modifier(Modifier::SLOW_BLINK),
        ));
    } else {
        // Unfocused Other: show "Other: typed text" when multi has a buffer.
        let suffix = if multi && !other_buf.is_empty() {
            format!(": {other_buf}")
        } else {
            String::new()
        };
        other_spans.push(Span::styled(
            format!("{OTHER_LABEL}{suffix}"),
            Style::default()
                .fg(other_color)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    lines.push(Line::from(other_spans));

    // Footer
    lines.extend(footer_lines(state));
}

/// Left-pane lines for the SPLIT layout (when at least one option has a
/// `preview`). Descriptions move to the right pane to keep the option list
/// compact — the focused option's full detail shows there.
pub(crate) fn options_pane_lines(state: &InlinePickerState) -> Vec<Line<'static>> {
    let q = state.current_question();
    let mut out: Vec<Line<'static>> = Vec::new();
    let opts_n = q.options.len();
    let cursor = state.cursor();
    let multi = state.is_multi();
    let max_number_width = (opts_n + 1).to_string().len();
    for (idx, opt) in q.options.iter().enumerate() {
        let selected = idx == cursor;
        let (clean_label, recommended) = split_recommended(&opt.label);
        let cursor_glyph = if selected { "> " } else { "  " };
        let number_str = format!("{:>w$}. ", idx + 1, w = max_number_width);
        let title_color = if selected { ACCENT } else { TEXT };
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(cursor_glyph, Style::default().fg(ACCENT)),
            Span::styled(number_str, Style::default().fg(TEXT_DIM)),
        ];
        if multi {
            let cb = if state.is_toggled(idx) {
                "[x] "
            } else {
                "[ ] "
            };
            spans.push(Span::styled(cb, Style::default().fg(TEXT_DIM)));
        }
        spans.push(Span::styled(
            clean_label,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ));
        if recommended {
            spans.push(Span::styled("  ", Style::default()));
            spans.extend(recommended_badge());
        }
        out.push(Line::from(spans));
    }
    // Separator + Other row (single-line, no description in split mode either).
    out.push(Line::from(Span::styled(
        "─".repeat(30),
        Style::default().fg(BORDER_DIM),
    )));
    let other_selected = cursor == opts_n;
    let other_cursor = if other_selected { "> " } else { "  " };
    let other_number = format!("{:>w$}. ", opts_n + 1, w = max_number_width);
    let other_color = if other_selected { ACCENT } else { TEXT };
    let other_buf = state.other_buf();
    let mut other_spans: Vec<Span<'static>> = vec![
        Span::styled(other_cursor, Style::default().fg(ACCENT)),
        Span::styled(other_number, Style::default().fg(TEXT_DIM)),
    ];
    if multi {
        let cb = if state.other_included() {
            "[x] "
        } else {
            "[ ] "
        };
        other_spans.push(Span::styled(cb, Style::default().fg(TEXT_DIM)));
    }
    if other_selected {
        other_spans.push(Span::styled(
            OTHER_LABEL,
            Style::default()
                .fg(other_color)
                .add_modifier(Modifier::ITALIC),
        ));
        if !other_buf.is_empty() {
            other_spans.push(Span::styled("  ", Style::default()));
            other_spans.push(Span::styled(
                other_buf.to_string(),
                Style::default().fg(TEXT),
            ));
        }
        other_spans.push(Span::styled(
            "_",
            Style::default()
                .fg(TEXT_DIM)
                .add_modifier(Modifier::SLOW_BLINK),
        ));
    } else {
        let suffix = if multi && !other_buf.is_empty() {
            format!(": {other_buf}")
        } else {
            String::new()
        };
        other_spans.push(Span::styled(
            format!("{OTHER_LABEL}{suffix}"),
            Style::default()
                .fg(other_color)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    out.push(Line::from(other_spans));
    out
}

/// Right-pane lines for the SPLIT layout — focused option's full detail
/// (title + Recommended badge + description + preview text). When "Other" is
/// focused, the pane shows a brief hint instead.
pub(crate) fn preview_pane_lines(state: &InlinePickerState) -> Vec<Line<'static>> {
    let q = state.current_question();
    let cursor = state.cursor();
    let mut out: Vec<Line<'static>> = Vec::new();
    let Some(opt) = q.options.get(cursor) else {
        // Other focused — no per-option detail to show.
        out.push(Line::from(Span::styled(
            "(type custom answer below)",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
        )));
        return out;
    };
    let (clean_label, recommended) = split_recommended(&opt.label);
    // Title row (mirrors the cursor styling so the pane reads as "you're
    // looking at this option").
    let mut header: Vec<Span<'static>> = vec![Span::styled(
        clean_label,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    if recommended {
        header.push(Span::styled("  ", Style::default()));
        header.extend(recommended_badge());
    }
    out.push(Line::from(header));
    if !opt.description.is_empty() {
        out.push(Line::from(Span::styled(
            opt.description.clone(),
            Style::default().fg(TEXT_DIM),
        )));
    }
    if let Some(preview) = &opt.preview {
        out.push(Line::from(""));
        for line in preview.lines() {
            out.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(TEXT_DIM),
            )));
        }
    }
    out
}

/// Strip a trailing " (Recommended)" suffix (case-insensitive, ignoring trailing
/// whitespace) from a label and return whether the original carried it. The
/// agent encodes recommendation via this label convention; we render the chip.
fn split_recommended(label: &str) -> (String, bool) {
    let trimmed = label.trim_end();
    if let Some(stripped) = trimmed.strip_suffix(')') {
        // Match "(Recommended)" case-insensitively at the end.
        if let Some(open) = stripped.rfind('(') {
            let tag = &stripped[open + 1..];
            if tag.eq_ignore_ascii_case("Recommended") {
                return (stripped[..open].trim_end().to_string(), true);
            }
        }
    }
    (label.to_string(), false)
}

/// The `┊ Recommended ┊` dim-color chip rendered after the title for the
/// option the agent marked.
fn recommended_badge() -> Vec<Span<'static>> {
    vec![
        Span::styled("┊ ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            "Recommended",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
        ),
        Span::styled(" ┊", Style::default().fg(TEXT_DIM)),
    ]
}

/// Height (rows) the picker needs given its current state. Used by
/// `render::live_height` so the inline viewport recreates at the right size.
pub(crate) fn picker_height(state: &InlinePickerState) -> u16 {
    let q = state.current_question();
    let header_rows: u16 = 3; // blank + header + question
    let footer_rows: u16 = 2; // blank + footer
    if state.has_any_preview() {
        // Split layout: max(left pane, right pane + border) + header + footer.
        // Left pane = N options (1 row each) + separator + Other = N + 2.
        let left = q.options.len() as u16 + 2;
        // Right pane is ~55% of the inline viewport width, minus the border
        // padding. At a conservative 80-col terminal that's ≈40 usable cols,
        // so any long description / preview line wraps to extra rows. Estimate
        // wrap rows per line at the narrow-terminal bound (40 cols) so the
        // live band stays generous enough to render without clipping.
        const PANE_USABLE_COLS: usize = 40;
        let wrapped = |s: &str| -> u16 {
            s.lines()
                .map(|line| {
                    let cols = line.chars().count();
                    1u16.max((cols as u16).div_ceil(PANE_USABLE_COLS as u16))
                })
                .sum()
        };
        let max_right_body = q
            .options
            .iter()
            .map(|o| {
                // 1 title row (may wrap), optional desc (may wrap),
                // optional preview block (blank + lines, each may wrap).
                let mut rows: u16 = wrapped(&o.label);
                if !o.description.is_empty() {
                    rows += wrapped(&o.description);
                }
                if let Some(p) = &o.preview {
                    rows += 1 /*blank*/ + wrapped(p);
                }
                rows
            })
            .max()
            .unwrap_or(1);
        let right = max_right_body + 2 /*border*/;
        header_rows + left.max(right) + footer_rows
    } else {
        // Single-column layout: options (2 rows each if desc, 1 if not) +
        // separator + Other.
        let option_rows: u16 = q
            .options
            .iter()
            .map(|o| if o.description.is_empty() { 1 } else { 2 })
            .sum::<usize>() as u16;
        let separator: u16 = 1;
        let other_row: u16 = 1;
        header_rows + option_rows + separator + other_row + footer_rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::picker::{PickerOption, PickerQuestion};

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
            allow_other: true,
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

    #[test]
    fn split_recommended_strips_suffix() {
        assert_eq!(
            super::split_recommended("serde_json (Recommended)"),
            ("serde_json".to_string(), true)
        );
        // Case-insensitive, tolerant of trailing whitespace.
        assert_eq!(
            super::split_recommended("foo (recommended)  "),
            ("foo".to_string(), true)
        );
    }

    #[test]
    fn split_recommended_leaves_other_parens_alone() {
        assert_eq!(
            super::split_recommended("foo (beta)"),
            ("foo (beta)".to_string(), false)
        );
        assert_eq!(
            super::split_recommended("plain"),
            ("plain".to_string(), false)
        );
    }
}
