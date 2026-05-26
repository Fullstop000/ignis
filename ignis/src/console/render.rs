use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{
    App, McpPicker, Mode, ModelPicker, SessionPicker, SkillPicker, ToolCallEntry, ToolStatus,
    UIBlock,
};
use super::markdown::render_md_block;
use super::{
    format_context, format_duration, format_tokens, highlight, sanitize, truncate, ACCENT, BG,
    BORDER, BORDER_ACTIVE, DIFF_ADD_BG, DIFF_DEL_BG, GREEN, MAUVE, RED, SPINNERS, SUBTEXT, SURFACE,
    SURFACE_2, TEXT, TEXT_DIM, YELLOW,
};

/// Max queued rows shown before collapsing to a "+N more" row.
const MAX_QUEUE_ROWS: usize = 5;
/// Max slash-suggestion rows shown at once; the list scrolls to keep the
/// selected entry visible when there are more (e.g. many skills + `/skills`).
const MAX_SLASH_ROWS: u16 = 8;

/// Adaptive hint shown above the input while busy (None = no hint row).
pub(crate) fn queued_hint(app: &App) -> Option<String> {
    if app.mode == Mode::Idle {
        return None;
    }
    let has_queue = !app.queue.is_empty();
    let typing = !app.input.is_empty();
    if !has_queue && !typing {
        return None;
    }
    Some(if has_queue {
        "↑ edit last · Enter queue · Ctrl+S send now".to_string()
    } else {
        "Enter queue · Ctrl+S send now".to_string()
    })
}

/// Height of the queued-rows + hint region between the status line and input.
pub(crate) fn queued_region_height(app: &App) -> u16 {
    if app.mode == Mode::Idle {
        return 0;
    }
    let shown = app.queue.len().min(MAX_QUEUE_ROWS) as u16;
    let overflow = if app.queue.len() > MAX_QUEUE_ROWS {
        1
    } else {
        0
    };
    let rows = if shown > 0 { 1 + shown + overflow } else { 0 }; // leading blank
    let hint = if queued_hint(app).is_some() { 1 } else { 0 };
    rows + hint
}

/// Live-region height (rows) the inline viewport needs for the current state.
/// Finalized transcript blocks live in the terminal's own scrollback; only this
/// band is repainted. The band grows for the multi-line input, slash
/// suggestions, and the modal pickers, and collapses to a tidy strip at rest.
pub(crate) fn live_height(app: &App, term_rows: u16) -> u16 {
    let cap = term_rows.saturating_sub(1).max(3);
    // `ask_user` runs while busy and owns the whole live band, same as the
    // slash pickers.
    if let Some(p) = &app.inline_picker {
        return super::inline_picker::picker_height(p).clamp(4, cap);
    }
    if app.model_picker.is_some() {
        let rows = app.model_options.len() as u16 + 4;
        return rows.clamp(4, cap);
    }
    if let Some(p) = &app.session_picker {
        let rows = p.sessions.len().max(1) as u16 + 4;
        return rows.clamp(4, cap);
    }
    if let Some(_p) = &app.skill_picker {
        let rows = app.skills.as_deref().map(|r| r.all().len()).unwrap_or(0) as u16 + 4;
        return rows.clamp(4, cap);
    }
    if app.mcp_picker.is_some() {
        let rows = app.mcp.as_deref().map(|r| r.len()).unwrap_or(0) as u16 + 4;
        return rows.clamp(4, cap);
    }
    let input_h = input_height(app, cap);
    let sugg = app.slash_suggestions();
    let sugg_h = if app.mode == Mode::Idle && !sugg.is_empty() {
        (sugg.len() as u16).min(MAX_SLASH_ROWS)
    } else {
        0
    };
    let queued_h = queued_region_height(app);
    (1 + queued_h + sugg_h + input_h + 1).min(cap) // status + queued + suggestions + input + footer
}

/// Input box height (incl. borders), growing with newline-separated lines.
fn input_height(app: &App, cap: u16) -> u16 {
    let lines = app.input.split('\n').count().max(1) as u16;
    (lines + 2).clamp(3, cap.saturating_sub(2).max(3))
}

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let size = f.size();
    f.render_widget(Block::default().style(Style::default().bg(BG)), size);

    // Inline (`ask_user`) picker with at least one option carrying a preview
    // gets its own split layout — option list left, bordered Preview pane
    // right. The other pickers fall through to the single-paragraph path.
    if let Some(picker) = &app.inline_picker {
        if picker.has_any_preview() {
            render_inline_picker_split(f, size, picker);
            return;
        }
    }

    // Modal pickers own the whole live band.
    if app.model_picker.is_some()
        || app.session_picker.is_some()
        || app.skill_picker.is_some()
        || app.mcp_picker.is_some()
        || app.inline_picker.is_some()
    {
        let mut lines: Vec<Line> = Vec::new();
        if let Some(picker) = &app.inline_picker {
            super::inline_picker::render_inline_picker(&mut lines, picker);
        } else if let Some(picker) = &app.model_picker {
            render_model_picker(&mut lines, picker, &app.model_options);
        } else if let Some(picker) = &app.session_picker {
            render_session_picker(&mut lines, picker);
        } else if let Some(picker) = &app.skill_picker {
            // Rows available for skill items = band minus header (2) + footer (1).
            let max_rows = (size.height as usize).saturating_sub(3).max(1);
            render_skill_picker(&mut lines, picker, app.skills.as_deref(), max_rows);
        } else if let Some(picker) = &app.mcp_picker {
            let max_rows = (size.height as usize).saturating_sub(3).max(1);
            render_mcp_picker(&mut lines, picker, app.mcp.as_deref(), max_rows);
        }
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false }),
            size,
        );
        return;
    }

    let input_h = input_height(app, size.height);
    let sugg = app.slash_suggestions();
    let sugg_h = if app.mode == Mode::Idle && !sugg.is_empty() {
        size.height
            .saturating_sub(input_h + 2)
            .min((sugg.len() as u16).min(MAX_SLASH_ROWS))
    } else {
        0
    };
    let queued_h = queued_region_height(app);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // loading / status
            Constraint::Length(queued_h), // queued rows + hint (0 when idle/clean)
            Constraint::Length(sugg_h),   // slash suggestions (0 when none)
            Constraint::Length(input_h),  // input
            Constraint::Length(1),        // footer
        ])
        .split(size);

    draw_loading(f, layout[0], app);
    if queued_h > 0 {
        draw_queued(f, layout[1], app);
    }
    if sugg_h > 0 {
        draw_slash_suggestions(f, layout[2], app);
    }
    draw_input(f, layout[3], app);
    draw_footer(f, layout[4], app);
}

/// Split-layout render for the `ask_user` picker when at least one option
/// carries a `preview`. The band is divided:
///   - top: blank + header strip + question (3 rows)
///   - middle: horizontal split — option list left (~45%), bordered Preview
///     pane right (~55%) showing the focused option's title + description +
///     preview text
///   - bottom: blank + footer (2 rows)
fn render_inline_picker_split(
    f: &mut Frame,
    size: Rect,
    picker: &super::inline_picker::InlinePickerState,
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header section
            Constraint::Min(4),    // middle (split)
            Constraint::Length(2), // footer section
        ])
        .split(size);

    // Header section (full width)
    let header_lines = super::inline_picker::header_lines(picker);
    f.render_widget(
        Paragraph::new(Text::from(header_lines))
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        outer[0],
    );

    // Middle horizontal split: options | preview
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(outer[1]);

    let left_lines = super::inline_picker::options_pane_lines(picker);
    f.render_widget(
        Paragraph::new(Text::from(left_lines))
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        middle[0],
    );

    let right_lines = super::inline_picker::preview_pane_lines(picker);
    let preview_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            " Preview ",
            Style::default().fg(TEXT_DIM).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(
        Paragraph::new(Text::from(right_lines))
            .block(preview_block)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        middle[1],
    );

    // Footer section (full width)
    let footer_lines = super::inline_picker::footer_lines(picker);
    f.render_widget(
        Paragraph::new(Text::from(footer_lines))
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        outer[2],
    );
}

/// Build the rendered lines for one transcript block, for committing to the
/// terminal scrollback via `insert_before`. Empty (placeholder) assistant blocks
/// yield no lines.
pub(crate) fn block_lines(
    block: &UIBlock,
    tick: u64,
    cwd: &Path,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match block {
        UIBlock::User(text) => {
            lines.push(Line::from(""));
            for (i, l) in text.lines().enumerate() {
                let prefix = if i == 0 { "👤 " } else { "   " };
                let line = Line::from(vec![
                    Span::styled(prefix, Style::default().fg(ACCENT)),
                    Span::styled(
                        sanitize(l),
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    ),
                ]);
                // Continuation rows align under the prompt text (past "👤 ").
                lines.extend(wrap_line(&line, width, 3));
            }
        }
        UIBlock::Assistant(text) => {
            if text.is_empty() {
                return lines;
            }
            lines.push(Line::from(""));
            for line in render_md_block(text, false) {
                let indent = leading_space_cols(&line);
                lines.extend(wrap_line(&line, width, indent));
            }
        }
        UIBlock::Tool(entry) => {
            // The `ask_user` tool has its own purpose-built scrollback line
            // (`inline_picker::trace_lines`); rendering the generic tool block
            // would dump verbose JSON args+result twice. We still want a record
            // on session resume — the live trace is ephemeral — so build a
            // compact trace from the persisted entry instead.
            if entry.name == "ask_user" {
                lines.extend(ask_user_resume_trace(entry));
                return lines;
            }
            let mut raw: Vec<Line<'static>> = Vec::new();
            render_tool_block(&mut raw, entry, tick, cwd, width);
            for line in raw {
                let indent = leading_space_cols(&line);
                lines.extend(wrap_line(&line, width, indent));
            }
        }
    }
    lines
}

/// Columns of leading spaces on a line (its natural indent).
fn leading_space_cols(line: &Line) -> usize {
    let mut n = 0;
    for span in &line.spans {
        for c in span.content.chars() {
            if c == ' ' {
                n += 1;
            } else {
                return n;
            }
        }
    }
    n
}

/// Group consecutive same-style cells back into spans.
fn cells_to_spans(cells: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for &(c, st) in cells {
        if cur != Some(st) {
            if let (Some(s), false) = (cur, buf.is_empty()) {
                spans.push(Span::styled(std::mem::take(&mut buf), s));
            }
            cur = Some(st);
        }
        buf.push(c);
    }
    if let (Some(s), false) = (cur, buf.is_empty()) {
        spans.push(Span::styled(buf, s));
    }
    spans
}

/// Word-wrap one rendered line to `width`, carrying `indent_cols` spaces onto
/// each continuation row so wrapped text stays left-aligned (ratatui's own wrap
/// drops the indent, leaving a ragged margin). Span styles are preserved.
fn wrap_line(line: &Line<'static>, width: u16, indent_cols: usize) -> Vec<Line<'static>> {
    let width = (width as usize).max(8);
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            let st = s.style;
            s.content.chars().map(move |c| (c, st))
        })
        .collect();
    let total: usize = cells
        .iter()
        .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
        .sum();
    if total <= width {
        return vec![line.clone()];
    }

    let indent_cols = indent_cols.min(width.saturating_sub(1));
    let indent_style = cells.first().map(|(_, s)| *s).unwrap_or_default();
    let indent_row = || -> Vec<(char, Style)> { vec![(' ', indent_style); indent_cols] };

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    let mut first_row = true;
    let n = cells.len();
    let mut i = 0;
    while i < n {
        let ws = i;
        while i < n && cells[i].0 != ' ' {
            i += 1;
        }
        let word = &cells[ws..i];
        let word_w: usize = word
            .iter()
            .map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0))
            .sum();
        let floor = if first_row { 0 } else { indent_cols };
        if cur_w + word_w > width && cur_w > floor {
            rows.push(std::mem::take(&mut cur));
            first_row = false;
            cur = indent_row();
            cur_w = indent_cols;
        }
        cur.extend_from_slice(word);
        cur_w += word_w;
        // Trailing run of spaces; keep only if it still fits the row.
        let ss = i;
        while i < n && cells[i].0 == ' ' {
            i += 1;
        }
        let spaces = &cells[ss..i];
        if cur_w + spaces.len() <= width {
            cur.extend_from_slice(spaces);
            cur_w += spaces.len();
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows.into_iter()
        .map(|r| Line::from(cells_to_spans(&r)))
        .collect()
}

/// Compact one-line trace for a persisted `ask_user` tool entry. Parses the
/// stored result JSON (`{"answers": [{"question": ..., "answer": ...}, ...]}`)
/// and emits one row per answered question, plus a cancellation marker if the
/// result was an error. Same look as the live trace from
/// `inline_picker::trace_lines` so resumed sessions read identically.
fn ask_user_resume_trace(entry: &ToolCallEntry) -> Vec<Line<'static>> {
    let (result_text, is_error) = match &entry.status {
        ToolStatus::Success(s) => (s.clone(), false),
        ToolStatus::Error(s) => (s.clone(), true),
        ToolStatus::Pending => return Vec::new(), // nothing to commit yet
    };
    let mut out = vec![Line::from("")];
    if is_error {
        // Distinguish a real cancellation (the tool's literal message) from
        // other error paths (schema validation, console-closed, headless run,
        // already-open). Anything that's not the cancellation sentence
        // preserves the real error text instead of pretending the user
        // cancelled.
        let is_cancel = result_text.contains("User cancelled the question");
        let label = if is_cancel {
            " · cancelled by user".to_string()
        } else {
            format!(" · {}", result_text.trim())
        };
        out.push(Line::from(vec![
            Span::styled("  ✗ ", Style::default().fg(RED)),
            Span::styled(
                "ask_user",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label, Style::default().fg(TEXT_DIM)),
        ]));
        return out;
    }
    // Parse the success JSON shape. Fall back to a generic line if it's
    // anything unexpected — preserves the resume invariant ("never silent").
    let parsed = serde_json::from_str::<serde_json::Value>(&result_text)
        .ok()
        .and_then(|v| {
            v.get("answers")
                .and_then(|a| a.as_array())
                .map(|a| a.to_vec())
        });
    match parsed {
        Some(answers) if !answers.is_empty() => {
            // Header chips come from the request args; the result JSON only
            // carries the question text, so reuse that as the label.
            for a in answers {
                let question = a
                    .get("question")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string();
                let answer_text = match a.get("answer") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Array(items)) => items
                        .iter()
                        .filter_map(|i| i.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                out.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(GREEN)),
                    Span::styled(
                        "ask_user",
                        Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" · ", Style::default().fg(BORDER)),
                    Span::styled(
                        question,
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(": ", Style::default().fg(TEXT_DIM)),
                    Span::styled(answer_text, Style::default().fg(TEXT)),
                ]));
            }
        }
        _ => {
            out.push(Line::from(vec![
                Span::styled("  ✓ ", Style::default().fg(GREEN)),
                Span::styled(
                    "ask_user",
                    Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }
    out
}

/// Wrapped row count of `lines` at `width` — the height to reserve in scrollback.
pub(crate) fn block_height(lines: &[Line<'static>], width: u16) -> u16 {
    Paragraph::new(Text::from(lines.to_vec()))
        .wrap(Wrap { trim: false })
        .line_count(width.max(1)) as u16
}

/// Render `lines` into a scrollback buffer slice (used by `insert_before`).
pub(crate) fn render_block_into(buf: &mut ratatui::buffer::Buffer, lines: &[Line<'static>]) {
    use ratatui::widgets::Widget;
    let area = buf.area;
    Paragraph::new(Text::from(lines.to_vec()))
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false })
        .render(area, buf);
    blank_wide_char_continuations(buf);
}

/// Empty the cell that follows each double-width glyph (CJK, emoji).
///
/// `Terminal::insert_before` flushes *every* buffer cell to the backend — it
/// skips the `diff` step that, in a normal draw, drops the blank continuation
/// cell after a wide glyph. The crossterm backend then prints that blank `" "`
/// at the column the wide glyph already advanced past, leaving a stray space
/// after every wide char. Clearing the continuation symbol makes the backend
/// print nothing there, so the glyph keeps its natural two columns.
fn blank_wide_char_continuations(buf: &mut ratatui::buffer::Buffer) {
    let area = buf.area;
    for y in area.top()..area.bottom() {
        let mut x = area.left();
        while x < area.right() {
            let w = UnicodeWidthStr::width(buf.get(x, y).symbol());
            if w >= 2 {
                if x + 1 < area.right() {
                    buf.get_mut(x + 1, y).set_symbol("");
                }
                x += 2;
            } else {
                x += 1;
            }
        }
    }
}

/// The startup banner, committed once to scrollback at launch.
pub(crate) fn welcome_lines(app: &App) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("🔥 ", Style::default()),
            Span::styled(
                "ignis",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  Your AI coding agent, right in the terminal.",
                Style::default().fg(SUBTEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Provider  ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                format!("{}/{}", app.provider, app.model),
                Style::default().fg(TEXT),
            ),
            Span::styled("   Directory  ", Style::default().fg(TEXT_DIM)),
            Span::styled(format!("{}", app.cwd.display()), Style::default().fg(TEXT)),
        ]),
        Line::from(""),
    ]
}

/// Loading/status line shown directly above the input box (Claude Code style).
pub(crate) fn draw_loading(f: &mut Frame, area: Rect, app: &App) {
    let line = if app.exit_pending {
        Line::from(Span::styled(
            "  Press Ctrl-D again to exit",
            Style::default().fg(YELLOW),
        ))
    } else if let Some((msg, _)) = &app.error_flash {
        Line::from(Span::styled(
            format!("  ✗ {}", msg),
            Style::default().fg(RED),
        ))
    } else if app.mode == Mode::Idle {
        Line::from("")
    } else {
        let label = match app.mode {
            Mode::Thinking => app.thinking_label(),
            Mode::ToolRunning => "Running tool",
            Mode::Idle => "",
        };
        let mut spans = vec![
            Span::styled(format!("  {} ", app.spinner()), Style::default().fg(ACCENT)),
            Span::styled(format!("{}… ", label), Style::default().fg(SUBTEXT)),
            Span::styled(app.elapsed_str(), Style::default().fg(TEXT_DIM)),
        ];
        // Token stats: ↑ input/context (real when known) and ↓ live output
        // (chars/4 estimate) + rate once the reply is flowing.
        let (ctx_tokens, _) = app.context_usage();
        let tok_segment = if app.stream_tokens() > 0 {
            format!(
                "  ·  ↑ {} ↓ {} tok · {}/s",
                format_tokens(ctx_tokens as usize),
                format_tokens(app.stream_tokens()),
                format_tokens(app.stream_rate()),
            )
        } else {
            format!("  ·  ↑ {} tok", format_tokens(ctx_tokens as usize))
        };
        spans.push(Span::styled(tok_segment, Style::default().fg(TEXT_DIM)));
        spans.push(Span::styled(
            "  ·  ctrl+c to interrupt",
            Style::default().fg(TEXT_DIM),
        ));
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line).style(Style::default().bg(BG)), area);
}

/// Queued prompts (dim, truncated) + the adaptive hint, between status and input.
pub(crate) fn draw_queued(f: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    if !app.queue.is_empty() {
        lines.push(Line::from(""));
        for text in app.queue.iter().take(MAX_QUEUE_ROWS) {
            lines.push(Line::from(vec![
                Span::styled("  ↳ ", Style::default().fg(TEXT_DIM)),
                Span::styled(truncate(&sanitize(text), 72), Style::default().fg(SUBTEXT)),
            ]));
        }
        if app.queue.len() > MAX_QUEUE_ROWS {
            lines.push(Line::from(Span::styled(
                format!("    +{} more", app.queue.len() - MAX_QUEUE_ROWS),
                Style::default().fg(TEXT_DIM),
            )));
        }
    }
    if let Some(hint) = queued_hint(app) {
        lines.push(Line::from(Span::styled(
            format!("  {}", hint),
            Style::default().fg(TEXT_DIM),
        )));
    }
    f.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().bg(BG)),
        area,
    );
}

/// Status footer under the input: working dir (left) and model + token usage (right).
pub(crate) fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let (ctx_tokens, ctx_pct) = app.context_usage();
    let model_str = match &app.effort {
        Some(e) => format!("{}/{} ({})", app.provider, app.model, e),
        None => format!("{}/{}", app.provider, app.model),
    };
    let right_str = format!(
        " {}  ·  {} tok ({}%) ",
        model_str,
        format_tokens(ctx_tokens as usize),
        ctx_pct
    );
    let right_w = right_str.chars().count() as u16;
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_w)])
        .split(area);

    let left = Line::from(Span::styled(
        format!("  {}", app.cwd.display()),
        Style::default().fg(TEXT_DIM),
    ));
    let right = Line::from(Span::styled(right_str, Style::default().fg(SUBTEXT)));

    f.render_widget(
        Paragraph::new(left).style(Style::default().bg(SURFACE)),
        split[0],
    );
    f.render_widget(
        Paragraph::new(right)
            .style(Style::default().bg(SURFACE))
            .alignment(ratatui::layout::Alignment::Right),
        split[1],
    );
}

pub(crate) fn render_session_picker(lines: &mut Vec<Line<'static>>, picker: &SessionPicker) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Sessions",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    if picker.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No sessions found.",
            Style::default().fg(TEXT_DIM),
        )));
        return;
    }

    for (idx, session) in picker.sessions.iter().enumerate() {
        let selected = idx == picker.selected;
        let marker = if selected { ">" } else { " " };
        let preview = if session.preview.is_empty() {
            "(no preview)".to_string()
        } else {
            truncate(&session.preview, 48)
        };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", marker), style.add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{:<24}", truncate(&session.id, 24)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {} msg  {}  {}",
                    session.message_count,
                    session.age_str(),
                    preview
                ),
                style,
            ),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "  Use Up/Down to choose, Enter to resume.",
        Style::default().fg(TEXT_DIM),
    )));
}

pub(crate) fn render_skill_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &SkillPicker,
    registry: Option<&crate::skills::SkillRegistry>,
    max_rows: usize,
) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Manage skills",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    let Some(reg) = registry else { return };
    let skills = reg.all();
    // Scroll the window so the selected row stays visible when there are more
    // skills than fit in the band.
    let sel = picker.selected.min(skills.len().saturating_sub(1));
    let visible = max_rows.max(1);
    let start = slash_window_start(sel, visible, skills.len());
    let end = (start + visible).min(skills.len());
    for (idx, skill) in skills.iter().enumerate().take(end).skip(start) {
        let selected = idx == sel;
        let marker = if selected { ">" } else { " " };
        let check = if reg.is_enabled(&skill.name) {
            "[x]"
        } else {
            "[ ]"
        };
        let scope = match skill.scope {
            crate::skills::SkillScope::Project => "project",
            crate::skills::SkillScope::Global => "global",
        };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let desc = skill.description.clone().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker} {check} "),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<18}", truncate(&skill.name, 18)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}  ({scope})", truncate(&desc, 40)), style),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "  Up/Down to move, Space/Enter to toggle, Esc to close.",
        Style::default().fg(TEXT_DIM),
    )));
}

pub(crate) fn render_mcp_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &McpPicker,
    registry: Option<&crate::mcp::McpRegistry>,
    max_rows: usize,
) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Manage MCP servers",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    let Some(reg) = registry else { return };
    let entries = reg.entries();
    let sel = picker.selected.min(entries.len().saturating_sub(1));
    let visible = max_rows.max(1);
    let start = slash_window_start(sel, visible, entries.len());
    let end = (start + visible).min(entries.len());
    for (idx, entry) in entries.iter().enumerate().take(end).skip(start) {
        let selected = idx == sel;
        let marker = if selected { ">" } else { " " };
        let check = match entry.status {
            crate::mcp::McpStatus::Disabled => "[ ]",
            _ => "[x]",
        };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let status_style = match entry.status {
            crate::mcp::McpStatus::Failed { .. } => style.fg(if selected { BG } else { RED }),
            crate::mcp::McpStatus::Disabled => style.fg(if selected { BG } else { TEXT_DIM }),
            _ => style.fg(if selected { BG } else { GREEN }),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker} {check} "),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<14}", truncate(&entry.name, 14)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", entry.status.label()), status_style),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "  Up/Down to move, Space/Enter to toggle, Esc to close.",
        Style::default().fg(TEXT_DIM),
    )));
}

pub(crate) fn render_model_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &ModelPicker,
    options: &[crate::models::ModelOption],
) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Switch model",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    if options.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No models configured.",
            Style::default().fg(TEXT_DIM),
        )));
        return;
    }

    // Effort row, shown only when the highlighted model declares levels.
    let sel = picker.selected.min(options.len() - 1);
    let levels = &options[sel].effort_levels;
    if !levels.is_empty() {
        let mut spans = vec![Span::styled("  effort:", Style::default().fg(TEXT_DIM))];
        for (i, level) in levels.iter().enumerate() {
            let style = if i == picker.effort_idx {
                Style::default()
                    .fg(BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(SUBTEXT)
            };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!(" {} ", level), style));
        }
        spans.push(Span::styled("   ←/→", Style::default().fg(TEXT_DIM)));
        lines.push(Line::from(spans));
    }

    for (idx, opt) in options.iter().enumerate() {
        let selected = idx == picker.selected;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let label = if opt.effort_levels.is_empty() {
            format!("{}/{}", opt.provider, opt.model)
        } else {
            format!("{}/{} ◆", opt.provider, opt.model)
        };
        let mut spans = vec![
            Span::styled(format!("  {} ", marker), style.add_modifier(Modifier::BOLD)),
            Span::styled(label, style.add_modifier(Modifier::BOLD)),
        ];
        if let Some(ctx) = opt.context {
            let ctx_style = if selected {
                style
            } else {
                Style::default().fg(TEXT_DIM)
            };
            spans.push(Span::styled(
                format!("  {} ctx", format_context(ctx)),
                ctx_style,
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(Span::styled(
        "  Up/Down model · ←/→ effort · Enter apply · Esc cancel",
        Style::default().fg(TEXT_DIM),
    )));
}

pub(crate) fn render_tool_block(
    lines: &mut Vec<Line<'static>>,
    entry: &ToolCallEntry,
    tick: u64,
    cwd: &Path,
    width: u16,
) {
    let (icon, color, status_line, elapsed) = match &entry.status {
        ToolStatus::Pending => {
            let spinner = SPINNERS[(tick as usize / 10) % SPINNERS.len()];
            let ms = entry.started_at.elapsed().as_millis();
            (
                spinner,
                YELLOW,
                format!("running… {}", format_duration(ms)),
                String::new(),
            )
        }
        ToolStatus::Success(out) => {
            let elapsed = format_duration(entry.elapsed_ms);
            let preview = truncate(out.trim(), 300);
            ("+", GREEN, preview, elapsed)
        }
        ToolStatus::Error(err) => {
            let elapsed = format_duration(entry.elapsed_ms);
            let preview = truncate(&sanitize(err.trim()), 300);
            ("x", RED, preview, elapsed)
        }
    };

    // Parse tool arguments for a compact display
    let args_compact = sanitize(&compact_tool_args(&entry.arguments, cwd));

    lines.push(Line::from(""));
    // Header line: ┌─ ⚙ tool_name(args) [1.2s]
    let mut header = vec![
        Span::styled("  ┌ ", Style::default().fg(color)),
        Span::styled(
            format!("{} ", icon),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            entry.name.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if !args_compact.is_empty() {
        header.push(Span::styled(
            format!("({})", args_compact),
            Style::default().fg(TEXT_DIM),
        ));
    }
    if !elapsed.is_empty() {
        header.push(Span::styled(
            format!(" {}", elapsed),
            Style::default().fg(TEXT_DIM),
        ));
    }
    lines.push(Line::from(header));

    // Status / output lines (collapsed for success, expanded for errors)
    match &entry.status {
        ToolStatus::Pending => {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(color)),
                Span::styled(status_line, Style::default().fg(TEXT_DIM)),
            ]));
        }
        ToolStatus::Success(out) => {
            // edit_file returns a git-style diff: render the hunk with solid
            // red/green backgrounds and syntax-highlighted code. Other tools get
            // a compact 3-line preview.
            let is_diff = entry.name == "edit_file";
            let max = if is_diff { 30 } else { 3 };
            if is_diff {
                let ext = diff_file_ext(&entry.arguments);
                for sl in out.lines().take(max) {
                    push_diff_line(lines, sl, &ext, color, width);
                }
            } else {
                for sl in out.lines().take(max) {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(color)),
                        Span::styled(truncate(&sanitize(sl), 200), Style::default().fg(TEXT_DIM)),
                    ]));
                }
            }
            let total_lines = out.lines().count();
            if total_lines > max {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(color)),
                    Span::styled(
                        format!("… {} more lines", total_lines - max),
                        Style::default().fg(TEXT_DIM),
                    ),
                ]));
            }
        }
        ToolStatus::Error(_) => {
            for sl in status_line.lines().take(5) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(color)),
                    Span::styled(sl.to_string(), Style::default().fg(RED)),
                ]));
            }
        }
    }

    lines.push(Line::from(Span::styled("  └", Style::default().fg(color))));
}

/// The active file's extension (for syntax highlighting), from the tool args.
fn diff_file_ext(args_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args_json)
        .ok()
        .and_then(|v| {
            ["path", "file_path"]
                .iter()
                .find_map(|k| v.get(k).and_then(|p| p.as_str()).map(String::from))
        })
        .and_then(|p| {
            Path::new(&p)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Render one diff line. Added (`+`) and removed (`-`) lines get a solid
/// background filling the row and syntax-highlighted code; other lines render
/// plain. `width` is the messages-area width (for the full-row background).
fn push_diff_line(
    lines: &mut Vec<Line<'static>>,
    raw: &str,
    ext: &str,
    border: ratatui::style::Color,
    width: u16,
) {
    let prefix = Span::styled("  │ ", Style::default().fg(border));
    let (sign, bg, sign_fg) = match raw.as_bytes().first() {
        Some(b'+') => ('+', DIFF_ADD_BG, GREEN),
        Some(b'-') => ('-', DIFF_DEL_BG, RED),
        _ => {
            lines.push(Line::from(vec![
                prefix,
                Span::styled(truncate(&sanitize(raw), 200), Style::default().fg(TEXT_DIM)),
            ]));
            return;
        }
    };

    // Content area = width − "  │ " (4) − "± " (2); fill it so the bg spans the row.
    let content_w = (width as usize).saturating_sub(6).max(8);
    // `truncate` appends `…` past its limit, so cap at content_w − 1: a truncated
    // line is then exactly content_w cells and never wraps off the bg bar.
    let code = truncate(&sanitize(raw.get(2..).unwrap_or("")), content_w - 1);

    let mut spans = vec![
        prefix,
        Span::styled(
            format!("{sign} "),
            Style::default()
                .fg(sign_fg)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let mut used = 0usize;
    for (fg, text) in highlight::highlight_line(&code, ext) {
        used += UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, Style::default().fg(fg).bg(bg)));
    }
    if let Some(pad) = content_w.checked_sub(used).filter(|p| *p > 0) {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    lines.push(Line::from(spans));
}

/// Produce a compact arg summary from JSON, showing **values only** (never the
/// parameter names): `grep("fn main")`, `read_file(src/main.rs)`. Path-valued
/// args render bare and relative to `cwd`; other strings keep their quotes.
pub(crate) fn compact_tool_args(json_str: &str, cwd: &Path) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return truncate(json_str, 60);
    };
    let Some(obj) = val.as_object() else {
        return truncate(json_str, 60);
    };
    let mut parts = Vec::new();
    for (k, v) in obj {
        let s = match v {
            serde_json::Value::String(s) if is_path_key(k) => {
                truncate(&relativize_path(s, cwd), 60)
            }
            serde_json::Value::String(s) => format!("\"{}\"", truncate(s, 40)),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            other => truncate(&other.to_string(), 30),
        };
        parts.push(s);
    }
    let joined = parts.join(", ");
    truncate(&joined, 80)
}

fn is_path_key(key: &str) -> bool {
    matches!(key, "path" | "file_path")
}

/// Shorten a path for display by dropping the current-directory prefix: an
/// absolute path under `cwd`, a leading `./`, or a leading `<cwd-name>/`
/// (e.g. running in `…/ignis`, `ignis/src/x` → `src/x`).
fn relativize_path(p: &str, cwd: &Path) -> String {
    let p = p.trim();
    if let Ok(stripped) = Path::new(p).strip_prefix(cwd) {
        return stripped.to_string_lossy().into_owned();
    }
    let rel = p.strip_prefix("./").unwrap_or(p);
    if let Some(name) = cwd.file_name().and_then(|n| n.to_str()) {
        if let Some(rest) = rel.strip_prefix(&format!("{name}/")) {
            return rest.to_string();
        }
    }
    rel.to_string()
}

pub(crate) fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let idle = app.mode == Mode::Idle;
    let border_color = if idle { BORDER_ACTIVE } else { BORDER };

    let content = if app.input.is_empty() {
        let placeholder = if idle {
            "Type a message…"
        } else {
            "Type your next message…"
        };
        Text::from(Span::styled(placeholder, Style::default().fg(TEXT_DIM)))
    } else {
        Text::from(
            app.input
                .split('\n')
                .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(TEXT))))
                .collect::<Vec<_>>(),
        )
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(SURFACE_2))
        .title(Span::styled(
            if idle { " > " } else { " … " },
            Style::default().fg(if idle { ACCENT } else { TEXT_DIM }),
        ));
    if !app.queue.is_empty() {
        block = block.title(
            ratatui::widgets::block::Title::from(Span::styled(
                format!(" · {} queued ", app.queue.len()),
                Style::default().fg(SUBTEXT),
            ))
            .alignment(ratatui::layout::Alignment::Right),
        );
    }

    let p = Paragraph::new(content).block(block);
    f.render_widget(p, area);

    // Cursor is shown whenever the input has focus — idle or busy (you can type
    // while the agent works to queue / steer).
    let before = &app.input[..app.cursor];
    let row = before.matches('\n').count() as u16;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = UnicodeWidthStr::width(&app.input[line_start..app.cursor]) as u16;
    f.set_cursor(area.x + 1 + col, area.y + 1 + row);
}

/// Render the slash-command suggestions into `area` (the band reserved above the
/// input). One row per suggestion; the highlighted row is inverted.
/// First index of the visible slash-suggestion window so that `sel` stays in
/// view: `[start, start+visible)` always contains `sel`.
fn slash_window_start(sel: usize, visible: usize, len: usize) -> usize {
    let visible = visible.max(1);
    let sel = sel.min(len.saturating_sub(1));
    if sel >= visible {
        sel - visible + 1
    } else {
        0
    }
}

pub(crate) fn draw_slash_suggestions(f: &mut Frame, area: Rect, app: &App) {
    let suggestions = app.slash_suggestions();
    if suggestions.is_empty() || area.height == 0 {
        return;
    }
    let visible = (area.height as usize).max(1);
    let sel = app.slash_selection.min(suggestions.len() - 1);
    // Scroll the window so the selected entry is always shown (the list can be
    // longer than `visible` once skills + `/skills` are present).
    let start = slash_window_start(sel, visible, suggestions.len());
    let end = (start + visible).min(suggestions.len());
    let mut lines = Vec::new();
    for (idx, suggestion) in suggestions.iter().enumerate().take(end).skip(start) {
        let selected = idx == sel;
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT).bg(SURFACE)
        };
        lines.push(Line::from(vec![
            Span::styled(
                if selected { " > " } else { "   " },
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<10}", suggestion.name),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", suggestion.description), style),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(SURFACE)),
        area,
    );
}

// ==========================================
// Render Tests
// ==========================================

#[cfg(test)]
mod queue_render_tests {
    use super::*;
    use crate::console::app::{App, Mode};
    use std::path::PathBuf;

    fn app() -> App {
        App::new("p".into(), "m".into(), "s".into(), PathBuf::from("/tmp"))
    }

    #[test]
    fn slash_window_keeps_selection_visible() {
        // Within the first window → no scroll.
        assert_eq!(slash_window_start(0, 5, 10), 0);
        assert_eq!(slash_window_start(4, 5, 10), 0);
        // Past the window → scroll so the selection is the last visible row.
        assert_eq!(slash_window_start(5, 5, 10), 1);
        assert_eq!(slash_window_start(9, 5, 10), 5);
        // Fewer items than the window, or zero height → start at 0.
        assert_eq!(slash_window_start(2, 8, 3), 0);
        assert_eq!(slash_window_start(0, 0, 1), 0);
    }

    #[test]
    fn hint_is_none_when_idle_or_clean_busy() {
        let mut a = app();
        a.mode = Mode::Idle;
        assert_eq!(queued_hint(&a), None);
        a.mode = Mode::Thinking; // busy, empty input, empty queue
        assert_eq!(queued_hint(&a), None);
    }

    #[test]
    fn hint_text_depends_on_queue_presence() {
        let mut a = app();
        a.mode = Mode::Thinking;
        a.input = "typing".into();
        assert_eq!(
            queued_hint(&a).as_deref(),
            Some("Enter queue · Ctrl+S send now")
        );
        a.queue.push("q".into());
        assert_eq!(
            queued_hint(&a).as_deref(),
            Some("↑ edit last · Enter queue · Ctrl+S send now")
        );
    }

    #[test]
    fn render_block_blanks_wide_char_continuations() {
        // Regression: `insert_before` flushes every cell, so the blank cell after
        // a double-width glyph would print as a stray space in scrollback. We
        // empty it. "ab的cd" → 的 occupies cells 2-3; cell 3 must be "".
        use ratatui::{buffer::Buffer, layout::Rect, text::Line};
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        render_block_into(&mut buf, &[Line::from("ab的cd")]);
        assert_eq!(buf.get(2, 0).symbol(), "的");
        assert_eq!(
            buf.get(3, 0).symbol(),
            "",
            "wide-char continuation must be emptied"
        );
        assert_eq!(buf.get(4, 0).symbol(), "c");
    }

    #[test]
    fn region_height_grows_with_queue_and_hint() {
        let mut a = app();
        a.mode = Mode::Idle;
        assert_eq!(queued_region_height(&a), 0);
        a.mode = Mode::Thinking;
        a.input = "x".into(); // hint only
        assert_eq!(queued_region_height(&a), 1);
        a.input.clear();
        a.queue = vec!["one".into(), "two".into()]; // blank + 2 rows + hint
        assert_eq!(queued_region_height(&a), 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::path::PathBuf;

    fn test_terminal(w: u16, h: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(w, h)).unwrap()
    }

    fn buffer_content(term: &Terminal<TestBackend>) -> String {
        term.backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Render all of `app`'s transcript blocks the way they're committed to
    /// scrollback (`block_lines`), into a TestBackend for assertions.
    fn render_blocks(app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut term = test_terminal(w, h);
        let blocks = app.blocks.clone();
        let cwd = app.cwd.clone();
        let tick = app.tick;
        term.draw(|f| {
            let mut lines: Vec<Line> = Vec::new();
            for b in &blocks {
                lines.extend(block_lines(b, tick, &cwd, w));
            }
            f.render_widget(
                Paragraph::new(Text::from(lines))
                    .style(Style::default().bg(BG))
                    .wrap(Wrap { trim: false }),
                f.size(),
            );
        })
        .unwrap();
        term
    }

    #[test]
    fn render_model_picker_shows_effort_only_for_reasoning_models() {
        let mut app = App::new(
            "deepseek".to_string(),
            "deepseek-v4-flash".to_string(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        );
        let opt = |p: &str, m: &str, l: &[&str]| crate::models::ModelOption {
            provider: p.to_string(),
            model: m.to_string(),
            effort_levels: l.iter().map(|s| s.to_string()).collect(),
            context: None,
        };
        app.set_model_options(
            vec![
                opt("deepseek", "deepseek-v4-flash", &[]),
                opt("deepseek", "deepseek-v4-pro", &["high", "max"]),
            ],
            None,
        );
        app.show_model_picker(); // highlights flash (no effort)

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        let content = buffer_content(&term);
        assert!(content.contains("Switch model"));
        assert!(
            content.contains("deepseek-v4-pro ◆"),
            "reasoning models get a ◆"
        );
        assert!(
            !content.contains("effort:"),
            "no effort row when a non-reasoning model is highlighted"
        );

        // Highlight the reasoning model → the effort row appears.
        app.select_model_picker(crate::console::SelectionDirection::Next);
        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        let content = buffer_content(&term);
        assert!(content.contains("effort:"));
        assert!(content.contains("high") && content.contains("max"));
    }

    #[test]
    fn compact_tool_args_shows_bare_cwd_relative_path() {
        let cwd = PathBuf::from("/home/u/ignis");
        // Absolute path under cwd -> relative, bare (no key=, no quotes).
        assert_eq!(
            compact_tool_args(r#"{"path":"/home/u/ignis/src/console/render.rs"}"#, &cwd),
            "src/console/render.rs"
        );
        // Leading "<cwd-name>/" prefix dropped: ignis/src/main.rs -> src/main.rs.
        assert_eq!(
            compact_tool_args(r#"{"path":"ignis/src/main.rs"}"#, &cwd),
            "src/main.rs"
        );
        // Non-path string args show the value only (quoted), never the param name.
        assert_eq!(
            compact_tool_args(r#"{"pattern":"fn main"}"#, &cwd),
            "\"fn main\""
        );
    }

    #[test]
    fn format_context_is_compact() {
        assert_eq!(format_context(131_072), "128K"); // binary
        assert_eq!(format_context(262_144), "256K"); // binary (kimi-k2.6)
        assert_eq!(format_context(200_000), "200K"); // decimal (claude)
        assert_eq!(format_context(1_048_576), "1M"); // binary
        assert_eq!(format_context(1_000_000), "1M"); // decimal
        assert_eq!(format_context(2_000_000), "2M");
        assert_eq!(format_context(1_300_000), "1.3M");
    }

    #[test]
    fn render_welcome_banner() {
        // The welcome banner is committed once to scrollback at launch.
        let app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("/home/test"),
        );
        let text: String = welcome_lines(&app)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("ignis"), "should show app name");
        assert!(
            text.contains("Your AI coding agent"),
            "should show welcome message"
        );
        assert!(text.contains("test/model"), "should show provider/model");
        assert!(text.contains("/home/test"), "should show cwd");
    }

    #[test]
    fn render_shows_user_message() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks.push(UIBlock::User("Hello".to_string()));

        let term = render_blocks(&app, 80, 24);

        let content = buffer_content(&term);
        assert!(content.contains("Hello"), "should show user text");
        assert!(
            content.contains('👤'),
            "user turn should carry the emoji prefix"
        );
    }

    #[test]
    fn render_shows_assistant_message() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks
            .push(UIBlock::Assistant("Code block".to_string()));

        let term = render_blocks(&app, 80, 24);

        let content = buffer_content(&term);
        assert!(content.contains("Code block"), "should show assistant text");
        assert!(
            !content.contains("Ignis"),
            "assistant label should be removed"
        );
    }

    #[test]
    fn render_shows_tool_block() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks.push(UIBlock::Tool(ToolCallEntry {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"main.rs"}"#.to_string(),
            status: ToolStatus::Success("file content".to_string()),
            started_at: std::time::Instant::now(),
            elapsed_ms: 42,
        }));

        let term = render_blocks(&app, 80, 24);

        let content = buffer_content(&term);
        assert!(content.contains("read_file"), "should show tool name");
        assert!(content.contains("file content"), "should show tool output");
    }

    #[test]
    fn render_edit_file_shows_diff_lines() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks.push(UIBlock::Tool(ToolCallEntry {
            id: "c".to_string(),
            name: "edit_file".to_string(),
            arguments: r#"{"path":"src/x.rs"}"#.to_string(),
            status: ToolStatus::Success("- let x = 1;\n+ let x = 2;".to_string()),
            started_at: std::time::Instant::now(),
            elapsed_ms: 5,
        }));

        let term = render_blocks(&app, 100, 24);

        let content = buffer_content(&term);
        assert!(content.contains("- let x = 1;"), "should show removed line");
        assert!(content.contains("+ let x = 2;"), "should show added line");
        assert!(
            !content.contains("Edited file"),
            "old message should be gone"
        );
    }

    #[test]
    fn diff_lines_have_solid_background_and_syntax_colors() {
        let mut app = App::new(
            "p".to_string(),
            "m".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks.push(UIBlock::Tool(ToolCallEntry {
            id: "c".to_string(),
            name: "edit_file".to_string(),
            arguments: r#"{"path":"src/x.rs"}"#.to_string(),
            status: ToolStatus::Success("- let x = 1;\n+ let y = vec![2];".to_string()),
            started_at: std::time::Instant::now(),
            elapsed_ms: 5,
        }));

        let term = render_blocks(&app, 100, 24);
        let buf = term.backend().buffer();

        let mut add_bg = false;
        let mut del_bg = false;
        let mut add_fg = std::collections::HashSet::new();
        for cell in buf.content.iter() {
            if cell.bg == DIFF_ADD_BG {
                add_bg = true;
                add_fg.insert(cell.fg);
            }
            if cell.bg == DIFF_DEL_BG {
                del_bg = true;
            }
        }
        assert!(add_bg, "added line must have a solid green background");
        assert!(del_bg, "removed line must have a solid red background");
        // Multiple foreground colors on the added line ⇒ syntax highlighting is on.
        assert!(
            add_fg.len() > 1,
            "added line should be syntax-highlighted (multiple colors), got {add_fg:?}"
        );
    }

    #[test]
    fn long_diff_line_stays_on_one_row() {
        // Regression: truncation appended `…` past the width, making the line one
        // cell too wide so its solid bg wrapped onto a second row.
        let mut app = App::new(
            "p".to_string(),
            "m".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        let long =
            "+ let x = a_really_long_identifier_that_far_exceeds_the_narrow_terminal_width = 1;";
        app.blocks.push(UIBlock::Tool(ToolCallEntry {
            id: "c".to_string(),
            name: "edit_file".to_string(),
            arguments: r#"{"path":"src/x.rs"}"#.to_string(),
            status: ToolStatus::Success(long.to_string()),
            started_at: std::time::Instant::now(),
            elapsed_ms: 1,
        }));
        let term = render_blocks(&app, 40, 24); // narrow → forces truncation
        let buf = term.backend().buffer();
        let mut rows = std::collections::HashSet::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.get(x, y).bg == DIFF_ADD_BG {
                    rows.insert(y);
                }
            }
        }
        assert_eq!(
            rows.len(),
            1,
            "one added line must occupy one row, got {rows:?}"
        );
    }

    #[test]
    fn render_tool_output_has_no_literal_tabs() {
        // Regression: tab-separated tool output (e.g. list_dir "dir\t4096\tname")
        // reaching ratatui as a literal \t desyncs the terminal layout and
        // garbles the screen. The renderer must expand tabs first.
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks.push(UIBlock::Tool(ToolCallEntry {
            id: "c".to_string(),
            name: "list_dir".to_string(),
            arguments: r#"{"path":"."}"#.to_string(),
            status: ToolStatus::Success("dir\t4096\t.claude\nfile\t512\tREADME.md".to_string()),
            started_at: std::time::Instant::now(),
            elapsed_ms: 2,
        }));

        let term = render_blocks(&app, 100, 24);

        let content = buffer_content(&term);
        assert!(
            !content.contains('\t'),
            "no literal tab may reach the buffer"
        );
        assert!(content.contains(".claude"), "directory name should render");
    }

    #[test]
    fn render_shows_session_picker() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        // Add a dummy block so we exit the welcome-screen branch
        app.blocks.push(UIBlock::User("trigger picker".to_string()));
        app.session_picker = Some(SessionPicker {
            sessions: vec![
                crate::session::SessionMeta {
                    id: "alpha".to_string(),
                    message_count: 3,
                    last_modified: 1234567890,
                    preview: "first prompt".to_string(),
                    start_dir: None,
                },
                crate::session::SessionMeta {
                    id: "beta".to_string(),
                    message_count: 5,
                    last_modified: 1234567891,
                    preview: "second prompt".to_string(),
                    start_dir: None,
                },
            ],
            selected: 0,
        });

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("Sessions"), "should show picker title");
        assert!(content.contains("alpha"), "should list session alpha");
        assert!(content.contains("beta"), "should list session beta");
    }

    #[test]
    fn render_idle_has_no_keybinding_bar() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        // The permanent keybinding hint bar is gone; the input placeholder shows.
        assert!(
            !content.contains("ctrl+u"),
            "keybinding bar should be removed"
        );
        assert!(
            content.contains("Type a message"),
            "should show input prompt"
        );
    }

    #[test]
    fn render_loading_line_shows_thinking() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.mode = Mode::Thinking;

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("Thinking"), "should show loading status");
        assert!(
            content.contains("ctrl+c to interrupt"),
            "should show interrupt hint while busy",
        );
    }

    #[test]
    fn render_footer_shows_model_dir_and_context() {
        let mut app = App::new(
            "openai".to_string(),
            "gpt-4".to_string(),
            "work".to_string(),
            PathBuf::from("/tmp"),
        );

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(
            content.contains("openai/gpt-4"),
            "footer should show provider/model"
        );
        assert!(content.contains("/tmp"), "footer should show cwd");
        assert!(content.contains("tok"), "footer should show token count");
        assert!(content.contains("(0%)"), "footer should show context %");
    }

    #[test]
    fn render_loading_shows_live_token_stats_when_streaming() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.mode = Mode::Thinking;
        app.stream_start = Some(std::time::Instant::now());
        app.stream_chars = 400; // ~100 estimated output tokens
        app.last_usage = Some(crate::Usage {
            input_tokens: 2000,
            ..Default::default()
        });

        let mut term = test_terminal(100, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(
            content.contains("↓ 100 tok"),
            "should show live output tokens"
        );
        assert!(
            content.contains("↑ 2.0k"),
            "should show real input/context tokens"
        );
    }

    #[test]
    fn render_input_with_wide_chars_does_not_panic() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        // Mixed CJK + ASCII, cursor mid-string on a char boundary.
        app.input = "中文a测试".to_string();
        app.cursor = "中文a".len();

        let mut term = test_terminal(80, 24);
        // Would panic on a non-char-boundary slice if cursor math were byte-naive.
        term.draw(|f| draw(f, &mut app)).unwrap();

        // (ratatui pads the trailing cell of a wide glyph with a space, so the
        // CJK chars don't appear contiguously — assert each is present.)
        let content = buffer_content(&term);
        assert!(content.contains("中"), "should render wide chars");
        assert!(content.contains("试"), "should render the full input");
    }

    #[test]
    fn wrapped_blocks_render_all_turns() {
        // Long wrapping turns all become scrollback lines; the latest turn is
        // present (the terminal's own scrollback handles viewing earlier ones).
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        for i in 0..6 {
            app.blocks.push(UIBlock::Assistant(format!(
                "Block {i}: {}",
                "word ".repeat(40)
            )));
        }
        app.blocks.push(UIBlock::User("FINAL_MARKER".to_string()));

        let term = render_blocks(&app, 40, 80);
        let content = buffer_content(&term);
        assert!(
            content.contains("FINAL_MARKER"),
            "the latest turn must render"
        );
        assert!(content.contains("Block 0"), "earlier turns must render too");
    }

    #[test]
    fn long_lines_wrap_with_hanging_indent() {
        // Regression: wrapped continuation rows must keep the line's indent, not
        // fall back to column 0 (ragged left margin).
        let block = UIBlock::Assistant("alpha bravo charlie ".repeat(8).trim().to_string());
        let lines = block_lines(&block, 0, &PathBuf::from("."), 40);
        let body: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .filter(|t| !t.trim().is_empty())
            .collect();
        assert!(
            body.len() > 1,
            "the long line should wrap into multiple rows"
        );
        for (i, row) in body.iter().enumerate() {
            assert!(row.starts_with("  "), "row {i} lost its indent: {row:?}");
        }
    }

    #[test]
    fn resumed_tool_call_renders_as_block_not_raw_json() {
        // Regression (bug 2a): a resumed tool result must render as a tool block,
        // not the raw persisted {"result":…,"is_error":…} JSON.
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("/tmp"),
        );
        let messages = vec![
            crate::Message {
                role: "user".to_string(),
                content: Some("list".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            crate::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![crate::ToolCall {
                    id: "call_1".to_string(),
                    r#type: "function".to_string(),
                    function: crate::ToolCallFunction {
                        name: "list_dir".to_string(),
                        arguments: r#"{"path":"."}"#.to_string(),
                    },
                }]),
            },
            crate::Message {
                role: "tool".to_string(),
                // \t here is escaped in the JSON string, as it is on disk.
                content: Some(r#"{"result":"dir\t4096\t.claude","is_error":false}"#.to_string()),
                reasoning_content: None,
                name: Some("list_dir".to_string()),
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
            },
        ];
        app.render_session_history("s".to_string(), messages);

        let term = render_blocks(&app, 100, 24);

        let content = buffer_content(&term);
        assert!(
            content.contains("list_dir"),
            "tool name should show in a block"
        );
        assert!(content.contains(".claude"), "tool result should render");
        assert!(!content.contains("is_error"), "raw JSON must not leak");
        assert!(!content.contains('\t'), "tabs must be expanded");
    }

    #[test]
    fn resume_picker_hides_prior_conversation() {
        // Regression (bug 2b): while the resume picker is open, the prior
        // conversation must not be shown.
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("/tmp"),
        );
        app.blocks
            .push(UIBlock::User("PRE_RESUME_MESSAGE".to_string()));
        app.session_picker = Some(SessionPicker {
            sessions: vec![crate::session::SessionMeta {
                id: "alpha".to_string(),
                message_count: 1,
                last_modified: 1,
                preview: "hi".to_string(),
                start_dir: None,
            }],
            selected: 0,
        });

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("Sessions"), "picker should render");
        assert!(content.contains("alpha"), "picker should list sessions");
        assert!(
            !content.contains("PRE_RESUME_MESSAGE"),
            "prior conversation must be hidden during resume"
        );
    }

    #[test]
    fn render_session_history_shows_reasoning_content() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("/tmp"),
        );
        let messages = vec![
            crate::Message {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            crate::Message {
                role: "assistant".to_string(),
                content: Some("hi there".to_string()),
                reasoning_content: Some("let me think".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        app.render_session_history("default".to_string(), messages);
        let term = render_blocks(&app, 80, 24);
        let content = buffer_content(&term);
        assert!(content.contains("hello"), "should show user message");
        assert!(
            content.contains("hi there"),
            "should show assistant message"
        );
    }

    #[test]
    fn render_session_history_with_only_reasoning() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("/tmp"),
        );
        let messages = vec![
            crate::Message {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            crate::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: Some("deep reasoning here".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        app.render_session_history("default".to_string(), messages);
        let term = render_blocks(&app, 80, 24);
        let content = buffer_content(&term);
        println!("content: {:?}", content);
        assert!(
            content.contains("deep reasoning here"),
            "should show reasoning content"
        );
    }
}
