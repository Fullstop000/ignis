//! Console rendering — entry point. Owns the top-level frame `draw`,
//! the inline viewport sizing (`live_height`), and the split-pane render
//! path for `ask_user` pickers that carry previews. Everything else is
//! delegated to one of the submodules below.
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::console::app::{App, Mode};
use crate::console::{BG, BORDER, TEXT_DIM};

pub(crate) mod blocks;
pub(crate) mod pickers;
pub(crate) mod tool_block;
pub(crate) mod widgets;

// Re-export per-frame primitives callers still reference by their old
// `console::render::*` path so this split is a pure file move from outside.
pub(crate) use blocks::{block_height, block_lines, render_block_into, welcome_lines};
pub(crate) use pickers::{
    render_mcp_picker, render_model_picker, render_session_picker, render_skill_picker,
};
pub(crate) use widgets::{
    draw_footer, draw_input, draw_loading, draw_queued, draw_slash_suggestions,
    queued_region_height, MAX_SLASH_ROWS,
};

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
            super::inline_picker::render_inline_picker(&mut lines, picker, size.width);
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
            Constraint::Length(4), // header section: divider + blank + title + question
            Constraint::Min(4),    // middle (split)
            Constraint::Length(2), // footer section
        ])
        .split(size);

    // Header section (full width)
    let header_lines = super::inline_picker::header_lines(picker, outer[0].width);
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

    let left_lines = super::inline_picker::options_pane_lines(picker, middle[0].width);
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
#[cfg(test)]
mod queue_render_tests {
    use super::*;
    use crate::console::app::{App, Mode};
    use crate::console::render::widgets::{queued_hint, slash_window_start};
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
    use crate::console::app::{SessionPicker, ToolCallEntry, ToolStatus, UIBlock};
    use crate::console::render::tool_block::compact_tool_args;
    use crate::console::{format_context, DIFF_ADD_BG, DIFF_DEL_BG};
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
        let opt = |p: &str, m: &str, l: &[&str]| crate::llm::ModelOption {
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
            compact_tool_args(
                "read_file",
                r#"{"path":"/home/u/ignis/src/console/render.rs"}"#,
                &cwd
            ),
            "src/console/render.rs"
        );
        // Leading "<cwd-name>/" prefix dropped: ignis/src/main.rs -> src/main.rs.
        assert_eq!(
            compact_tool_args("read_file", r#"{"path":"ignis/src/main.rs"}"#, &cwd),
            "src/main.rs"
        );
        // Non-path string args show the value only (quoted), never the param name.
        assert_eq!(
            compact_tool_args("grep", r#"{"pattern":"fn main"}"#, &cwd),
            "\"fn main\""
        );
    }

    #[test]
    fn compact_tool_args_edit_create_file_show_only_path() {
        let cwd = PathBuf::from("/home/u/ignis");
        // edit_file with large old_string/new_string -> only the file_path renders.
        let edit_args = r#"{"file_path":"ignis/src/console/app.rs",
            "old_string":"                AgentRequest::ReloadConfigAndRebuildSomething(very long)",
            "new_string":"                AgentRequest::ReloadConfigAndRebuildSomethingElse(also long)"}"#;
        assert_eq!(
            compact_tool_args("edit_file", edit_args, &cwd),
            "src/console/app.rs"
        );
        // create_file with a big content blob -> only the file_path renders.
        let create_args = r#"{"file_path":"ignis/src/tools/foo.rs",
            "content":"pub fn foo() { /* lots of text here that would otherwise dominate */ }"}"#;
        assert_eq!(
            compact_tool_args("create_file", create_args, &cwd),
            "src/tools/foo.rs"
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
        // Off mode → no badge.
        assert!(
            !content.contains("HANDS-FREE") && !content.contains(" AFK "),
            "Off mode should render no mode badge"
        );
    }

    #[test]
    fn render_footer_hands_free_badge_is_peach() {
        use crate::console::colors::PEACH;
        use crate::permissions::{runtime::PermissionState, Mode as PermMode};

        let mut app = App::new(
            "openai".to_string(),
            "gpt-4".to_string(),
            "work".to_string(),
            PathBuf::from("/tmp"),
        );
        app.permissions = Some(PermissionState::new(PermMode::HandsFree));

        let mut term = test_terminal(120, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(
            content.contains("HANDS-FREE"),
            "footer should show HANDS-FREE badge under HandsFree mode"
        );

        // Verify the badge cells are peach (not the default SUBTEXT color).
        let buf = term.backend().buffer();
        let mut peach_cells = 0;
        for cell in buf.content.iter() {
            if cell.fg == PEACH {
                peach_cells += 1;
            }
        }
        assert!(
            peach_cells >= "HANDS-FREE".len(),
            "expected ≥{} peach cells for the badge, got {}",
            "HANDS-FREE".len(),
            peach_cells
        );
    }

    #[test]
    fn render_footer_fully_unattended_badge_is_red() {
        use crate::console::colors::RED;
        use crate::permissions::{runtime::PermissionState, Mode as PermMode};

        let mut app = App::new(
            "openai".to_string(),
            "gpt-4".to_string(),
            "work".to_string(),
            PathBuf::from("/tmp"),
        );
        app.permissions = Some(PermissionState::new(PermMode::FullyUnattended));

        let mut term = test_terminal(120, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(
            content.contains(" AFK "),
            "footer should show AFK badge under FullyUnattended mode"
        );

        let buf = term.backend().buffer();
        let mut red_cells = 0;
        for cell in buf.content.iter() {
            if cell.fg == RED {
                red_cells += 1;
            }
        }
        assert!(
            red_cells >= "AFK".len(),
            "expected ≥{} red cells for the badge, got {}",
            "AFK".len(),
            red_cells
        );
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
