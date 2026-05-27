//! Live-band widgets — the status / queue / footer / input / slash-suggestions
//! strip that the inline viewport repaints every frame. Each `draw_*` fn
//! takes a ratatui `Rect` and paints into it. The `queued_*` helpers compute
//! the height the queue strip needs so `live_height` can size the band.
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::console::app::{App, Mode};
use crate::console::{
    format_tokens, sanitize, truncate, ACCENT, BG, BORDER, BORDER_ACTIVE, RED, SUBTEXT, SURFACE,
    SURFACE_2, TEXT, TEXT_DIM, YELLOW,
};

/// Max queued rows shown before collapsing to a "+N more" row.
pub(crate) const MAX_QUEUE_ROWS: usize = 5;
/// Max slash-suggestion rows shown at once; the list scrolls to keep the
/// selected entry visible when there are more (e.g. many skills + `/skills`).
pub(crate) const MAX_SLASH_ROWS: u16 = 8;

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
pub(crate) fn slash_window_start(sel: usize, visible: usize, len: usize) -> usize {
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
