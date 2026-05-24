use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};
use std::path::Path;
use unicode_width::UnicodeWidthStr;

use super::app::{App, Mode, SessionPicker, ToolCallEntry, ToolStatus, UIBlock};
use super::markdown::render_md_block;
use super::{
    format_duration, sanitize, truncate, ACCENT, BG, BORDER, BORDER_ACTIVE, GREEN, MAUVE, RED,
    SPINNERS, SUBTEXT, SURFACE, SURFACE_2, TEXT, TEXT_DIM, YELLOW,
};

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let size = f.size();
    f.render_widget(Block::default().style(Style::default().bg(BG)), size);

    // Input box grows with its line count (Ctrl/Cmd+J inserts newlines).
    let input_text_lines = app.input.split('\n').count().max(1) as u16;
    let input_height = (input_text_lines + 2).clamp(3, 10);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),               // messages (borderless)
            Constraint::Length(1),            // loading status (above input)
            Constraint::Length(input_height), // input
            Constraint::Length(1),            // footer: dir · model · context%
        ])
        .split(size);

    draw_messages(f, layout[0], app);
    draw_slash_suggestions(f, layout[0], app);
    draw_loading(f, layout[1], app);
    draw_input(f, layout[2], app);
    draw_footer(f, layout[3], app);
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
    } else {
        let label = match app.mode {
            Mode::Thinking => "Thinking…",
            Mode::ToolRunning => "Running tool…",
            Mode::Idle => "",
        };
        if label.is_empty() {
            Line::from("")
        } else {
            Line::from(vec![
                Span::styled(format!("  {} ", app.spinner()), Style::default().fg(ACCENT)),
                Span::styled(label, Style::default().fg(SUBTEXT)),
                Span::styled(
                    format!("  {}  ·  ctrl+c to interrupt", app.elapsed_str()),
                    Style::default().fg(TEXT_DIM),
                ),
            ])
        }
    };
    f.render_widget(Paragraph::new(line).style(Style::default().bg(BG)), area);
}

/// Status footer under the input: working dir (left) and model + context % (right).
pub(crate) fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let right_str = format!(
        " {}/{}  ·  {}% ctx ",
        app.provider,
        app.model,
        app.context_pct()
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

pub(crate) fn draw_messages(f: &mut Frame, area: Rect, app: &mut App) {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(picker) = &app.session_picker {
        // Resume view: show only the picker, never the prior conversation.
        render_session_picker(&mut lines, picker);
    } else if app.blocks.is_empty() {
        // Welcome screen
        lines.push(Line::from(""));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled("🔥 ", Style::default()),
            Span::styled(
                "ignis",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Your AI coding agent, right in the terminal.",
            Style::default().fg(SUBTEXT),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  Provider  ", Style::default().fg(TEXT_DIM)),
            Span::styled(
                format!("{}/{}", app.provider, app.model),
                Style::default().fg(TEXT),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Directory ", Style::default().fg(TEXT_DIM)),
            Span::styled(format!("{}", app.cwd.display()), Style::default().fg(TEXT)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Type a prompt below to get started.",
            Style::default().fg(TEXT_DIM),
        )));
    } else {
        for (bi, block) in app.blocks.iter().enumerate() {
            match block {
                UIBlock::User(text) => {
                    // No "You" label — a 👤 prefix on the first line marks the
                    // user turn; the prompt is bold to stand out from replies.
                    lines.push(Line::from(""));
                    for (i, l) in text.lines().enumerate() {
                        let prefix = if i == 0 { "👤 " } else { "   " };
                        lines.push(Line::from(vec![
                            Span::styled(prefix, Style::default().fg(ACCENT)),
                            Span::styled(
                                sanitize(l),
                                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    }
                }
                UIBlock::Assistant(text) => {
                    // Skip empty assistant placeholders (tool-only turns, or
                    // before the first streamed delta) so we don't render a
                    // blank gap. No "Ignis" label — replies render as plain
                    // (unprefixed) markdown, distinct from the 👤 user turn.
                    if text.is_empty() {
                        continue;
                    }
                    lines.push(Line::from(""));
                    let is_active = app.current_chunk_idx == Some(bi);
                    let md_lines = render_md_block(text, is_active);
                    lines.extend(md_lines);
                }
                UIBlock::Tool(entry) => {
                    render_tool_block(&mut lines, entry, app.tick, &app.cwd);
                }
            }
        }
    }

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false });

    // Scroll bounds in *rendered* rows: lines word-wrap, so the visible height
    // is line_count(width), not the logical line count — otherwise auto-scroll
    // under-shoots and the last wrapped rows hide behind the input box.
    let visible = area.height;
    let total = paragraph.line_count(area.width) as u16;
    app.max_scroll = total.saturating_sub(visible);
    if !app.user_scrolled {
        app.scroll = app.max_scroll;
    }
    app.scroll = app.scroll.min(app.max_scroll);

    f.render_widget(paragraph.scroll((app.scroll, 0)), area);

    // Scrollbar
    if app.max_scroll > 0 {
        let mut sb_state =
            ScrollbarState::new(app.max_scroll as usize).position(app.scroll as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃"),
            area,
            &mut sb_state,
        );
    }

    // "Scroll to bottom" indicator
    if app.user_scrolled && app.scroll < app.max_scroll {
        let indicator = Paragraph::new(Line::from(vec![Span::styled(
            " ↓ new content below ",
            Style::default().fg(BG).bg(ACCENT),
        )]));
        let ind_area = Rect {
            x: area.x + area.width.saturating_sub(24),
            y: area.y + area.height.saturating_sub(2),
            width: 22,
            height: 1,
        };
        f.render_widget(indicator, ind_area);
    }
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

pub(crate) fn render_tool_block(
    lines: &mut Vec<Line<'static>>,
    entry: &ToolCallEntry,
    tick: u64,
    cwd: &Path,
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
            // edit_file returns a git-style diff: show the whole hunk with
            // red/green +/- coloring. Other tools get a compact 3-line preview.
            let is_diff = entry.name == "edit_file";
            let max = if is_diff { 30 } else { 3 };
            for sl in out.lines().take(max) {
                let display = truncate(&sanitize(sl), 200);
                let line_style = if is_diff && display.starts_with('+') {
                    Style::default().fg(GREEN)
                } else if is_diff && display.starts_with('-') {
                    Style::default().fg(RED)
                } else {
                    Style::default().fg(TEXT_DIM)
                };
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(color)),
                    Span::styled(display, line_style),
                ]));
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
    let active = app.mode == Mode::Idle;
    let border_color = if active { BORDER_ACTIVE } else { BORDER };

    let content = if app.input.is_empty() {
        if active {
            Text::from(Span::styled(
                "Type a message…",
                Style::default().fg(TEXT_DIM),
            ))
        } else {
            Text::from("")
        }
    } else {
        // Build one Line per newline-separated row (a Span with embedded "\n"
        // does not wrap, so we split explicitly).
        Text::from(
            app.input
                .split('\n')
                .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(TEXT))))
                .collect::<Vec<_>>(),
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(SURFACE_2))
        .title(Span::styled(
            if active { " > " } else { " … " },
            Style::default().fg(if active { ACCENT } else { TEXT_DIM }),
        ));

    let p = Paragraph::new(content).block(block);
    f.render_widget(p, area);

    if active {
        // Place the cursor at its (row, col) within the multi-line input.
        // `cursor` is a byte offset; the column is the *display width* of the
        // current row up to it (wide CJK glyphs span two cells), matching how
        // ratatui lays the text out.
        let before = &app.input[..app.cursor];
        let row = before.matches('\n').count() as u16;
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = UnicodeWidthStr::width(&app.input[line_start..app.cursor]) as u16;
        f.set_cursor(area.x + 1 + col, area.y + 1 + row);
    }
}

pub(crate) fn draw_slash_suggestions(f: &mut Frame, messages_area: Rect, app: &App) {
    if app.mode != Mode::Idle {
        return;
    }

    let suggestions = app.slash_suggestions();
    if suggestions.is_empty() {
        return;
    }

    let height = (suggestions.len() as u16).min(4).saturating_add(2);
    if messages_area.height <= height + 1 {
        return;
    }

    let width = messages_area.width.saturating_sub(4).min(54);
    let area = Rect {
        x: messages_area.x + 2,
        y: messages_area.y + messages_area.height.saturating_sub(height + 1),
        width,
        height,
    };

    let mut lines = Vec::new();
    for (idx, suggestion) in suggestions.iter().take(4).enumerate() {
        let selected = idx == app.slash_selection.min(suggestions.len() - 1);
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER_ACTIVE))
        .style(Style::default().bg(SURFACE))
        .title(Span::styled(
            " commands ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ==========================================
// Render Tests
// ==========================================

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
    fn render_welcome_screen_when_empty() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("/home/test"),
        );
        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("ignis"), "should show app name");
        assert!(
            content.contains("Your AI coding agent"),
            "should show welcome message"
        );
        assert!(content.contains("test/model"), "should show provider/model");
        assert!(content.contains("/home/test"), "should show cwd");
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

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

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

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

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

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

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

        let mut term = test_terminal(100, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("- let x = 1;"), "should show removed line");
        assert!(content.contains("+ let x = 2;"), "should show added line");
        assert!(
            !content.contains("Edited file"),
            "old message should be gone"
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

        let mut term = test_terminal(100, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

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
        assert!(content.contains("% ctx"), "footer should show context %");
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
    fn last_line_stays_visible_when_content_wraps() {
        // Regression (bug 1): scroll bounds must count wrapped rows, not logical
        // lines. With long wrapping blocks, auto-scroll-to-bottom must still
        // reveal the most recent turn instead of clipping it behind the input.
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

        let mut term = test_terminal(40, 16);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(
            content.contains("FINAL_MARKER"),
            "auto-scroll must reveal the latest line despite wrapping"
        );
        assert!(app.max_scroll > 0, "wrapped content should be scrollable");
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
                tool_calls: Some(vec![crate::types::ToolCall {
                    id: "call_1".to_string(),
                    r#type: "function".to_string(),
                    function: crate::types::ToolCallFunction {
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

        let mut term = test_terminal(100, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

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
        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
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
        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        let content = buffer_content(&term);
        println!("content: {:?}", content);
        assert!(
            content.contains("deep reasoning here"),
            "should show reasoning content"
        );
    }
}
