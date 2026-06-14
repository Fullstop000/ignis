//! The text composer (input box), as a self-contained view component.
//!
//! Second panel in the `XProps + impl Widget + From<&App>` pattern (see
//! [`super::footer`] for the first). The composer carries real interaction
//! state — input text, the cursor, paste chips, the queued-count title — so it
//! also surfaces the pattern's one boundary: a ratatui [`Widget`] renders cells
//! only, but the input box also positions the terminal cursor, which is a
//! `Frame`-level operation. So [`ComposerProps::render`] paints the box and the
//! thin [`draw_input`] adapter places the cursor via [`ComposerProps::cursor_pos`]
//! — both reading the same props, no `App` access in either.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Widget},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::console::app::{App, Mode};
use crate::console::composer::PendingPaste;
use crate::console::{ACCENT, BORDER, BORDER_ACTIVE, PEACH, SUBTEXT, SURFACE_2, TEXT, TEXT_DIM};

/// Leftmost prompt glyph that prefixes the first input line (Claude-Code style);
/// continuation lines are indented `PROMPT_W` columns so wrapped text aligns.
const PROMPT: &str = "❯ ";
const PROMPT_W: u16 = 2;

/// Everything the composer renders from — the composer's props.
pub(crate) struct ComposerProps<'a> {
    /// Idle (awaiting your prompt) vs busy (agent running); drives the border
    /// accent and placeholder text.
    pub idle: bool,
    pub input: &'a str,
    /// Byte offset of the cursor within `input` (always a char boundary).
    pub cursor: usize,
    pub pending_pastes: &'a [PendingPaste],
    /// Number of queued messages; renders a `· N queued` title when > 0.
    pub queued: usize,
}

impl<'a> From<&'a App> for ComposerProps<'a> {
    /// Map the `App` god-object down to the composer's props — the only place
    /// the composer touches `App`.
    fn from(app: &'a App) -> Self {
        ComposerProps {
            idle: app.mode == Mode::Idle,
            input: app.composer.input.as_str(),
            cursor: app.composer.cursor,
            pending_pastes: &app.composer.pending_pastes,
            queued: app.queue.len(),
        }
    }
}

impl ComposerProps<'_> {
    /// Cursor column/row *within the text area* (before the border + prompt
    /// offsets the adapter adds). Pure from props so it's unit-testable.
    pub(crate) fn cursor_pos(&self) -> (u16, u16) {
        let before = &self.input[..self.cursor];
        let row = before.matches('\n').count() as u16;
        let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = UnicodeWidthStr::width(&self.input[line_start..self.cursor]) as u16;
        (col, row)
    }
}

impl Widget for ComposerProps<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border_color = if self.idle { BORDER_ACTIVE } else { BORDER };
        let prompt = Span::styled(
            PROMPT,
            Style::default().fg(if self.idle { ACCENT } else { TEXT_DIM }),
        );

        let content = if self.input.is_empty() {
            let placeholder = if self.idle {
                "Type a message…"
            } else {
                "Type your next message…"
            };
            Text::from(Line::from(vec![
                prompt,
                Span::styled(placeholder, Style::default().fg(TEXT_DIM)),
            ]))
        } else {
            Text::from(
                self.input
                    .split('\n')
                    .enumerate()
                    .map(|(i, l)| {
                        let mut line = input_line(l, self.pending_pastes);
                        // Prompt glyph on the first line; align continuation lines.
                        let lead = if i == 0 {
                            prompt.clone()
                        } else {
                            Span::raw("  ")
                        };
                        line.spans.insert(0, lead);
                        line
                    })
                    .collect::<Vec<_>>(),
            )
        };

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(SURFACE_2));
        if self.queued > 0 {
            block = block.title(
                ratatui::widgets::block::Title::from(Span::styled(
                    format!(" · {} queued ", self.queued),
                    Style::default().fg(SUBTEXT),
                ))
                .alignment(ratatui::layout::Alignment::Right),
            );
        }

        Paragraph::new(content).block(block).render(area, buf);
    }
}

/// Render one composer line, painting any paste-chip placeholder in PEACH so it
/// reads as a token instead of literal brackets. Placeholders never contain a
/// newline, so each chip lives within a single rendered line.
fn input_line(line: &str, pastes: &[PendingPaste]) -> Line<'static> {
    if pastes.is_empty() || !line.contains('[') {
        return Line::from(Span::styled(line.to_string(), Style::default().fg(TEXT)));
    }
    // Locate each chip on this line; placeholders are unique so each matches at
    // most once. Sort by start to walk left-to-right.
    let mut ranges: Vec<(usize, usize)> = pastes
        .iter()
        .filter_map(|p| {
            line.find(&p.placeholder)
                .map(|s| (s, s + p.placeholder.len()))
        })
        .collect();
    if ranges.is_empty() {
        return Line::from(Span::styled(line.to_string(), Style::default().fg(TEXT)));
    }
    ranges.sort_by_key(|r| r.0);
    let mut spans: Vec<Span> = Vec::new();
    let mut pos = 0;
    for (start, end) in ranges {
        if start < pos {
            continue; // overlap guard — shouldn't happen with unique chips
        }
        if start > pos {
            spans.push(Span::styled(
                line[pos..start].to_string(),
                Style::default().fg(TEXT),
            ));
        }
        spans.push(Span::styled(
            line[start..end].to_string(),
            Style::default().fg(PEACH),
        ));
        pos = end;
    }
    if pos < line.len() {
        spans.push(Span::styled(
            line[pos..].to_string(),
            Style::default().fg(TEXT),
        ));
    }
    Line::from(spans)
}

/// Render the composer input box into `area`, then position the terminal cursor.
/// Thin adapter so existing callsites stay `draw_input(f, area, app)`; the cell
/// rendering lives in the [`ComposerProps`] component.
pub(crate) fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let props = ComposerProps::from(app);
    // Cursor first — `render_widget` consumes `props`.
    let (col, row) = props.cursor_pos();
    f.render_widget(props, area);
    // Cursor sits just past the prompt glyph; shown whether idle or busy (you
    // can type while the agent works to queue / steer).
    f.set_cursor(area.x + 1 + PROMPT_W + col, area.y + 1 + row);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn paste(placeholder: &str) -> PendingPaste {
        PendingPaste {
            placeholder: placeholder.to_string(),
            content: "…".to_string(),
        }
    }

    /// Render props into a bordered area and return the painted text rows.
    fn render_rows(props: ComposerProps, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        props.render(area, &mut buf);
        (0..h)
            .map(|y| (0..w).map(|x| buf.get(x, y).symbol()).collect::<String>())
            .collect()
    }

    fn base(input: &str) -> ComposerProps<'_> {
        ComposerProps {
            idle: true,
            input,
            cursor: input.len(),
            pending_pastes: &[],
            queued: 0,
        }
    }

    // --- input_line chip painting (pure) ---

    #[test]
    fn input_line_paints_chip_in_peach() {
        let pastes = [paste("[paste#1 3 lines]")];
        let line = input_line("see [paste#1 3 lines] ok", &pastes);
        // Three spans: leading text, the chip, trailing text.
        assert_eq!(line.spans.len(), 3, "spans: {:?}", line.spans);
        assert_eq!(line.spans[0].content, "see ");
        assert_eq!(line.spans[1].content, "[paste#1 3 lines]");
        assert_eq!(line.spans[1].style.fg, Some(PEACH), "chip must be PEACH");
        assert_eq!(line.spans[2].content, " ok");
    }

    #[test]
    fn input_line_plain_when_no_chips() {
        let line = input_line("just text", &[]);
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "just text");
        assert_eq!(line.spans[0].style.fg, Some(TEXT));
    }

    // --- cursor positioning (pure) ---

    #[test]
    fn cursor_pos_tracks_row_and_display_column() {
        let mut props = base("ab\ncde");
        props.cursor = "ab\ncde".len(); // end of second line
        assert_eq!(props.cursor_pos(), (3, 1));
    }

    #[test]
    fn cursor_pos_uses_display_width_for_wide_chars() {
        // "🔥" is two columns wide; cursor after it sits at column 2.
        let mut props = base("🔥");
        props.cursor = "🔥".len();
        assert_eq!(props.cursor_pos(), (2, 0));
    }

    // --- rendered box ---

    #[test]
    fn empty_idle_shows_idle_placeholder() {
        let rows = render_rows(base(""), 40, 3);
        let joined = rows.join("\n");
        assert!(joined.contains("Type a message"), "got: {joined}");
    }

    #[test]
    fn empty_busy_shows_busy_placeholder() {
        let mut props = base("");
        props.idle = false;
        let rows = render_rows(props, 40, 3);
        let joined = rows.join("\n");
        assert!(joined.contains("Type your next message"), "got: {joined}");
    }

    #[test]
    fn renders_input_text_with_prompt_glyph() {
        let rows = render_rows(base("hello"), 40, 3);
        assert!(rows[1].contains("❯ hello"), "got: {:?}", rows[1]);
    }

    #[test]
    fn queue_title_shown_when_queued() {
        let mut props = base("hi");
        props.queued = 2;
        let rows = render_rows(props, 40, 3);
        // Title rides the top border (row 0).
        assert!(rows[0].contains("2 queued"), "top border: {:?}", rows[0]);
    }

    #[test]
    fn no_queue_title_when_empty() {
        let rows = render_rows(base("hi"), 40, 3);
        assert!(!rows[0].contains("queued"), "top border: {:?}", rows[0]);
    }

    // --- behavior preservation through draw_input + From<&App> ---

    #[test]
    fn draw_input_shows_placeholder_via_app() {
        use std::path::PathBuf;
        let app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        let mut term = Terminal::new(TestBackend::new(40, 3)).unwrap();
        term.draw(|f| draw_input(f, f.size(), &app)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Type a message"), "got: {text}");
    }
}
