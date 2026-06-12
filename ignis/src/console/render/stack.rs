//! A tiny content-driven layout for composing terminal lines without
//! hand-padded magic column widths.
//!
//! Panels build a [`Cell`] per fragment ([`text`]/[`spans`]), lay fragments
//! out left-to-right with [`row`], and content-size shared columns with
//! [`column_width`] + [`pad_right`] — so a key/value list aligns to its widest
//! key instead of a hardcoded `{:<9}`. Output is [`Line<'static>`], so panels
//! keep rendering through the existing `Paragraph` path: no buffer/`Rect`
//! plumbing, no new dependency, single binary unaffected.
//!
//! This is deliberately *not* a flexbox engine. It handles stacked,
//! content-sized terminal panels (the shape ignis actually has today). The
//! moment a panel genuinely needs flex-grow / wrap / a 2-D grid, swap a real
//! layout engine in behind these same helpers.

use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// A measured run of styled spans — one fragment of a line.
pub(crate) struct Cell {
    spans: Vec<Span<'static>>,
    width: usize,
}

impl Cell {
    /// Display width of this cell in terminal columns.
    pub(crate) fn width(&self) -> usize {
        self.width
    }
}

/// A single styled text fragment (the common leaf).
pub(crate) fn text(content: impl Into<String>, style: Style) -> Cell {
    let content = content.into();
    let width = UnicodeWidthStr::width(content.as_str());
    Cell {
        spans: vec![Span::styled(content, style)],
        width,
    }
}

/// A cell from pre-built spans, width measured across them.
pub(crate) fn spans(spans: Vec<Span<'static>>) -> Cell {
    let width = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    Cell { spans, width }
}

/// Pad `cell` on the right with blanks up to `width` columns (no-op if wider).
pub(crate) fn pad_right(mut cell: Cell, width: usize) -> Cell {
    if cell.width < width {
        cell.spans.push(Span::raw(" ".repeat(width - cell.width)));
        cell.width = width;
    }
    cell
}

/// Content width of a column: the widest cell plus a `gutter` of trailing
/// space. This is what replaces a hardcoded field width — add a longer entry
/// and every row re-aligns to it automatically.
pub(crate) fn column_width<'a>(cells: impl IntoIterator<Item = &'a Cell>, gutter: usize) -> usize {
    cells.into_iter().map(Cell::width).max().unwrap_or(0) + gutter
}

/// Lay cells left-to-right into one line.
pub(crate) fn row(cells: impl IntoIterator<Item = Cell>) -> Line<'static> {
    let spans: Vec<Span<'static>> = cells.into_iter().flat_map(|c| c.spans).collect();
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn joined(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn text_measures_display_width() {
        assert_eq!(text("abc", Style::default()).width(), 3);
        // CJK glyphs are two cells wide each.
        assert_eq!(text("添加", Style::default()).width(), 4);
    }

    #[test]
    fn spans_sums_member_widths() {
        let c = spans(vec![Span::raw("ab"), Span::raw("添")]);
        assert_eq!(c.width(), 4);
    }

    #[test]
    fn pad_right_fills_to_width_and_is_idempotent_when_wider() {
        let padded = pad_right(text("hi", Style::default()), 5);
        assert_eq!(padded.width(), 5);
        assert_eq!(joined(&row([padded])), "hi   ");
        // Already wider than target: untouched.
        let wide = pad_right(text("hello", Style::default()), 3);
        assert_eq!(wide.width(), 5);
        assert_eq!(joined(&row([wide])), "hello");
    }

    #[test]
    fn column_width_is_widest_plus_gutter() {
        let cells = vec![text("a", Style::default()), text("abcd", Style::default())];
        assert_eq!(column_width(&cells, 2), 6);
        assert_eq!(column_width(std::iter::empty(), 2), 2);
    }

    #[test]
    fn row_concatenates_cells_left_to_right() {
        let line = row([
            pad_right(text("k", Style::default()), 4),
            text("v", Style::default().fg(Color::Red)),
        ]);
        assert_eq!(joined(&line), "k   v");
    }

    #[test]
    fn key_value_column_aligns_to_widest_key() {
        // Two rows whose keys differ in width align to a shared column —
        // the property that replaces a hardcoded `{:<9}`.
        let keys = [
            text("short", Style::default()),
            text("muchlonger", Style::default()),
        ];
        let w = column_width(&keys, 1);
        let mut out = Vec::new();
        for (k, v) in [("short", "A"), ("muchlonger", "B")] {
            out.push(joined(&row([
                pad_right(text(k, Style::default()), w),
                text(v, Style::default()),
            ])));
        }
        // Values start at the same column in both rows.
        assert_eq!(out[0].find('A'), out[1].find('B'));
    }
}
