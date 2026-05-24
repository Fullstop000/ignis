use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};

use super::app::{App, Mode, SessionPicker, ToolCallEntry, ToolStatus, UIBlock};
use super::markdown::render_md_block;
use super::{
    format_duration, truncate, ACCENT, BG, BORDER, BORDER_ACTIVE, GREEN, MAUVE, RED, SPINNERS,
    SUBTEXT, SURFACE, SURFACE_2, TEXT, TEXT_DIM, YELLOW,
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
            Constraint::Length(1),            // header
            Constraint::Min(3),               // messages (borderless)
            Constraint::Length(1),            // loading status (above input)
            Constraint::Length(input_height), // input
        ])
        .split(size);

    draw_header(f, layout[0], app);
    draw_messages(f, layout[1], app);
    draw_slash_suggestions(f, layout[1], app);
    draw_loading(f, layout[2], app);
    draw_input(f, layout[3], app);
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

pub(crate) fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let cwd_str = format!(" {} ", app.cwd.display());
    let header_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(cwd_str.len() as u16)])
        .split(area);

    let left = Line::from(vec![
        Span::styled(" 🔥 ", Style::default()),
        Span::styled(
            "ignis",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{}/{}", app.provider, app.model),
            Style::default().fg(SUBTEXT),
        ),
        Span::styled(
            format!("  {}", app.session_id),
            Style::default().fg(TEXT_DIM),
        ),
    ]);

    let right = Line::from(Span::styled(cwd_str, Style::default().fg(TEXT_DIM)));

    f.render_widget(
        Paragraph::new(left).style(Style::default().bg(SURFACE)),
        header_layout[0],
    );
    f.render_widget(
        Paragraph::new(right)
            .style(Style::default().bg(SURFACE))
            .alignment(ratatui::layout::Alignment::Right),
        header_layout[1],
    );
}

pub(crate) fn draw_messages(f: &mut Frame, area: Rect, app: &mut App) {
    let mut lines: Vec<Line> = Vec::new();

    if app.blocks.is_empty() {
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
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("  ❯ ", Style::default().fg(ACCENT)),
                        Span::styled(
                            "You",
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    for l in text.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("  {}", l),
                            Style::default().fg(TEXT),
                        )));
                    }
                }
                UIBlock::Assistant(text) => {
                    // Skip empty assistant placeholders (e.g. tool-only turns,
                    // or before the first streamed delta) so we don't render a
                    // bare "> Ignis" header with nothing under it.
                    if text.is_empty() {
                        continue;
                    }
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled("  > ", Style::default().fg(MAUVE)),
                        Span::styled(
                            "Ignis",
                            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    let is_active = app.current_chunk_idx == Some(bi);
                    let md_lines = render_md_block(text, is_active);
                    lines.extend(md_lines);
                }
                UIBlock::Tool(entry) => {
                    render_tool_block(&mut lines, entry, app.tick);
                }
            }
        }
        if let Some(picker) = &app.session_picker {
            render_session_picker(&mut lines, picker);
        }
    }

    // Calculate scroll bounds (borderless: full height is visible)
    let visible = area.height;
    let total = lines.len() as u16;
    app.max_scroll = total.saturating_sub(visible);
    if !app.user_scrolled {
        app.scroll = app.max_scroll;
    }
    // Clamp scroll
    app.scroll = app.scroll.min(app.max_scroll);

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text)
        .style(Style::default().bg(BG))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0));
    f.render_widget(paragraph, area);

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

pub(crate) fn render_tool_block(lines: &mut Vec<Line<'static>>, entry: &ToolCallEntry, tick: u64) {
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
            let preview = truncate(err.trim(), 300);
            ("x", RED, preview, elapsed)
        }
    };

    // Parse tool arguments for a compact display
    let args_compact = compact_tool_args(&entry.arguments);

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
            // Show first 3 lines of output
            for sl in out.lines().take(3) {
                let display = truncate(sl, 100);
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(color)),
                    Span::styled(display, Style::default().fg(TEXT_DIM)),
                ]));
            }
            let total_lines = out.lines().count();
            if total_lines > 3 {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(color)),
                    Span::styled(
                        format!("… {} more lines", total_lines - 3),
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

/// Produce a compact arg summary like `path="src/main.rs"` from JSON
pub(crate) fn compact_tool_args(json_str: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return truncate(json_str, 60);
    };
    let Some(obj) = val.as_object() else {
        return truncate(json_str, 60);
    };
    let mut parts = Vec::new();
    for (k, v) in obj {
        let s = match v {
            serde_json::Value::String(s) => {
                let t = truncate(s, 40);
                format!("{}=\"{}\"", k, t)
            }
            serde_json::Value::Number(n) => format!("{}={}", k, n),
            serde_json::Value::Bool(b) => format!("{}={}", k, b),
            _ => {
                let t = truncate(&v.to_string(), 30);
                format!("{}={}", k, t)
            }
        };
        parts.push(s);
    }
    let joined = parts.join(", ");
    truncate(&joined, 80)
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
        let before = &app.input[..app.cursor];
        let row = before.matches('\n').count() as u16;
        let col = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u16;
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
        assert!(content.contains("You"), "should show user label");
        assert!(content.contains("Hello"), "should show user text");
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
        assert!(content.contains("Ignis"), "should show assistant label");
        assert!(content.contains("Code block"), "should show assistant text");
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
    fn render_header_shows_provider_model_session() {
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
            "should show provider/model"
        );
        assert!(content.contains("work"), "should show session id");
        assert!(content.contains("/tmp"), "should show cwd");
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
