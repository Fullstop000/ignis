//! Console rendering — entry point. Owns the top-level frame `draw`,
//! the band-height calculation (`band_height`), the in-app transcript
//! scroller, and the split-pane render path for `ask_user` pickers that
//! carry previews. Everything else is delegated to one of the submodules
//! below.
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::console::app::App;
use crate::console::{BG, BORDER, TEXT_DIM};

pub(crate) mod blocks;
pub(crate) mod pickers;
pub(crate) mod tool_block;
pub(crate) mod widgets;

// Re-export per-frame primitives callers still reference by their old
// `console::render::*` path so this split is a pure file move from outside.
pub(crate) use blocks::{block_lines, welcome_lines};
pub(crate) use pickers::{
    render_mcp_picker, render_model_picker, render_session_picker, render_skill_picker,
};
pub(crate) use widgets::{
    draw_footer, draw_input, draw_loading, draw_queued, draw_slash_suggestions,
    queued_region_height, MAX_SLASH_ROWS,
};

/// Height (rows) of the bottom band — status line + queued strip + slash
/// suggestions + input box + footer. Independent of the transcript above it.
pub(crate) fn band_height(app: &App, term_rows: u16) -> u16 {
    let cap = term_rows.saturating_sub(1).max(3);
    let input_h = input_height(app, cap);
    let sugg = app.slash_suggestions();
    let sugg_h = if !sugg.is_empty() {
        (sugg.len() as u16).min(MAX_SLASH_ROWS)
    } else {
        0
    };
    let queued_h = queued_region_height(app);
    (1 + queued_h + sugg_h + input_h + 1).min(cap)
}

/// Input box height (incl. borders), growing with newline-separated lines.
fn input_height(app: &App, cap: u16) -> u16 {
    let lines = app.input.split('\n').count().max(1) as u16;
    (lines + 2).clamp(3, cap.saturating_sub(2).max(3))
}

/// Whether any picker is currently open (tool-initiated `ask_user` or a
/// slash-command picker). When one is, the inline viewport grows to give it
/// room above the band.
fn picker_open(app: &App) -> bool {
    app.inline_picker.is_some()
        || app.model_picker.is_some()
        || app.session_picker.is_some()
        || app.skill_picker.is_some()
        || app.mcp_picker.is_some()
}

/// Height (rows) of the inline viewport: just the band in the common case, or
/// (nearly) the whole terminal when a picker is open so it has room above the
/// band. The runner rebuilds the `Terminal` whenever this changes — which, with
/// a fixed band, is only on picker open/close or multi-line input growth.
pub(crate) fn viewport_height(app: &App, term_rows: u16) -> u16 {
    let cap = term_rows.saturating_sub(1).max(3);
    if picker_open(app) {
        cap
    } else {
        band_height(app, term_rows).min(cap)
    }
}

/// Blit pre-wrapped `lines` (one `Line` per visual row — `block_lines` already
/// wraps to width) into the `insert_before` scratch buffer, one row each. This
/// is the inline path that pushes finalized blocks into native scrollback.
pub(crate) fn render_block_into(buf: &mut Buffer, lines: &[Line]) {
    let area = buf.area;
    for (i, line) in lines.iter().enumerate() {
        let y = area.y.saturating_add(i as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        buf.set_line(area.x, y, line, area.width);
    }
}

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let size = f.size();
    f.render_widget(Block::default().style(Style::default().bg(BG)), size);

    // Inline layout: `size` IS the viewport. With no picker the band fills it
    // entirely (the conversation is in native scrollback above). With a picker
    // open the viewport is grown by `viewport_height`, and the picker takes the
    // top while the band stays pinned at the bottom.
    let band_h = if picker_open(app) {
        band_height(app, size.height)
    } else {
        size.height
    };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(band_h)])
        .split(size);
    let body_area = outer[0];
    let band_area = outer[1];

    // Body area: tool-initiated pickers (ask_user / permission / connect /
    // afk) anchor to the BOTTOM of the body — same convention Claude Code
    // and Codex use — with the transcript still visible above. Sized to the
    // picker's natural height, capped at 70% so context behind it stays in
    // view. Slash-command pickers (model/session/skill/mcp) continue to take
    // the full body since they're entered intentionally and benefit from the
    // room.
    if app.inline_picker.is_some() {
        // Compute the bottom picker slice in a scope holding only an
        // immutable borrow, so render_transcript can take `&mut app` after.
        // The picker takes exactly its natural height (clamped to body) and
        // whatever's left becomes transcript context above — no proactive
        // share cap. Capping was clipping the input/footer of tiny pickers
        // (e.g. a 7-row text-input picker on an 8-row body) even when the
        // full body could have shown them.
        let (picker_area, above) = {
            let picker = app.inline_picker.as_ref().expect("checked above");
            let desired = super::inline_picker::picker_height(picker, body_area.width);
            let h = desired.min(body_area.height).max(1);
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(h)])
                .split(body_area);
            (split[1], split[0])
        };
        if above.height > 0 {
            render_transcript(f, above, app);
        }
        let picker = app.inline_picker.as_ref().expect("still set");
        if picker.is_reviewing() {
            let mut lines: Vec<Line> = Vec::new();
            super::inline_picker::render_review(&mut lines, picker, picker_area.width);
            f.render_widget(
                Paragraph::new(Text::from(lines))
                    .style(Style::default().bg(BG))
                    .wrap(Wrap { trim: false }),
                picker_area,
            );
        } else if picker.has_any_preview() {
            render_inline_picker_split(f, picker_area, picker);
        } else {
            let mut lines: Vec<Line> = Vec::new();
            super::inline_picker::render_inline_picker(
                &mut lines,
                picker,
                picker_area.width,
                picker_area.height as usize,
            );
            f.render_widget(
                Paragraph::new(Text::from(lines))
                    .style(Style::default().bg(BG))
                    .wrap(Wrap { trim: false }),
                picker_area,
            );
        }
    } else if let Some(picker) = &app.model_picker {
        let max_rows = (body_area.height as usize).saturating_sub(4).max(1);
        let mut lines: Vec<Line> = Vec::new();
        render_model_picker(&mut lines, picker, &app.model_options, max_rows);
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false }),
            body_area,
        );
    } else if let Some(picker) = &app.session_picker {
        let max_rows = (body_area.height as usize).saturating_sub(3).max(1);
        let mut lines: Vec<Line> = Vec::new();
        render_session_picker(&mut lines, picker, max_rows);
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false }),
            body_area,
        );
    } else if let Some(picker) = &app.skill_picker {
        let max_rows = (body_area.height as usize).saturating_sub(3).max(1);
        let mut lines: Vec<Line> = Vec::new();
        render_skill_picker(&mut lines, picker, app.skills.as_deref(), max_rows);
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false }),
            body_area,
        );
    } else if let Some(picker) = &app.mcp_picker {
        let max_rows = (body_area.height as usize).saturating_sub(3).max(1);
        let mut lines: Vec<Line> = Vec::new();
        render_mcp_picker(&mut lines, picker, app.mcp.as_deref(), max_rows);
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false }),
            body_area,
        );
    } else {
        render_transcript(f, body_area, app);
    }

    // Band: laid out from the top of `band_area` down. Order is the same as
    // the legacy inline band — status, queued, slash, input, footer.
    let input_h = input_height(app, band_area.height);
    let sugg = app.slash_suggestions();
    let sugg_h = if !sugg.is_empty() {
        band_area
            .height
            .saturating_sub(input_h + 2)
            .min((sugg.len() as u16).min(MAX_SLASH_ROWS))
    } else {
        0
    };
    let queued_h = queued_region_height(app);
    let band_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(queued_h),
            Constraint::Length(sugg_h),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .split(band_area);

    draw_loading(f, band_layout[0], app);
    if queued_h > 0 {
        draw_queued(f, band_layout[1], app);
    }
    if sugg_h > 0 {
        draw_slash_suggestions(f, band_layout[2], app);
    }
    draw_input(f, band_layout[3], app);
    draw_footer(f, band_layout[4], app);
}

/// Largest scroll offset that still shows the last line of `total` in a
/// `visible`-row viewport: `total - visible` (0 when everything fits). Shared
/// by `render_transcript`, the mouse wheel, PgDn, and `End` so they all agree
/// on "the bottom".
pub(crate) fn natural_max_offset(total: usize, visible: usize) -> usize {
    total.saturating_sub(visible)
}

/// Render the in-app transcript into `area`. Honors `app.auto_follow`
/// (recomputes scroll_offset to the natural bottom) and `app.scroll_offset`
/// (sticky position when the user has scrolled up). No "N more lines" hints —
/// the window is exactly the visible slice.
fn render_transcript(f: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 {
        return;
    }
    let total = app.transcript.len();
    let visible = area.height as usize;
    // Record the real visible-row count so PgDn / the mouse wheel can detect
    // "at the natural bottom" and re-enable auto-follow.
    app.transcript_visible_rows = visible;
    let max_offset = natural_max_offset(total, visible);
    // Auto-follow recomputes offset each frame; manual scroll keeps the
    // user's offset, clamped to the current max in case content shrunk.
    if app.auto_follow || app.scroll_offset > max_offset {
        app.scroll_offset = max_offset;
    }
    if total == 0 {
        return;
    }

    let start = app.scroll_offset.min(total);
    let end = (start + visible).min(total);
    let slice: Vec<Line> = app.transcript[start..end].to_vec();

    f.render_widget(
        Paragraph::new(Text::from(slice))
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        area,
    );
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

    let left_lines = super::inline_picker::options_pane_lines(
        picker,
        middle[0].width,
        middle[0].height as usize,
    );
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

    /// Regression: a long `ask_user` trace once re-wrapped at render while
    /// being counted as a single row, pushing the newest transcript line off
    /// the bottom — it read as "the running status bar covers the chat
    /// history". With the trace wrapped at commit time, the newest line stays
    /// visible. Exercises the real block_lines → transcript → render path.
    #[test]
    fn long_ask_user_trace_keeps_newest_line_visible() {
        use crate::console::app::{ToolCallEntry, ToolStatus, UIBlock};
        use ratatui::{backend::TestBackend, text::Line, Terminal};
        use std::time::Instant;

        // Viewport SHORTER than the transcript so auto-follow pins to the
        // bottom — the condition under which an over-wide line's render-time
        // re-wrap pushes the newest row off the bottom.
        let (width, height) = (40u16, 4u16);
        let mut a = app();
        for i in 0..6 {
            a.commit_transcript_lines(vec![Line::from(format!("ctx{i}"))]);
        }
        let result = serde_json::json!({
            "answers": [{
                "question": "PR #127 green. Two feat: commits since v0.34.0. \
                             Add to [Unreleased], or bump to v0.35.0 and cut a release?",
                "answer": "Bump and cut a release"
            }]
        })
        .to_string();
        let block = UIBlock::Tool(ToolCallEntry {
            id: "1".into(),
            name: "ask_user".into(),
            arguments: "{}".into(),
            status: ToolStatus::Success(result),
            started_at: Instant::now(),
            elapsed_ms: 0,
        });
        let lines = blocks::block_lines(&block, 0, std::path::Path::new("/tmp"), width);
        a.commit_transcript_lines(lines);
        a.commit_transcript_lines(vec![Line::from("NEWEST_SENTINEL")]);

        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            let area = f.size();
            super::render_transcript(f, area, &mut a);
        })
        .unwrap();
        let rendered: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            rendered.contains("NEWEST_SENTINEL"),
            "newest transcript line was clipped behind the band:\n{rendered}"
        );
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
    fn picker_window_returns_start_end_around_selection() {
        use crate::console::render::widgets::picker_window;
        // sel sits past the window → start moves up so sel is the last row.
        assert_eq!(picker_window(9, 5, 20), (5, 10));
        // sel near the top → start stays at 0, end clamps to visible.
        assert_eq!(picker_window(2, 5, 20), (0, 5));
        // fewer items than visible → (0, len) — no scrolling needed.
        assert_eq!(picker_window(2, 8, 5), (0, 5));
        // empty list returns an empty window.
        assert_eq!(picker_window(0, 5, 0), (0, 0));
    }

    #[test]
    fn auto_follow_pins_view_to_bottom() {
        // App starts with auto_follow=true, scroll_offset=0. After several
        // lines are committed, the renderer pins scroll_offset to the natural
        // bottom = `total - visible` (no hints reserve a row anymore).
        let mut a = app();
        for i in 0..20 {
            a.commit_transcript_lines(vec![Line::from(format!("line {i}"))]);
        }
        // Simulate a render: visible=5, total=20 → max_offset = 20 - 5 = 15.
        let max_offset = natural_max_offset(a.transcript.len(), 5);
        if a.auto_follow {
            a.scroll_offset = max_offset;
        }
        assert_eq!(a.scroll_offset, 15);
        assert!(a.auto_follow);
    }

    #[test]
    fn scroll_up_disables_auto_follow_when_offset_moves() {
        let mut a = app();
        for i in 0..30 {
            a.commit_transcript_lines(vec![Line::from(format!("line {i}"))]);
        }
        a.scroll_offset = 20;
        a.scroll_transcript_up(8);
        assert_eq!(a.scroll_offset, 12);
        assert!(!a.auto_follow, "real scroll must release auto-follow");
        // Walking further up clamps at 0, never underflows.
        a.scroll_transcript_up(100);
        assert_eq!(a.scroll_offset, 0);
        assert!(!a.auto_follow);
    }

    #[test]
    fn scroll_up_at_top_preserves_auto_follow() {
        // codex P2: PgUp at offset 0 (already at the top, or before the
        // transcript grew past the viewport) is a no-op; it must NOT silently
        // flip auto_follow off — that would un-pin the view from the bottom
        // when later content arrives.
        let mut a = app();
        a.commit_transcript_lines(vec![Line::from("a"), Line::from("b")]);
        assert_eq!(a.scroll_offset, 0);
        assert!(a.auto_follow);
        a.scroll_transcript_up(10);
        assert_eq!(a.scroll_offset, 0);
        assert!(a.auto_follow, "no-move PgUp must keep follow on");
    }

    #[test]
    fn new_session_clears_transcript_and_resets_scroll() {
        // codex P3: switching sessions used to leave the previous session's
        // rendered lines in `transcript`, since only `blocks`/`committed`
        // got reset. New session output then appeared *below* the stale tail.
        let mut a = app();
        a.commit_transcript_lines(vec![Line::from("old line 1"), Line::from("old line 2")]);
        a.scroll_offset = 1;
        a.auto_follow = false;

        a.start_new_session("sess-new".to_string());

        // Only the "Started new session" notice is in `blocks` — `transcript`
        // is reset until the runner flushes the new session's blocks.
        assert!(a.transcript.is_empty());
        assert_eq!(a.scroll_offset, 0);
        assert!(a.auto_follow);
        assert_eq!(a.session_id, "sess-new");
    }

    #[test]
    fn scroll_down_to_bottom_resumes_auto_follow() {
        let mut a = app();
        for i in 0..30 {
            a.commit_transcript_lines(vec![Line::from(format!("line {i}"))]);
        }
        a.scroll_offset = 5;
        a.auto_follow = false;
        // visible=10, total=30 → natural max_offset = 30 - 10 = 20.
        // PgDn(18) from offset 5 → next=23, which exceeds 20, so we snap to
        // 20 and re-enable auto_follow.
        a.scroll_transcript_down(18, 10);
        assert_eq!(a.scroll_offset, 20);
        assert!(a.auto_follow);
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
    use crate::console::app::{Mode, SessionPicker, ToolCallEntry, ToolStatus, UIBlock};
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
    fn render_reasoning_shows_header_and_dim_color() {
        use crate::console::colors::TEXT_DIM;

        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks
            .push(UIBlock::Reasoning("considering the options".to_string()));

        let term = render_blocks(&app, 80, 24);
        let content = buffer_content(&term);
        assert!(
            content.contains("✻ Thinking"),
            "reasoning block must render the ✻ Thinking header"
        );
        assert!(
            content.contains("considering the options"),
            "reasoning body must render"
        );

        // The header + body should both render in TEXT_DIM; count dim cells
        // to prove the styling actually applied (the renderer could silently
        // fall through to default styling without this).
        let buf = term.backend().buffer();
        let dim_cells = buf.content.iter().filter(|c| c.fg == TEXT_DIM).count();
        assert!(
            dim_cells >= "✻ Thinking".len() + "considering the options".len(),
            "expected ≥{} dim cells, got {}",
            "✻ Thinking".len() + "considering the options".len(),
            dim_cells
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
                crate::cli::sessions::SessionRecord {
                    session_id: "alpha".to_string(),
                    project_slug: "p".to_string(),
                    started_at: Some(1_735_787_045),
                    last_modified: Some(1_735_787_045),
                    agent_messages: 3,
                    user_queries: 1,
                    ..Default::default()
                },
                crate::cli::sessions::SessionRecord {
                    session_id: "beta".to_string(),
                    project_slug: "p".to_string(),
                    started_at: Some(1_735_873_445),
                    last_modified: Some(1_735_873_445),
                    agent_messages: 5,
                    user_queries: 2,
                    ..Default::default()
                },
            ],
            selected: 0,
            mode: crate::console::app::SessionPickerMode::List,
            current_session_id: "alpha".to_string(),
            projects_dir: std::path::PathBuf::from("/tmp"),
        });

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("Sessions"), "should show picker title");
        assert!(content.contains("STARTED"), "should show columns header");
        assert!(content.contains("2025-01-"), "should render row timestamps");
        assert!(content.contains("▸"), "should mark current session with ▸");
        assert!(
            content.contains("details"),
            "footer should advertise → details"
        );
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
    fn render_footer_omits_update_segment_when_no_notice() {
        let mut app = App::new(
            "openai".to_string(),
            "gpt-4".to_string(),
            "work".to_string(),
            PathBuf::from("/tmp"),
        );
        // No update_notice set.
        let mut term = test_terminal(120, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        let content = buffer_content(&term);
        assert!(
            !content.contains("new version available"),
            "footer must be clean when update_notice is None"
        );
    }

    #[test]
    fn render_footer_shows_update_segment_when_notice_set() {
        use crate::cli::upgrade::UpdateNotice;
        use crate::console::colors::YELLOW;

        let mut app = App::new(
            "openai".to_string(),
            "gpt-4".to_string(),
            "work".to_string(),
            PathBuf::from("/tmp"),
        );
        app.update_notice = Some(UpdateNotice {
            current: "0.30.0".to_string(),
            latest_tag: "v0.31.0".to_string(),
        });

        let mut term = test_terminal(120, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        let content = buffer_content(&term);
        assert!(
            content.contains("new version available"),
            "footer must show notice when update_notice is set"
        );
        assert!(
            content.contains("ignis upgrade"),
            "footer must mention the upgrade command"
        );

        // Verify the segment is rendered in yellow (not the default dim).
        let buf = term.backend().buffer();
        let yellow_cells = buf.content.iter().filter(|c| c.fg == YELLOW).count();
        assert!(
            yellow_cells >= "new version available".len(),
            "expected ≥{} yellow cells for the notice, got {}",
            "new version available".len(),
            yellow_cells
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
                created_at_ms: None,
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
                created_at_ms: None,
            },
            crate::Message {
                role: "tool".to_string(),
                // \t here is escaped in the JSON string, as it is on disk.
                content: Some(r#"{"result":"dir\t4096\t.claude","is_error":false}"#.to_string()),
                reasoning_content: None,
                name: Some("list_dir".to_string()),
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
                created_at_ms: None,
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
            sessions: vec![crate::cli::sessions::SessionRecord {
                session_id: "alpha".to_string(),
                project_slug: "p".to_string(),
                started_at: Some(1_735_787_045),
                last_modified: Some(1),
                ..Default::default()
            }],
            selected: 0,
            mode: crate::console::app::SessionPickerMode::List,
            current_session_id: "alpha".to_string(),
            projects_dir: std::path::PathBuf::from("/tmp"),
        });

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("Sessions"), "picker should render");
        assert!(
            !content.contains("PRE_RESUME_MESSAGE"),
            "prior conversation must be hidden while picker is open"
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
                created_at_ms: None,
            },
            crate::Message {
                role: "assistant".to_string(),
                content: Some("hi there".to_string()),
                reasoning_content: Some("let me think".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
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
    fn inline_picker_anchors_to_bottom_with_transcript_above() {
        use crate::console::picker::{PickerOption, PickerQuestion};
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("/tmp"),
        );
        // Fill scrollback so something is pinned just above the picker via
        // auto-follow — that's the regression we're guarding: tool-initiated
        // pickers used to wipe the transcript and take the full body.
        for i in 0..40 {
            app.commit_transcript_lines(vec![Line::from(format!("FILLER_{i}"))]);
        }
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.inline_picker = Some(crate::console::inline_picker::InlinePickerState::new(
            crate::console::picker::PickerRequest {
                questions: vec![PickerQuestion {
                    question: "PICKER_QUESTION".into(),
                    kind: "permission".into(),
                    header: "Allow?".into(),
                    multi_select: false,
                    allow_other: false,
                    text_input: false,
                    mask: false,
                    options: vec![
                        PickerOption {
                            label: "Approve once".into(),
                            description: "Run this command and continue.".into(),
                            preview: None,
                        },
                        PickerOption {
                            label: "Deny".into(),
                            description: "Reject this tool call.".into(),
                            preview: None,
                        },
                    ],
                }],
                reply: tx,
            },
        ));
        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let w = buf.area.width as usize;
        let h = buf.area.height as usize;
        let row_text = |y: usize| -> String {
            buf.content[y * w..(y + 1) * w]
                .iter()
                .map(|c| c.symbol())
                .collect()
        };
        let picker_row = (0..h)
            .find(|y| row_text(*y).contains("PICKER_QUESTION"))
            .expect("picker question must render");
        let filler_row = (0..h)
            .find(|y| row_text(*y).contains("FILLER_"))
            .expect("transcript must still be visible above the picker");
        assert!(
            filler_row < picker_row,
            "transcript row {filler_row} should sit ABOVE picker row {picker_row}",
        );
        assert!(
            picker_row >= h / 3,
            "picker should anchor toward bottom (question at row {picker_row} of {h})",
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
                created_at_ms: None,
            },
            crate::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: Some("deep reasoning here".to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
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
