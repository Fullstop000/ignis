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
    /// True once every question has been answered and we're showing the
    /// review-and-submit screen (multi-question batches only). Enter submits;
    /// Left/Shift-Tab steps back into the last question to revise it.
    reviewing: bool,
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
            reviewing: false,
            reply: Some(request.reply),
        }
    }

    /// True when the current question is a plain text-input field (used by
    /// `/connect` for API-key entry). In this mode there are no options to
    /// navigate — every printable key extends the buffer, Enter submits.
    pub(crate) fn is_text_input(&self) -> bool {
        self.current_question().text_input
    }
    /// True when the current text-input question wants its content masked
    /// (`●` glyphs). Only meaningful when `is_text_input()` is true.
    pub(crate) fn is_masked(&self) -> bool {
        self.current_question().mask
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
    /// True when the review-and-submit screen is showing (all questions
    /// answered). The renderer switches to `render_review` in this state.
    pub(crate) fn is_reviewing(&self) -> bool {
        self.reviewing
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
        // Ctrl-D also cancels. The inline picker pre-empts the TUI's global
        // Ctrl-D handler (which exits ignis), so without this branch the 'd'
        // ends up swallowed into the picker — particularly visible on the
        // text-input API-key step, where it would appear as a stray masked
        // glyph. Cancel + second-Ctrl-D-to-quit matches the shell convention.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('d')) {
            return KeyOutcome::Cancel;
        }
        // Defensive: every Ctrl-modified Char goes through the global TUI
        // handlers in the same key path (Ctrl-A/E/J/U/W for editing,
        // Ctrl-S for steer, etc.). Pickers ignore them so users don't see
        // accidental letters leak into the picker buffer.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char(_)) {
            return KeyOutcome::Continue;
        }
        // Review-and-submit screen (multi-question batches, all answered).
        // Enter submits the whole batch; Left/Shift-Tab steps back into the
        // last question to revise it (other answers are preserved).
        if self.reviewing {
            return match key.code {
                KeyCode::Enter => KeyOutcome::Done(std::mem::take(&mut self.answers)),
                KeyCode::Left | KeyCode::BackTab => self.go_back(),
                _ => KeyOutcome::Continue,
            };
        }
        // Text-input mode (e.g. `/connect`'s API-key step): no options to
        // navigate. Every printable key extends the buffer, Backspace shrinks
        // it, Enter submits whatever's in the buffer. Length cap matches the
        // Other-row cap so behaviour is consistent.
        if self.is_text_input() {
            return match key.code {
                KeyCode::Enter => self.try_advance(),
                KeyCode::Left | KeyCode::BackTab => self.go_back(),
                KeyCode::Backspace => {
                    self.other_buf.pop();
                    KeyOutcome::Continue
                }
                KeyCode::Char(c) => {
                    if !c.is_control() && self.other_buf.len() + c.len_utf8() <= MAX_OTHER_LEN {
                        self.other_buf.push(c);
                    }
                    KeyOutcome::Continue
                }
                _ => KeyOutcome::Continue,
            };
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
            // Left / Shift-Tab steps back to the previous question (or out of
            // review), rehydrating its prior answer for revision.
            KeyCode::Left | KeyCode::BackTab => self.go_back(),
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

    /// Try to commit the current question's selection. Returns `Continue` if
    /// the user hasn't picked anything yet (no-op); otherwise delegates to
    /// `commit`, which advances, enters review, or finishes.
    fn try_advance(&mut self) -> KeyOutcome {
        let q = self.current_question();
        let pick: Option<PickerAnswer> = if q.text_input {
            // Text-input questions are always single-answer. Empty submits are
            // rejected so the user can't accidentally save a blank API key.
            let text = self.other_buf.trim();
            (!text.is_empty()).then(|| PickerAnswer::Single(text.to_string()))
        } else if q.multi_select {
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

        match pick {
            Some(answer) => self.commit(answer),
            None => KeyOutcome::Continue,
        }
    }

    /// Record `answer` for the current question, then move forward: to the
    /// next question, to the review screen (last question of a multi-question
    /// batch), or to `Done` (last question of a single-question batch). When
    /// revisiting an already-answered question the answer is overwritten in
    /// place so the other answers are preserved.
    fn commit(&mut self, answer: PickerAnswer) -> KeyOutcome {
        if self.current < self.answers.len() {
            self.answers[self.current] = answer;
        } else {
            self.answers.push(answer);
        }
        if self.current + 1 >= self.questions.len() {
            // Last question answered. A single-question batch returns
            // immediately (a review screen for one answer is pure friction);
            // multi-question batches show the review-and-submit step.
            if self.questions.len() > 1 {
                self.reviewing = true;
                return KeyOutcome::Continue;
            }
            return KeyOutcome::Done(std::mem::take(&mut self.answers));
        }
        self.current += 1;
        self.load_question_state();
        KeyOutcome::Continue
    }

    /// Step back one screen: out of review into the last question, or to the
    /// previous question. The target question's prior answer is rehydrated so
    /// the user revises rather than re-enters. No-op at the first question.
    fn go_back(&mut self) -> KeyOutcome {
        if self.reviewing {
            // `current` is still the last question — just leave review and
            // reload it with its committed answer.
            self.reviewing = false;
            self.load_question_state();
            return KeyOutcome::Continue;
        }
        if self.current == 0 {
            return KeyOutcome::Continue;
        }
        self.current -= 1;
        self.load_question_state();
        KeyOutcome::Continue
    }

    /// Reset the per-question editing state (cursor / toggles / Other buffer)
    /// for `self.current`, then rehydrate it from a previously committed
    /// answer if one exists (i.e. we're revisiting, not answering fresh).
    fn load_question_state(&mut self) {
        let opts_n = self.current_question().options.len();
        self.cursor = 0;
        self.toggled = vec![false; opts_n];
        self.other_buf.clear();
        if self.current < self.answers.len() {
            let prev = self.answers[self.current].clone();
            self.rehydrate_into(prev);
        }
    }

    /// Restore the cursor / toggles / Other buffer to reflect `prev` for the
    /// current question, so a revisited question shows the user's last choice.
    fn rehydrate_into(&mut self, prev: PickerAnswer) {
        let q = &self.questions[self.current];
        match prev {
            PickerAnswer::Single(val) => {
                if q.text_input {
                    self.other_buf = val;
                } else if let Some(i) = q.options.iter().position(|o| o.label == val) {
                    self.cursor = i;
                } else if q.allow_other {
                    // The prior answer was free-text — restore it into Other.
                    self.other_buf = val;
                    self.cursor = q.options.len();
                }
            }
            PickerAnswer::Multi(vals) => {
                for v in vals {
                    if let Some(i) = q.options.iter().position(|o| o.label == v) {
                        self.toggled[i] = true;
                    } else {
                        self.other_buf = v;
                    }
                }
            }
        }
    }
}

// ---------- renderer ----------

/// Horizontal separator that spans the full render width. Used above the
/// picker (to break it off from committed scrollback) and between the regular
/// options and the Other row. Caller passes the area width so the divider
/// resizes with the terminal — a fixed length would wrap on narrow terminals
/// and leave a stub on wide ones.
fn divider_line(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(BORDER_DIM),
    ))
}

/// Divider + blank + header strip + question. Used by both layouts (single-
/// column and split-with-preview) so they share an identical top section. The
/// leading divider is what visually breaks the picker off from the previous
/// scrollback line — without it the `◆ Question` row sits flush against
/// whatever rendered last.
pub(crate) fn header_lines(state: &InlinePickerState, width: u16) -> Vec<Line<'static>> {
    let q = state.current_question();
    let progress = if state.total() > 1 {
        format!(" · {}/{}", state.current_index() + 1, state.total())
    } else {
        String::new()
    };
    vec![
        divider_line(width),
        Line::from(""),
        Line::from(vec![
            Span::styled("◆ ", Style::default().fg(MAUVE)),
            Span::styled(
                q.kind.clone(),
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
    let footer = if state.is_text_input() {
        "type · ↵ confirm · esc cancel"
    } else if state.other_focused() {
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

/// Review-and-submit screen for a multi-question batch: every question with
/// its committed answer, a Submit affordance, and a footer advertising the
/// revise / submit / cancel keys. Shown when `state.is_reviewing()`.
pub(crate) fn render_review(lines: &mut Vec<Line<'static>>, state: &InlinePickerState, width: u16) {
    let n = state.questions.len();
    let kind = state.questions[state.current].kind.clone();
    lines.push(divider_line(width));
    lines.push(Line::from(""));
    // Header strip mirrors the question header but reads `· review · N/N`.
    lines.push(Line::from(vec![
        Span::styled("◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            kind,
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(TEXT_DIM)),
        Span::styled(
            "review",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {n}/{n}"), Style::default().fg(TEXT_DIM)),
    ]));
    lines.push(Line::from(Span::styled(
        "Confirm your answers:",
        Style::default().fg(TEXT),
    )));
    lines.push(Line::from(""));

    // `  N. <header>   <answer>` with the header column padded so answers line
    // up; multi-select answers stack under the first, each prefixed with ✓.
    let num_w = n.to_string().len();
    let header_w = state
        .questions
        .iter()
        .map(|q| q.header.chars().count())
        .max()
        .unwrap_or(0);
    for (i, q) in state.questions.iter().enumerate() {
        let prefix = format!(
            "  {:>num_w$}. {:<header_w$}   ",
            i + 1,
            q.header,
            num_w = num_w,
            header_w = header_w
        );
        let indent = " ".repeat(prefix.chars().count());
        match state.answers.get(i) {
            Some(PickerAnswer::Single(v)) => {
                let shown = if q.text_input {
                    format!("\"{v}\"")
                } else {
                    v.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(TEXT_DIM)),
                    Span::styled(shown, Style::default().fg(TEXT)),
                ]));
            }
            Some(PickerAnswer::Multi(vs)) => {
                for (j, v) in vs.iter().enumerate() {
                    let lead = if j == 0 {
                        prefix.clone()
                    } else {
                        indent.clone()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(lead, Style::default().fg(TEXT_DIM)),
                        Span::styled(format!("✓ {v}"), Style::default().fg(TEXT)),
                    ]));
                }
            }
            None => {}
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [ Submit ]",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "← revisit · ↵ submit · esc cancel",
        Style::default().fg(TEXT_DIM),
    )));
}

/// One-line input row for text-input mode. Renders the buffer (masked when
/// the question opts in) plus a blinking caret. Shared by both render paths
/// — single-column and split — though text-input never triggers the split.
fn text_input_row(state: &InlinePickerState) -> Line<'static> {
    let raw = state.other_buf();
    let display: String = if state.is_masked() {
        "●".repeat(raw.chars().count())
    } else {
        raw.to_string()
    };
    Line::from(vec![
        Span::styled("> ", Style::default().fg(ACCENT)),
        Span::styled(display, Style::default().fg(TEXT)),
        Span::styled(
            "_",
            Style::default()
                .fg(TEXT_DIM)
                .add_modifier(Modifier::SLOW_BLINK),
        ),
    ])
}

/// Lines for the picker band (used by render::draw when inline_picker is open).
/// `max_rows` is the total rows available to the picker (the area height).
/// We reserve a fixed budget for header + Other + footer chrome and window
/// the options list inside what's left so the cursor stays visible.
pub(crate) fn render_inline_picker(
    lines: &mut Vec<Line<'static>>,
    state: &InlinePickerState,
    width: u16,
    max_rows: usize,
) {
    let q = state.current_question();
    lines.extend(header_lines(state, width));

    // Text-input mode short-circuits: no options, just the input row + footer.
    if state.is_text_input() {
        lines.push(text_input_row(state));
        lines.extend(footer_lines(state));
        return;
    }

    // Options (CC-style stacked: title row + description row indented).
    // Everything sits flush left to match CC's reference layout — no extra
    // gutter that ratatui's wrap pipeline sometimes collapses inconsistently.
    let opts_n = q.options.len();
    let cursor = state.cursor();
    let multi = state.is_multi();
    let max_number_width = (opts_n + 1).to_string().len(); // "10" → 2, "9" → 1
    let desc_indent = 2 /*cursor col*/ + max_number_width + 2 /*". "*/;
    let desc_indent_str = " ".repeat(desc_indent);

    // Budget for the option list. Header is 4 rows (divider+blank+title+
    // question); footer is 2 (blank+hint); the ↑/↓ hint markers eat 2 more
    // when the window is non-zero. Divider+Other adds another 2 — but only
    // when the question opts into the Other row (permission/AFK pickers do
    // not). Min 1 keeps at least one option visible on tiny terminals.
    let header_rows: usize = 4;
    let footer_rows: usize = 2;
    let hint_slack: usize = 2;
    let other_rows: usize = if q.allow_other { 2 } else { 0 };
    let chrome = header_rows + footer_rows + hint_slack + other_rows;
    let opts_budget = max_rows.saturating_sub(chrome).max(1);
    // Each option takes 2 rows when it has a description, else 1.
    // Window over the option *list* (not row count) keeping cursor in view —
    // the row-count slop is small enough to ignore in practice.
    let (opts_start, opts_end) =
        crate::console::render::widgets::picker_window(cursor.min(opts_n), opts_budget, opts_n);
    if opts_start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {} more above", opts_start),
            Style::default().fg(TEXT_DIM),
        )));
    }

    for (offset, opt) in q.options[opts_start..opts_end].iter().enumerate() {
        let idx = opts_start + offset;
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
    let opts_below = opts_n.saturating_sub(opts_end);
    if opts_below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more below", opts_below),
            Style::default().fg(TEXT_DIM),
        )));
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
    lines.push(divider_line(width));

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
        // Focused Other: dim placeholder when empty, typed buffer once the
        // user starts typing — never both at once. Caret always trails.
        if other_buf.is_empty() {
            other_spans.push(Span::styled(
                OTHER_LABEL,
                Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
            ));
        } else {
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
/// compact — the focused option's full detail shows there. `max_rows` is
/// the pane height; options window around the cursor so it stays visible
/// when the list is taller than the pane.
pub(crate) fn options_pane_lines(
    state: &InlinePickerState,
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let q = state.current_question();
    let mut out: Vec<Line<'static>> = Vec::new();
    let opts_n = q.options.len();
    let cursor = state.cursor();
    let multi = state.is_multi();
    let max_number_width = (opts_n + 1).to_string().len();
    // Reserve: divider + Other row + 2-line ↑/↓ slack.
    const CHROME_RESERVE: usize = 1 + 1 + 2;
    let opts_budget = max_rows.saturating_sub(CHROME_RESERVE).max(1);
    let (start, end) =
        crate::console::render::widgets::picker_window(cursor.min(opts_n), opts_budget, opts_n);
    if start > 0 {
        out.push(Line::from(Span::styled(
            format!("  ↑ {} more above", start),
            Style::default().fg(TEXT_DIM),
        )));
    }
    for (offset, opt) in q.options[start..end].iter().enumerate() {
        let idx = start + offset;
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
    let below = opts_n.saturating_sub(end);
    if below > 0 {
        out.push(Line::from(Span::styled(
            format!("  ↓ {} more below", below),
            Style::default().fg(TEXT_DIM),
        )));
    }
    // Separator + Other row (single-line, no description in split mode either).
    out.push(divider_line(width));
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
        // Focused Other: dim placeholder when empty, typed buffer once the
        // user starts typing — never both at once. Caret always trails.
        if other_buf.is_empty() {
            other_spans.push(Span::styled(
                OTHER_LABEL,
                Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
            ));
        } else {
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

/// Height (rows) the picker wants at the given render width, ignoring any
/// cap. Used by the renderer to anchor the picker to the bottom of the body
/// area: the picker reserves only as many rows as it actually needs, leaving
/// the transcript visible above. `width` is the viewport width — needed so
/// option descriptions that wrap onto multiple display rows are budgeted
/// correctly. Passing the body width prevents the footer hint from clipping.
pub(crate) fn picker_height(state: &InlinePickerState, width: u16) -> u16 {
    // Review screen: divider + blank + header + "Confirm" + blank (5) + one
    // row per answer (multi-select answers stack) + blank + Submit + blank +
    // footer (4).
    if state.is_reviewing() {
        let answer_rows: u16 = state
            .answers
            .iter()
            .map(|a| match a {
                PickerAnswer::Multi(v) => v.len().max(1) as u16,
                PickerAnswer::Single(_) => 1,
            })
            .sum();
        return 5 + answer_rows + 4;
    }
    let q = state.current_question();
    let header_rows: u16 = 4; // divider + blank + header + question
    let footer_rows: u16 = 2; // blank + footer
    if state.is_text_input() {
        // Header + one input row + footer. No options, no separator.
        return header_rows + 1 + footer_rows;
    }
    let wrap_at = |cols: u16, s: &str| -> u16 {
        let cols = cols.max(1) as usize;
        s.lines()
            .map(|line| {
                let n = line.chars().count();
                1u16.max(n.div_ceil(cols) as u16)
            })
            .sum()
    };
    if state.has_any_preview() {
        // Split layout: max(left pane, right pane + border) + header + footer.
        // Left pane = N options (1 row each) + separator + Other = N + 2.
        let left = q.options.len() as u16 + 2;
        // Right pane is ~55% of the viewport width, minus the border padding.
        // Use the actual width so this scales with the terminal instead of
        // baking in a 40-col assumption that under-budgets wide terminals.
        let right_cols = ((width as u32) * 55 / 100).saturating_sub(2).max(20) as u16;
        let max_right_body = q
            .options
            .iter()
            .map(|o| {
                let mut rows: u16 = wrap_at(right_cols, &o.label);
                if !o.description.is_empty() {
                    rows += wrap_at(right_cols, &o.description);
                }
                if let Some(p) = &o.preview {
                    rows += 1 /*blank*/ + wrap_at(right_cols, p);
                }
                rows
            })
            .max()
            .unwrap_or(1);
        let right = max_right_body + 2 /*border*/;
        header_rows + left.max(right) + footer_rows
    } else {
        // Single-column layout. Titles render on one line each. Descriptions
        // are indented under the number ("  N. ") so their effective wrap
        // width is `width - desc_indent` — using the full width here used to
        // undercount by 1 row whenever the description sat near the right
        // margin and pushed the footer hint off-screen.
        let opts_n = q.options.len();
        let max_number_width = (opts_n + 1).to_string().len() as u16;
        let desc_indent = 2 /*cursor col*/ + max_number_width + 2 /*". "*/;
        let desc_width = width.saturating_sub(desc_indent).max(1);
        let option_rows: u16 = q
            .options
            .iter()
            .map(|o| {
                let title = 1u16;
                let desc = if o.description.is_empty() {
                    0
                } else {
                    wrap_at(desc_width, &o.description)
                };
                title + desc
            })
            .sum();
        // Separator + Other row only render when the picker opts in (permission
        // and AFK skip both — closed-by-design option sets).
        let separator: u16 = if q.allow_other { 1 } else { 0 };
        let other_row: u16 = if q.allow_other { 1 } else { 0 };
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
            kind: "ask_user".to_string(),
            header: header.to_string(),
            multi_select: multi,
            allow_other: true,
            text_input: false,
            mask: false,
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
    fn two_question_flow_advances_then_reviews_before_done() {
        let (mut s, _rx) = make_request(vec![
            q("Q1?", "h1", false, &["x", "y"]),
            q("Q2?", "h2", false, &["p", "q"]),
        ]);
        assert_eq!(s.current_index(), 0);
        let _ = s.on_key(key(KeyCode::Enter)); // pick "x"
        assert_eq!(s.current_index(), 1);
        assert_eq!(s.cursor(), 0); // reset
        let _ = s.on_key(key(KeyCode::Down)); // move to "q"
                                              // Enter on the LAST question opens review, not Done.
        assert!(matches!(
            s.on_key(key(KeyCode::Enter)),
            KeyOutcome::Continue
        ));
        assert!(s.is_reviewing());
        // Enter on review submits the whole batch.
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
    fn single_question_skips_review_and_returns_done() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        match s.on_key(key(KeyCode::Enter)) {
            KeyOutcome::Done(ans) => assert_eq!(ans, vec![PickerAnswer::Single("a".to_string())]),
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(!s.is_reviewing());
    }

    #[test]
    fn back_from_review_reopens_last_question_with_prior_answer() {
        let (mut s, _rx) = make_request(vec![
            q("Q1?", "h1", false, &["x", "y"]),
            q("Q2?", "h2", false, &["p", "q"]),
        ]);
        let _ = s.on_key(key(KeyCode::Enter)); // Q1 -> x
        let _ = s.on_key(key(KeyCode::Down)); // Q2 cursor -> q
        let _ = s.on_key(key(KeyCode::Enter)); // -> review
        assert!(s.is_reviewing());
        let _ = s.on_key(key(KeyCode::Left)); // back into Q2
        assert!(!s.is_reviewing());
        assert_eq!(s.current_index(), 1);
        assert_eq!(s.cursor(), 1, "prior answer 'q' rehydrated");
    }

    #[test]
    fn back_nav_preserves_other_answers_and_allows_edit() {
        let (mut s, _rx) = make_request(vec![
            q("Q1?", "h1", false, &["a", "b"]),
            q("Q2?", "h2", false, &["c", "d"]),
            q("Q3?", "h3", false, &["e", "f"]),
        ]);
        let _ = s.on_key(key(KeyCode::Enter)); // Q1 -> a
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Enter)); // Q2 -> d
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Enter)); // Q3 -> f -> review
        assert!(s.is_reviewing());
        // Walk all the way back to Q1, prior answers rehydrated each step.
        let _ = s.on_key(key(KeyCode::Left)); // -> Q3 (f)
        assert_eq!((s.current_index(), s.cursor()), (2, 1));
        let _ = s.on_key(key(KeyCode::Left)); // -> Q2 (d)
        assert_eq!((s.current_index(), s.cursor()), (1, 1));
        let _ = s.on_key(key(KeyCode::Left)); // -> Q1 (a)
        assert_eq!((s.current_index(), s.cursor()), (0, 0));
        // Change Q1 a -> b, then go forward; Q2/Q3 answers must be preserved.
        let _ = s.on_key(key(KeyCode::Down)); // cursor -> b
        let _ = s.on_key(key(KeyCode::Enter)); // commit b -> Q2 (rehydrated d)
        assert_eq!((s.current_index(), s.cursor()), (1, 1));
        let _ = s.on_key(key(KeyCode::Enter)); // keep d -> Q3 (rehydrated f)
        assert_eq!((s.current_index(), s.cursor()), (2, 1));
        let _ = s.on_key(key(KeyCode::Enter)); // keep f -> review
        assert!(s.is_reviewing());
        match s.on_key(key(KeyCode::Enter)) {
            KeyOutcome::Done(ans) => assert_eq!(
                ans,
                vec![
                    PickerAnswer::Single("b".to_string()),
                    PickerAnswer::Single("d".to_string()),
                    PickerAnswer::Single("f".to_string()),
                ]
            ),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn back_nav_rehydrates_multiselect_toggles() {
        let (mut s, _rx) = make_request(vec![
            q("Q1?", "h1", true, &["a", "b", "c"]),
            q("Q2?", "h2", false, &["x", "y"]),
        ]);
        let _ = s.on_key(key(KeyCode::Char(' '))); // toggle a (cursor 0)
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down)); // cursor -> c
        let _ = s.on_key(key(KeyCode::Char(' '))); // toggle c
        let _ = s.on_key(key(KeyCode::Enter)); // commit [a,c] -> Q2
        let _ = s.on_key(key(KeyCode::Enter)); // Q2 -> x -> review
        let _ = s.on_key(key(KeyCode::Left)); // -> Q2
        let _ = s.on_key(key(KeyCode::Left)); // -> Q1, toggles rehydrated
        assert_eq!(s.current_index(), 0);
        assert!(s.is_toggled(0));
        assert!(!s.is_toggled(1));
        assert!(s.is_toggled(2));
    }

    #[test]
    fn render_review_lists_questions_and_answers() {
        let (mut s, _rx) = make_request(vec![
            q("Q1?", "Database", false, &["postgres", "mysql"]),
            q("Q2?", "ORM", false, &["sqlx", "diesel"]),
        ]);
        let _ = s.on_key(key(KeyCode::Enter)); // Q1 -> postgres
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Enter)); // Q2 -> diesel -> review
        assert!(s.is_reviewing());
        let mut lines: Vec<Line> = Vec::new();
        render_review(&mut lines, &s, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|sp| sp.content.as_ref())
            .collect();
        for needle in ["review", "Database", "postgres", "ORM", "diesel", "Submit"] {
            assert!(text.contains(needle), "review missing {needle:?}: {text}");
        }
    }

    #[test]
    fn back_nav_rehydrates_other_free_text() {
        let (mut s, _rx) = make_request(vec![
            q("Q1?", "h1", false, &["a", "b"]),
            q("Q2?", "h2", false, &["x", "y"]),
        ]);
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down)); // cursor -> Other row
        for c in "custom".chars() {
            let _ = s.on_key(key(KeyCode::Char(c)));
        }
        let _ = s.on_key(key(KeyCode::Enter)); // commit Single("custom") -> Q2
        let _ = s.on_key(key(KeyCode::Enter)); // Q2 -> x -> review
        let _ = s.on_key(key(KeyCode::Left)); // -> Q2
        let _ = s.on_key(key(KeyCode::Left)); // -> Q1
        assert_eq!(s.current_index(), 0);
        assert!(s.other_focused(), "cursor returned to Other row");
        assert_eq!(s.other_buf(), "custom");
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

    fn text_input_q(question: &str, header: &str, mask: bool) -> PickerQuestion {
        PickerQuestion {
            question: question.to_string(),
            kind: "connect".to_string(),
            header: header.to_string(),
            multi_select: false,
            allow_other: false,
            text_input: true,
            mask,
            options: vec![],
        }
    }

    #[test]
    fn ctrl_d_cancels_inline_picker() {
        // Pre-empts the global Ctrl-D handler, so without this branch 'd'
        // leaks into the buffer (caught by /connect dogfood as stray ●●).
        let (mut s, _rx) = make_request(vec![text_input_q("API key", "API Key", true)]);
        let ev = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(s.on_key(ev), KeyOutcome::Cancel));
        // Buffer stayed empty — 'd' did NOT get pushed.
        assert_eq!(s.other_buf(), "");
    }

    #[test]
    fn ctrl_modified_chars_dont_leak_into_text_input_buffer() {
        // Ctrl-Anything (other than C/D handled above) is a no-op for the
        // picker — the global TUI handlers own those bindings.
        let (mut s, _rx) = make_request(vec![text_input_q("API key", "API Key", true)]);
        for ch in ['a', 'e', 'j', 'u', 'w', 's'] {
            let ev = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
            let _ = s.on_key(ev);
        }
        assert_eq!(
            s.other_buf(),
            "",
            "Ctrl-modified chars must not push into the picker buffer"
        );
    }

    #[test]
    fn text_input_typing_extends_buffer_enter_returns_done() {
        let (mut s, _rx) = make_request(vec![text_input_q("API key", "API Key", true)]);
        for c in "sk-abc".chars() {
            let _ = s.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(s.other_buf(), "sk-abc");
        match s.on_key(key(KeyCode::Enter)) {
            KeyOutcome::Done(ans) => {
                assert_eq!(ans[0], PickerAnswer::Single("sk-abc".to_string()));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn text_input_empty_submit_is_noop() {
        let (mut s, _rx) = make_request(vec![text_input_q("API key", "API Key", true)]);
        // No keys typed → Enter does nothing (avoids saving a blank API key).
        assert!(matches!(
            s.on_key(key(KeyCode::Enter)),
            KeyOutcome::Continue
        ));
        // Pure whitespace also counts as empty.
        for _ in 0..3 {
            let _ = s.on_key(key(KeyCode::Char(' ')));
        }
        assert!(matches!(
            s.on_key(key(KeyCode::Enter)),
            KeyOutcome::Continue
        ));
    }

    #[test]
    fn text_input_backspace_shrinks_buffer() {
        let (mut s, _rx) = make_request(vec![text_input_q("API key", "API Key", false)]);
        for c in "abc".chars() {
            let _ = s.on_key(key(KeyCode::Char(c)));
        }
        let _ = s.on_key(key(KeyCode::Backspace));
        assert_eq!(s.other_buf(), "ab");
    }

    #[test]
    fn text_input_esc_cancels() {
        let (mut s, _rx) = make_request(vec![text_input_q("API key", "API Key", true)]);
        let _ = s.on_key(key(KeyCode::Char('x')));
        assert!(matches!(s.on_key(key(KeyCode::Esc)), KeyOutcome::Cancel));
    }

    #[test]
    fn text_input_height_is_smaller_than_options_picker() {
        let (s_text, _rx) = make_request(vec![text_input_q("API key", "API Key", true)]);
        let (s_opts, _rx2) = make_request(vec![q("Pick", "h", false, &["a", "b", "c"])]);
        assert!(picker_height(&s_text, 80) < picker_height(&s_opts, 80));
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

    /// Concatenate the literal text of a `Line`'s spans (drops styling) for
    /// content assertions in render-shape tests.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn picker_header_starts_with_full_width_divider() {
        let (s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        let width: u16 = 80;
        let lines = super::header_lines(&s, width);
        assert_eq!(line_text(&lines[0]), "─".repeat(width as usize));
        // A second render at a different width must follow the new width —
        // i.e. the divider resizes with the terminal.
        let narrow = super::header_lines(&s, 30);
        assert_eq!(line_text(&narrow[0]), "─".repeat(30));
    }

    #[test]
    fn focused_other_shows_placeholder_when_buffer_empty() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        // Move cursor to Other (last row).
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down));
        assert!(s.other_focused());
        assert!(s.other_buf().is_empty());

        let mut lines: Vec<Line<'static>> = Vec::new();
        super::render_inline_picker(&mut lines, &s, 80, 50);
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains(super::OTHER_LABEL),
            "placeholder must show while Other is focused with an empty buffer"
        );
    }

    #[test]
    fn focused_other_replaces_placeholder_when_typing() {
        let (mut s, _rx) = make_request(vec![q("Q?", "h", false, &["a", "b"])]);
        let _ = s.on_key(key(KeyCode::Down));
        let _ = s.on_key(key(KeyCode::Down));
        for c in "hello".chars() {
            let _ = s.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(s.other_buf(), "hello");

        let mut lines: Vec<Line<'static>> = Vec::new();
        super::render_inline_picker(&mut lines, &s, 80, 50);
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("hello"),
            "typed text must render on the Other row"
        );
        assert!(
            !joined.contains(super::OTHER_LABEL),
            "placeholder must disappear once the user types — found stale `{}` next to user input",
            super::OTHER_LABEL,
        );
    }
}
