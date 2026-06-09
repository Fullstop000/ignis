//! Block rendering primitives — convert in-memory transcript blocks
//! (`UIBlock::User`, `Assistant`, `Tool`) into wrapped `Line`s for the in-app
//! transcript buffer.
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::path::Path;
use unicode_width::UnicodeWidthChar;

use crate::console::app::{App, UIBlock};
use crate::console::markdown::render_md_block;
use crate::console::{sanitize, ACCENT, MAUVE, SUBTEXT, TEXT, TEXT_DIM};

use super::tool_block::{ask_user_resume_trace, render_tool_block};

// Local alias so the wrap_line column-width math reads as `…::width(…)`
// (the rest of the file uses both Char- and Str-level widths).

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
            // Special left-rail style: a mauve "▌" bar runs down every visual row
            // (including wrapped continuations) so the user's own words read as one
            // accented block. No background fill — keeps scrollback select/copy
            // clean. Mauve deliberately differs from the blue composer border and
            // the green/yellow/red tool bullets.
            for l in text.lines() {
                let content = Line::from(Span::styled(
                    sanitize(l),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ));
                // Wrap the text alone to the rail-inset width, then draw the rail
                // on each resulting row for a continuous bar.
                for row in wrap_line(&content, width.saturating_sub(2), 0) {
                    let mut spans = vec![Span::styled("▌ ", Style::default().fg(MAUVE))];
                    spans.extend(row.spans);
                    lines.push(Line::from(spans));
                }
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
            lines.extend(reasoning_full_lines(text, width));
        }
        UIBlock::Tool(entry) => {
            // The `ask_user` tool has its own purpose-built scrollback line
            // (`inline_picker::trace_lines`); rendering the generic tool block
            // would dump verbose JSON args+result twice. We still want a record
            // on session resume — the live trace is ephemeral — so build a
            // compact trace from the persisted entry instead.
            if entry.name == "ask_user" {
                // Wrap to width like every other block: render_transcript slices
                // one logical line per row, so an over-wide trace line would
                // re-wrap at render and clip the transcript bottom behind the band.
                for line in ask_user_resume_trace(entry) {
                    let indent = leading_space_cols(&line);
                    lines.extend(wrap_line(&line, width, indent));
                }
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

/// Live rolling-preview window: how many reasoning body rows show while a
/// thought streams in collapsed mode. The region is this + 1 header row.
pub(crate) const REASONING_PREVIEW_LINES: usize = 3;

/// Below this line count a collapsed thought isn't worth truncating — it
/// commits in full instead of a lead + "(N more lines)" hint.
const REASONING_COLLAPSE_MIN: usize = 4;

/// The full chain-of-thought block (expanded look, and the resume render):
/// dim+italic `✻ Thinking` header over plain dim body lines (no markdown —
/// reasoning is prose, markdown would invent heading/list noise).
pub(crate) fn reasoning_full_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if text.is_empty() {
        return lines;
    }
    lines.push(Line::from(""));
    lines.push(reasoning_header(None));
    for raw_line in text.lines() {
        let line = Line::from(Span::styled(
            format!("  {}", sanitize(raw_line)),
            Style::default().fg(TEXT_DIM),
        ));
        lines.extend(wrap_line(&line, width, 2));
    }
    lines
}

/// The collapsed committed form (Claude-Code style): the lead line of the
/// thought + a dim `… (N more lines, ctrl+o to expand)` hint. Short thoughts
/// (≤ `REASONING_COLLAPSE_MIN` lines) commit in full — nothing to hide.
pub(crate) fn reasoning_collapsed_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let all: Vec<&str> = text.lines().collect();
    if all.len() <= REASONING_COLLAPSE_MIN {
        return reasoning_full_lines(text, width);
    }
    let lead = all
        .iter()
        .find(|l| !l.trim().is_empty())
        .copied()
        .unwrap_or("");
    let mut lines = vec![Line::from(""), reasoning_header(None)];
    let lead_line = Line::from(Span::styled(
        format!("  {}", sanitize(lead)),
        Style::default().fg(TEXT_DIM),
    ));
    lines.extend(wrap_line(&lead_line, width, 2));
    lines.push(Line::from(Span::styled(
        format!("  … ({} more lines, ctrl+o to expand)", all.len() - 1),
        Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
    )));
    lines
}

/// The transient live preview while a thought streams in collapsed mode: the
/// `✻ Thinking` header with a spinner over a fixed `REASONING_PREVIEW_LINES`-row
/// window of the *last* reasoning rows, rolling as more text arrives. Always
/// returns exactly `1 + REASONING_PREVIEW_LINES` rows (front-padded with blanks)
/// so the region's height never jitters mid-thought.
pub(crate) fn reasoning_preview_lines(text: &str, spinner: &str, width: u16) -> Vec<Line<'static>> {
    let mut out = vec![reasoning_header(Some(spinner))];
    // Wrap a generous tail, then keep the last N visual rows.
    let tail = tail_lines(text, REASONING_PREVIEW_LINES * 2);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for raw in tail.lines() {
        let line = Line::from(Span::styled(
            format!("  {}", sanitize(raw)),
            Style::default().fg(TEXT_DIM),
        ));
        rows.extend(wrap_line(&line, width, 2));
    }
    let start = rows.len().saturating_sub(REASONING_PREVIEW_LINES);
    let mut body = rows.split_off(start);
    while body.len() < REASONING_PREVIEW_LINES {
        body.insert(0, Line::from(""));
    }
    out.extend(body);
    out
}

/// `✻ Thinking` marker, dim+italic, optionally trailed by a spinner frame.
fn reasoning_header(spinner: Option<&str>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "✻ Thinking",
        Style::default().fg(TEXT_DIM).add_modifier(Modifier::ITALIC),
    )];
    if let Some(s) = spinner {
        spans.push(Span::styled(format!(" {s}"), Style::default().fg(ACCENT)));
    }
    Line::from(spans)
}

/// The last `n` newline-delimited lines of `text` (reverse scan, O(n)). Fewer
/// than `n` lines → the whole text. Mirrors kimi's `_tail_lines`.
fn tail_lines(text: &str, n: usize) -> &str {
    let mut pos = text.len();
    for _ in 0..n {
        match text[..pos].rfind('\n') {
            Some(i) => pos = i,
            None => return text,
        }
    }
    &text[pos + 1..]
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
        if word_w <= width {
            // Word fits on a row — soft-wrap to a fresh row if it won't fit here.
            let floor = if first_row { 0 } else { indent_cols };
            if cur_w + word_w > width && cur_w > floor {
                rows.push(std::mem::take(&mut cur));
                first_row = false;
                cur = indent_row();
                cur_w = indent_cols;
            }
            cur.extend_from_slice(word);
            cur_w += word_w;
        } else {
            // Word is wider than a whole row — a space-less run (CJK text, a long
            // URL). Hard-break it at the column boundary so no line exceeds
            // `width`; otherwise it re-wraps at render and clips the transcript.
            for &cell in word {
                let cw = UnicodeWidthChar::width(cell.0).unwrap_or(0);
                let floor = if first_row { 0 } else { indent_cols };
                if cur_w + cw > width && cur_w > floor {
                    rows.push(std::mem::take(&mut cur));
                    first_row = false;
                    cur = indent_row();
                    cur_w = indent_cols;
                }
                cur.push(cell);
                cur_w += cw;
            }
        }
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
/// The startup banner, committed once to the transcript at launch. When no
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::app::{ToolCallEntry, ToolStatus, UIBlock};
    use crate::console::{GREEN, RED, YELLOW};
    use std::time::Instant;
    use unicode_width::UnicodeWidthStr;

    fn line_cols(line: &Line) -> usize {
        line.spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    /// A space-less run wider than a row (CJK text, a long URL) must be
    /// hard-broken so no line exceeds width. Pre-fix `wrap_line` only broke on
    /// ASCII spaces, so CJK — which has none — stayed over-wide, re-wrapped at
    /// render, and clipped the transcript (the bilingual ask_user case).
    #[test]
    fn wrap_line_hard_breaks_spaceless_run() {
        let width: u16 = 10;
        let original = "汉字汉字汉字汉字abcdefghij"; // 16 + 10 = 26 cols, no spaces
        let rows = wrap_line(&Line::from(original), width, 2);
        assert!(rows.len() > 1, "expected the run to break across rows");
        for (i, r) in rows.iter().enumerate() {
            assert!(
                line_cols(r) <= width as usize,
                "row {i} is {} cols, exceeds width {width}",
                line_cols(r),
            );
        }
        let joined: String = rows
            .iter()
            .flat_map(|r| r.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert_eq!(
            joined.replace(' ', ""),
            original,
            "content must round-trip (only synthetic indent spaces added)",
        );
    }

    /// `render_transcript` slices the transcript one logical `Line` per visible
    /// row and renders with `Wrap`. That only holds if every committed line is
    /// ≤ width. A long `ask_user` question must therefore be wrapped at commit
    /// time, not left as one over-wide line that re-wraps (and over-runs the
    /// area) at render — which is what hid chat history behind the status band.
    #[test]
    fn ask_user_trace_lines_fit_width() {
        let width: u16 = 40;
        let result = serde_json::json!({
            "answers": [{
                "question": "PR #127 green. Two feat: commits since v0.34.0. \
                             Add to [Unreleased], or bump to v0.35.0 and cut a release?",
                "answer": "Bump to v0.35.0 and cut a release"
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
        let lines = block_lines(&block, 0, Path::new("/tmp"), width);
        for (i, l) in lines.iter().enumerate() {
            assert!(
                line_cols(l) <= width as usize,
                "ask_user trace line {i} is {} cols, exceeds width {width} — \
                 it will re-wrap at render and clip the transcript bottom",
                line_cols(l),
            );
        }
    }

    fn flatten(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The bullet `●`'s foreground colour (header status colour).
    fn bullet_color(lines: &[Line]) -> Option<ratatui::style::Color> {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.starts_with('●'))
            .and_then(|s| s.style.fg)
    }

    fn tool(name: &str, status: ToolStatus) -> UIBlock {
        UIBlock::Tool(ToolCallEntry {
            id: "1".into(),
            name: name.into(),
            arguments: r#"{"command":"echo hi"}"#.into(),
            status,
            started_at: Instant::now(),
            elapsed_ms: 12,
        })
    }

    /// User messages render the mauve `▌` left-rail on every visual row — never
    /// the old `👤` emoji (which also left a stray wide-char space in scrollback).
    #[test]
    fn user_block_uses_mauve_rail() {
        let block = UIBlock::User("first line\nsecond line".into());
        let lines = block_lines(&block, 0, Path::new("/tmp"), 40);
        let text = flatten(&lines);
        assert!(text.contains("▌ "), "expected the rail prefix: {text:?}");
        assert!(!text.contains('👤'), "emoji prefix must be gone: {text:?}");
        assert!(text.contains("first line") && text.contains("second line"));
        // Every rail span is mauve.
        let rails: Vec<_> = lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.content == "▌ ")
            .collect();
        assert_eq!(rails.len(), 2, "one rail per literal line: {text:?}");
        assert!(rails.iter().all(|s| s.style.fg == Some(MAUVE)));
    }

    /// The rail must run down *every wrapped continuation row*, not just the
    /// first row of each literal line — a long message wraps to several visual
    /// rows and each one starts with the mauve `▌ `.
    #[test]
    fn user_rail_spans_wrapped_continuations() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let block = UIBlock::User(long.into());
        let width = 24; // forces several wrapped rows
        let lines = block_lines(&block, 0, Path::new("/tmp"), width);
        // Skip the leading blank spacer (an empty span trims to ""); keep real rows.
        let content_rows: Vec<_> = lines
            .iter()
            .filter(|l| !flatten(&[(*l).clone()]).trim().is_empty())
            .collect();
        assert!(
            content_rows.len() >= 3,
            "expected the line to wrap: {content_rows:?}"
        );
        for row in &content_rows {
            let first = &row.spans[0];
            assert_eq!(first.content, "▌ ", "row missing rail: {row:?}");
            assert_eq!(first.style.fg, Some(MAUVE));
            let cols: usize = row
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                cols <= width as usize,
                "railed row {cols} exceeds width {width}"
            );
        }
    }

    /// Tool blocks render the Claude-Code gutter: a `●` header + `╰` result, the
    /// raw tool name, and none of the old `┌ │ └` box drawing.
    #[test]
    fn tool_block_renders_cc_gutter_no_box() {
        let block = tool("bash", ToolStatus::Success("hello\nworld".into()));
        let lines = block_lines(&block, 0, Path::new("/tmp"), 60);
        let joined = flatten(&lines);
        assert!(
            lines.iter().any(|l| l
                .spans
                .first()
                .map(|s| s.content.starts_with('●'))
                .unwrap_or(false)),
            "header bullet: {joined:?}"
        );
        assert!(joined.contains("bash"), "raw tool name kept: {joined:?}");
        assert!(joined.contains('╰'), "gutter connector: {joined:?}");
        for ch in ['┌', '└', '│'] {
            assert!(
                !joined.contains(ch),
                "box char {ch} must be gone: {joined:?}"
            );
        }
        assert_eq!(bullet_color(&lines), Some(GREEN), "success bullet is green");
    }

    /// The bullet colour encodes status: yellow pending, green success, red error.
    #[test]
    fn tool_block_bullet_color_tracks_status() {
        let pending = block_lines(&tool("grep", ToolStatus::Pending), 0, Path::new("/tmp"), 60);
        assert_eq!(bullet_color(&pending), Some(YELLOW));
        // Pending shows a spinner on the `╰` line, not in the bullet.
        assert!(flatten(&pending).contains("running…"));

        let err = block_lines(
            &tool("grep", ToolStatus::Error("boom".into())),
            0,
            Path::new("/tmp"),
            60,
        );
        assert_eq!(bullet_color(&err), Some(RED));
        assert!(flatten(&err).contains("boom"));
    }

    /// A tool that succeeds with no output still gets a gutter line so the block
    /// reads as complete rather than a bare dangling header.
    #[test]
    fn tool_block_empty_success_shows_no_output() {
        let lines = block_lines(
            &tool("bash", ToolStatus::Success("   \n".into())),
            0,
            Path::new("/tmp"),
            60,
        );
        let joined = flatten(&lines);
        assert!(joined.contains('╰'), "still has a gutter line: {joined:?}");
        assert!(joined.contains("(no output)"), "{joined:?}");
    }

    #[test]
    fn tail_lines_keeps_last_n() {
        assert_eq!(tail_lines("a\nb\nc\nd", 2), "c\nd");
        // Fewer than n lines → whole text.
        assert_eq!(tail_lines("a\nb", 5), "a\nb");
        // No trailing newline → the partial last line is included.
        assert_eq!(tail_lines("one\ntwo\nthree", 1), "three");
    }

    #[test]
    fn collapsed_reasoning_shows_lead_and_count() {
        let text = (1..=125)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = flatten(&reasoning_collapsed_lines(&text, 80));
        assert!(out.contains("✻ Thinking"), "{out:?}");
        assert!(out.contains("line 1"), "lead line shown: {out:?}");
        assert!(
            !out.contains("line 2"),
            "body hidden when collapsed: {out:?}"
        );
        assert!(
            out.contains("… (124 more lines, ctrl+o to expand)"),
            "hint with hidden count: {out:?}"
        );
    }

    #[test]
    fn short_reasoning_commits_in_full_not_collapsed() {
        // ≤ REASONING_COLLAPSE_MIN lines: nothing to hide, render the whole thing.
        let text = "first\nsecond\nthird";
        let out = flatten(&reasoning_collapsed_lines(text, 80));
        assert!(out.contains("first") && out.contains("third"), "{out:?}");
        assert!(!out.contains("more lines"), "no truncation hint: {out:?}");
    }

    #[test]
    fn reasoning_preview_is_fixed_height_with_rolling_tail() {
        let text = "alpha\nbravo\ncharlie\ndelta\necho";
        let out = reasoning_preview_lines(text, "⠹", 80);
        // Exactly header + REASONING_PREVIEW_LINES rows, always.
        assert_eq!(out.len(), 1 + REASONING_PREVIEW_LINES);
        let joined = flatten(&out);
        assert!(joined.contains("✻ Thinking"), "{joined:?}");
        assert!(joined.contains('⠹'), "spinner present: {joined:?}");
        // Rolling window = the last 3 lines; older ones scrolled off.
        assert!(
            joined.contains("echo") && joined.contains("charlie"),
            "{joined:?}"
        );
        assert!(
            !joined.contains("alpha"),
            "oldest line rolled off: {joined:?}"
        );
    }

    #[test]
    fn reasoning_preview_pads_short_content_to_fixed_height() {
        // One line of thought still reserves the full region (front-padded).
        let out = reasoning_preview_lines("just started", "⠋", 80);
        assert_eq!(out.len(), 1 + REASONING_PREVIEW_LINES);
    }
}
