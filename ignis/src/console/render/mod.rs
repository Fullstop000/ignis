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

use unicode_width::UnicodeWidthStr;

use crate::console::app::App;
use crate::console::{BG, BORDER, TEXT_DIM};

pub(crate) mod anchor;
pub(crate) mod blocks;
pub(crate) mod layout;
pub(crate) mod pickers;
pub(crate) mod stream_commit;
pub(crate) mod tool_block;
pub(crate) mod widgets;

// Re-export per-frame primitives callers still reference by their old
// `console::render::*` path so this split is a pure file move from outside.
pub(crate) use blocks::{block_lines, reasoning_collapsed_lines, welcome_lines};
// Runner reaches these by the old `render::*` path; `draw` (below) uses the rest.
pub(crate) use layout::{band_height, viewport_height};
use layout::{input_height, picker_open, reasoning_preview_height, MODEL_PICKER_MAX_OPTION_ROWS};
pub(crate) use pickers::{
    render_mcp_picker, render_model_picker, render_session_picker, render_settings_panel,
    render_skill_picker,
};
pub(crate) use widgets::{
    draw_footer, draw_input, draw_loading, draw_queued, draw_slash_suggestions,
    queued_region_height, MAX_SLASH_ROWS,
};

/// Max rows a single `insert_before` may carry at `width` columns.
///
/// `insert_before` builds a `width * height` cell scratch buffer, but
/// `Rect::area()` saturates at `u16::MAX` (65535) and `insert_before` skips
/// `Rect::new`'s aspect-clamp (it builds the area with a struct literal). So a
/// call whose `width * height >= 65536` under-allocates the cell Vec while the
/// buffer area still reports the full height — and `render_block_into`'s
/// `set_style` then runs off the end ("index out of bounds: the len is 65535
/// but the index is 65535"). Splitting a commit so each frame stays at or below
/// this many rows keeps every buffer within the limit. Bites `/resume` of a
/// long transcript, which commits every block in one batch.
pub(crate) fn max_commit_rows(width: u16) -> usize {
    (u16::MAX as usize / (width.max(1) as usize)).max(1)
}

/// Blit pre-wrapped `lines` (one `Line` per visual row — `block_lines` already
/// wraps to width) into the `insert_before` scratch buffer, one row each. This
/// is the inline path that pushes finalized blocks into native scrollback.
pub(crate) fn render_block_into(buf: &mut Buffer, lines: &[Line]) {
    let area = buf.area;
    // Paint the whole buffer with the app background first: insert_before cells
    // otherwise default to the terminal's own bg, leaving the scrollback a
    // different shade than the band. Spans rarely set a bg, so this fill shows
    // through everywhere except explicit code-bg spans.
    buf.set_style(area, Style::default().bg(BG));
    for (i, line) in lines.iter().enumerate() {
        let y = area.y.saturating_add(i as u16);
        if y >= area.y.saturating_add(area.height) {
            break;
        }
        buf.set_line(area.x, y, line, area.width);
    }
    blank_wide_char_continuations(buf);
}

/// Empty the cell that follows each double-width glyph (CJK, emoji).
///
/// `Terminal::insert_before` flushes *every* buffer cell to the backend — it
/// skips the `diff` step that, in a normal draw, drops the blank continuation
/// cell after a wide glyph. The backend then prints that blank `" "` at the
/// column the wide glyph already advanced past, leaving a stray space after
/// every wide char. Clearing the continuation symbol makes the backend print
/// nothing there, so the glyph keeps its natural two columns.
fn blank_wide_char_continuations(buf: &mut Buffer) {
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

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let size = f.size();
    f.render_widget(Block::default().style(Style::default().bg(BG)), size);

    // Inline layout: `size` IS the viewport. With no picker the band fills it
    // entirely (the conversation is in native scrollback above), minus a
    // reasoning preview region when a thought streams collapsed. With a picker
    // open the viewport is grown by `viewport_height`, and the picker takes the
    // top while the band stays pinned at the bottom.
    let preview_h = if picker_open(app) {
        0
    } else {
        reasoning_preview_height(app)
    };
    let band_h = if picker_open(app) {
        band_height(app, size.height)
    } else {
        size.height.saturating_sub(preview_h)
    };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(band_h)])
        .split(size);
    let body_area = outer[0];
    let band_area = outer[1];

    // Body area: a tool-initiated picker (ask_user / permission / connect /
    // afk) anchors to the BOTTOM of the body at its natural height. The area
    // above it stays blank — the conversation is in native scrollback above
    // the whole inline viewport. Slash-command pickers (model/session/skill/
    // mcp) take the full body since they're entered intentionally.
    if let Some(picker) = &app.inline_picker {
        let desired = super::inline_picker::picker_height(picker, body_area.width);
        let h = desired.min(body_area.height).max(1);
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(h)])
            .split(body_area);
        let picker_area = split[1];
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
        // Model picker anchors to the bottom of the body area — like the
        // inline ask_user picker — so the conversation in native scrollback
        // above the TUI is visible. `viewport_height` only grows the TUI by
        // the picker's natural height, not the whole terminal.
        let options = &app.model_options;
        let max_rows = MODEL_PICKER_MAX_OPTION_ROWS;
        let mut lines: Vec<Line> = Vec::new();
        render_model_picker(&mut lines, picker, options, max_rows);
        let desired = lines.len() as u16;
        let h = desired.min(body_area.height);
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(h)])
            .split(body_area);
        let picker_area = split[1];
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false }),
            picker_area,
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
    } else if let Some(panel) = &app.settings_panel {
        let mut lines: Vec<Line> = Vec::new();
        render_settings_panel(&mut lines, panel, app);
        f.render_widget(
            Paragraph::new(Text::from(lines))
                .style(Style::default().bg(BG))
                .wrap(Wrap { trim: false }),
            body_area,
        );
    }
    // No picker → `body_area` is zero-height, unless a thought is streaming in
    // collapsed mode: then it's the `preview_h`-row reasoning preview region,
    // anchored just above the band. The rolling window redraws each frame and is
    // never committed to scrollback (only the one-line breadcrumb is, on finish).
    if preview_h > 0 {
        if let Some(text) = app.live_reasoning() {
            let lines = blocks::reasoning_preview_lines(text, app.spinner(), body_area.width);
            f.render_widget(
                Paragraph::new(Text::from(lines)).style(Style::default().bg(BG)),
                body_area,
            );
        }
    }

    // Band: status … footer. While a picker is open the band is just status +
    // footer (no input box / slash / queued) — the picker above is the input.
    if picker_open(app) {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(band_area);
        draw_loading(f, split[0], app);
        draw_footer(f, split[2], app);
        return;
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
        .border_type(ratatui::widgets::BorderType::Rounded)
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

    /// Regression: a long `ask_user` trace once produced rows wider than the
    /// viewport. Inline rendering blits `block_lines` rows verbatim into
    /// scrollback (no render-time re-wrap), so an over-width row would be
    /// truncated. `block_lines` must therefore hard-break over-wide words so
    /// every row fits the width.
    #[test]
    fn long_ask_user_trace_wraps_within_width() {
        use crate::console::app::{ToolCallEntry, ToolStatus, UIBlock};
        use std::time::Instant;
        use unicode_width::UnicodeWidthStr;

        let width = 40u16;
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
        for line in &lines {
            let cols: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                cols <= width as usize,
                "ask_user trace row exceeds width {width} ({cols} cols): {line:?}"
            );
        }
    }

    #[test]
    fn render_block_into_blanks_wide_char_continuation() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::text::Line;
        // "🔥 x": fire (width 2) at cell 0, its continuation at cell 1, space at
        // 2, 'x' at 3. The continuation must be an EMPTY symbol so insert_before
        // doesn't flush it as a stray space (which would push 'x' to cell 4).
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        super::render_block_into(&mut buf, &[Line::from("🔥 x")]);
        assert_eq!(
            buf.get(1, 0).symbol(),
            "",
            "wide-char continuation must be blank"
        );
        assert_eq!(buf.get(2, 0).symbol(), " ");
        assert_eq!(buf.get(3, 0).symbol(), "x");
    }

    #[test]
    fn max_commit_rows_keeps_insert_before_buffer_in_bounds() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::text::Line;
        // Regression for the `/resume` panic on a long transcript: committing
        // the whole history in one `insert_before(h, ..)` built a
        // `width * h >= 65536` scratch buffer, whose backing Vec saturates at
        // 65535 cells while the area still reports the full size — so
        // `render_block_into`'s `set_style` ran off the end (panic
        // "index out of bounds: the len is 65535 but the index is 65535").
        // The runner now caps each frame at `max_commit_rows(width)`; a
        // full-cap batch must render without panicking.
        for width in [1u16, 64, 80, 120, 200, 256, 1000] {
            let rows = super::max_commit_rows(width);
            assert!(
                rows * width as usize <= u16::MAX as usize,
                "cap overflows at width {width}: {rows} rows"
            );
            assert!(rows >= 1);
            // The runner builds insert_before's area with a struct literal (no
            // `Rect::new` aspect-clamp) — mirror that exactly.
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: rows as u16,
            };
            let mut buf = Buffer::empty(area);
            let lines: Vec<Line> = (0..rows).map(|_| Line::from("x")).collect();
            super::render_block_into(&mut buf, &lines); // must not panic
        }
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
        a.composer.input = "typing".into();
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
        a.composer.input = "x".into(); // hint only
        assert_eq!(queued_region_height(&a), 1);
        a.composer.input.clear();
        a.queue = vec!["one".into(), "two".into()]; // blank + 2 rows + hint
        assert_eq!(queued_region_height(&a), 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::app::{Mode, SessionPicker, ToolCallEntry, ToolStatus, UIBlock};
    use crate::console::format_context;
    use crate::console::render::layout::model_picker_height;
    use crate::console::render::tool_block::compact_tool_args;
    use crate::console::{DIFF_ADD_BG, DIFF_DEL_BG, PEACH, RED, YELLOW};
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

    /// `model_picker_height` is selection-stable: walking `sel` across the
    /// scroll-window boundary or across a reasoning↔non-reasoning boundary
    /// must NOT change the returned height — otherwise the runner rebuilds
    /// the viewport and the user sees a full-screen flicker on every `↓`.
    #[test]
    fn model_picker_height_is_stable_across_navigation() {
        let opt = |p: &str, m: &str, l: &[&str]| crate::llm::ModelOption {
            provider: p.to_string(),
            model: m.to_string(),
            effort_levels: l.iter().map(|s| s.to_string()).collect(),
            context: None,
        };
        // 20 options, mixed effort. Walk sel across the full range — including
        // both the scroll-window boundary at sel=15 and the
        // reasoning↔non-reasoning boundary inside the mix.
        let options: Vec<crate::llm::ModelOption> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    opt("p", &format!("m{i}"), &["high"])
                } else {
                    opt("p", &format!("m{i}"), &[])
                }
            })
            .collect();
        let mut picker = crate::console::app::ModelPicker {
            selected: 0,
            effort_idx: 0,
        };
        let baseline = model_picker_height(&picker, &options);
        for sel in 0..options.len() {
            picker.selected = sel;
            let h = model_picker_height(&picker, &options);
            assert_eq!(
                h, baseline,
                "model_picker_height must not change when sel moves (sel={sel}, h={h}, baseline={baseline})"
            );
        }
        // Also unchanged when effort is cycled (effort doesn't enter the math).
        picker.cycle_effort(crate::console::SelectionDirection::Next, &options);
        assert_eq!(model_picker_height(&picker, &options), baseline);

        // Sanity: the formula handles edge cases.
        // Empty options → 3.
        let empty: Vec<crate::llm::ModelOption> = vec![];
        assert_eq!(model_picker_height(&picker, &empty), 3);
        // 1 option, no effort: blank + header + 1 option + footer = 4.
        let one = vec![opt("p", "m0", &[])];
        assert_eq!(model_picker_height(&picker, &one), 4);
        // 1 option, with effort: 5.
        let one_effort = vec![opt("p", "m0", &["high"])];
        assert_eq!(model_picker_height(&picker, &one_effort), 5);
        // 16 options, no effort, list overflows: 2 + 0 + 1 + 15 + 1 + 1 = 20.
        let sixteen: Vec<crate::llm::ModelOption> =
            (0..16).map(|i| opt("p", &format!("m{i}"), &[])).collect();
        assert_eq!(model_picker_height(&picker, &sixteen), 20);
        // 20 options, with effort, list overflows: 2 + 1 + 1 + 15 + 1 + 1 = 21.
        let twenty: Vec<crate::llm::ModelOption> = (0..20)
            .map(|i| opt("p", &format!("m{i}"), &["high"]))
            .collect();
        assert_eq!(model_picker_height(&picker, &twenty), 21);
    }

    /// `viewport_height` for the model picker must always include the band
    /// height (status + footer) plus the picker's natural height, clamped to
    /// the terminal — never returning 0 in normal use.
    #[test]
    fn viewport_height_clamps_model_picker_to_terminal() {
        let opt = |p: &str, m: &str, l: &[&str]| crate::llm::ModelOption {
            provider: p.to_string(),
            model: m.to_string(),
            effort_levels: l.iter().map(|s| s.to_string()).collect(),
            context: None,
        };
        let options: Vec<crate::llm::ModelOption> = (0..20)
            .map(|i| opt("p", &format!("m{i}"), &["high"]))
            .collect();
        let mut app = App::new(
            "p".to_string(),
            "m0".to_string(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        );
        app.set_model_options(options, None);
        app.show_model_picker();

        // 8-row terminal: ph=21 + bh=2 = 23, clamped to cap=7.
        assert_eq!(viewport_height(&app, 80, 8), 7);
        // 100-row terminal: ph=21 + bh=2 = 23.
        assert_eq!(viewport_height(&app, 80, 100), 23);
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
            content.contains('▌'),
            "user turn should carry the mauve left-rail"
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
    fn render_skill_tool_collapses_body_to_one_line() {
        // The skill tool returns the whole skill body wrapped in <skill name>.
        // The block must show a single "loaded skill instructions" line, never
        // the wrapper or the body (model reads it, not the user).
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        app.blocks.push(UIBlock::Tool(ToolCallEntry {
            id: "s1".to_string(),
            name: "skill".to_string(),
            arguments: r#"{"name":"brainstorming"}"#.to_string(),
            status: ToolStatus::Success(
                "<skill name=\"brainstorming\">\nSECRET_BODY_LINE_ONE\nSECRET_BODY_LINE_TWO\n</skill>"
                    .to_string(),
            ),
            started_at: std::time::Instant::now(),
            elapsed_ms: 3,
        }));

        let term = render_blocks(&app, 80, 24);
        let content = buffer_content(&term);
        assert!(
            content.contains("skill"),
            "header still shows the tool name"
        );
        assert!(
            content.contains("loaded skill instructions"),
            "skill block shows the one-line confirmation"
        );
        assert!(
            !content.contains("SECRET_BODY_LINE_ONE"),
            "the skill body must not spill into the transcript"
        );
        assert!(
            !content.contains("<skill name"),
            "the <skill> wrapper must not render"
        );
        assert!(
            !content.contains("more lines"),
            "no '… N more lines' tail for the collapsed skill block"
        );
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
                    title: "add session title".to_string(),
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
            projects_dir: std::path::PathBuf::from("/tmp"),
        });

        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();

        let content = buffer_content(&term);
        assert!(content.contains("Sessions"), "should show picker title");
        // Two-line rows: a derived title line over a dim meta line.
        assert!(
            content.contains("add session title"),
            "should render the derived title"
        );
        assert!(
            content.contains("(no message yet)"),
            "title-less session falls back to a placeholder"
        );
        assert!(
            content.contains("2025-01-"),
            "meta line shows the timestamp"
        );
        assert!(content.contains("tok"), "meta line shows token count");
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
        app.composer.input = "中文a测试".to_string();
        app.composer.cursor = "中文a".len();

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
    fn input_bar_shows_prompt_glyph() {
        let mut app = App::new(
            "test".to_string(),
            "model".to_string(),
            "default".to_string(),
            PathBuf::from("."),
        );
        // Empty input: the ❯ prompt sits left of the placeholder.
        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(
            buffer_content(&term).contains('❯'),
            "input bar should show the ❯ prompt glyph"
        );

        // With typed text the glyph still leads the line.
        app.composer.input = "hello".to_string();
        app.composer.cursor = app.composer.input.len();
        let mut term = test_terminal(80, 24);
        term.draw(|f| draw(f, &mut app)).unwrap();
        assert!(
            buffer_content(&term).contains("❯ hello"),
            "prompt glyph should prefix the typed input"
        );
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
