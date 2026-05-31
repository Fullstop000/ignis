//! Block rendering primitives — convert in-memory transcript blocks
//! (`UIBlock::User`, `Assistant`, `Tool`) into wrapped `Line`s ready to commit
//! to scrollback via `terminal.insert_before`. Also hosts the line-wrap +
//! buffer-render helpers used both by scrollback flushes and tests.
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};
use std::path::Path;
use unicode_width::UnicodeWidthChar;

use crate::console::app::{App, UIBlock};
use crate::console::markdown::render_md_block;
use crate::console::{sanitize, ACCENT, BG, SUBTEXT, TEXT, TEXT_DIM};

use super::tool_block::{ask_user_resume_trace, render_tool_block};

// Local alias so the wrap_line column-width math reads as `…::width(…)`
// (the rest of the file uses both Char- and Str-level widths).
use unicode_width::UnicodeWidthStr;

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
        UIBlock::Reasoning(text) => {
            if text.is_empty() {
                return lines;
            }
            lines.push(Line::from(""));
            // Header row — the marker line that says "this block is the
            // model's chain-of-thought, not the reply." Dim+italic so it's
            // visually subordinate to the assistant blocks that follow.
            lines.push(Line::from(Span::styled(
                "✻ Thinking",
                Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
            )));
            // Body — plain dim text, no markdown. Reasoning streams are
            // prose-y and rarely contain code fences; rendering them as
            // markdown would create heading/list noise from natural language.
            for raw_line in text.lines() {
                let line = Line::from(Span::styled(
                    format!("  {}", sanitize(raw_line)),
                    Style::default().fg(TEXT_DIM),
                ));
                lines.extend(wrap_line(&line, width, 2));
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
/// The startup banner, committed once to scrollback at launch. When no
/// provider is configured (first-launch / cleared config), the provider line
/// is replaced with a one-line hint that points the user at `/connect`.
pub(crate) fn welcome_lines(app: &App) -> Vec<Line<'static>> {
    let title = Line::from(vec![
        Span::styled("🔥 ", Style::default()),
        Span::styled(
            "ignis",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  Your AI coding agent, right in the terminal.",
            Style::default().fg(SUBTEXT),
        ),
    ]);
    let info_line = if app.provider.is_empty() {
        // First-launch / no-provider mode. Tell the user the one thing they
        // need to do next; don't show a fake provider value.
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled("No provider configured. ", Style::default().fg(TEXT_DIM)),
            Span::styled("Type ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                "/connect",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to pick one and paste your API key.",
                Style::default().fg(TEXT_DIM),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("  Provider  ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                format!("{}/{}", app.provider, app.model),
                Style::default().fg(TEXT),
            ),
            Span::styled("   Directory  ", Style::default().fg(TEXT_DIM)),
            Span::styled(format!("{}", app.cwd.display()), Style::default().fg(TEXT)),
        ])
    };
    vec![Line::from(""), title, info_line, Line::from("")]
}
