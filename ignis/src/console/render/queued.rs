//! The queued-prompts strip (queued messages + adaptive hint) shown between the
//! status line and the input box, as a self-contained view component
//! (`XProps + Widget + From<&App>`; see [`super::footer`]). Sizing math
//! (`queued_region_height`) and the hint text (`queued_hint`) stay shared in
//! [`super::widgets`]; this owns only the rendering.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget},
    Frame,
};

use crate::console::app::App;
use crate::console::render::widgets::{queued_hint, MAX_QUEUE_ROWS};
use crate::console::{sanitize, truncate, BG, SUBTEXT, TEXT_DIM};

/// Props for the queued strip: the queued prompts plus the resolved hint line.
pub(crate) struct QueuedProps<'a> {
    pub queue: &'a [String],
    pub hint: Option<String>,
}

impl<'a> From<&'a App> for QueuedProps<'a> {
    fn from(app: &'a App) -> Self {
        QueuedProps {
            queue: &app.queue,
            hint: queued_hint(app),
        }
    }
}

impl Widget for QueuedProps<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines: Vec<Line> = Vec::new();
        if !self.queue.is_empty() {
            lines.push(Line::from(""));
            for text in self.queue.iter().take(MAX_QUEUE_ROWS) {
                lines.push(Line::from(vec![
                    Span::styled("  ↳ ", Style::default().fg(TEXT_DIM)),
                    Span::styled(truncate(&sanitize(text), 72), Style::default().fg(SUBTEXT)),
                ]));
            }
            if self.queue.len() > MAX_QUEUE_ROWS {
                lines.push(Line::from(Span::styled(
                    format!("    +{} more", self.queue.len() - MAX_QUEUE_ROWS),
                    Style::default().fg(TEXT_DIM),
                )));
            }
        }
        if let Some(hint) = self.hint {
            lines.push(Line::from(Span::styled(
                format!("  {}", hint),
                Style::default().fg(TEXT_DIM),
            )));
        }
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(BG))
            .render(area, buf);
    }
}

/// Render the queued-prompts strip into `area`. Thin adapter; work lives in the
/// [`QueuedProps`] component.
pub(crate) fn draw_queued(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(QueuedProps::from(app), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_rows(props: QueuedProps, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        props.render(area, &mut buf);
        (0..h)
            .map(|y| (0..w).map(|x| buf.get(x, y).symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn renders_each_queued_prompt() {
        let queue = vec!["first prompt".to_string(), "second prompt".to_string()];
        let rows = render_rows(
            QueuedProps {
                queue: &queue,
                hint: None,
            },
            60,
            6,
        )
        .join("\n");
        assert!(rows.contains("↳"), "expected arrow rows: {rows}");
        assert!(rows.contains("first prompt"), "got: {rows}");
        assert!(rows.contains("second prompt"), "got: {rows}");
    }

    #[test]
    fn collapses_overflow_to_plus_n_more() {
        let queue: Vec<String> = (0..MAX_QUEUE_ROWS + 3).map(|i| format!("q{i}")).collect();
        let rows = render_rows(
            QueuedProps {
                queue: &queue,
                hint: None,
            },
            60,
            10,
        )
        .join("\n");
        assert!(rows.contains("+3 more"), "expected overflow row: {rows}");
    }

    #[test]
    fn renders_hint_line() {
        let queue: Vec<String> = vec![];
        let rows = render_rows(
            QueuedProps {
                queue: &queue,
                hint: Some("Enter queue · Ctrl+S send now".to_string()),
            },
            60,
            3,
        )
        .join("\n");
        assert!(rows.contains("Enter queue"), "hint missing: {rows}");
    }

    #[test]
    fn empty_queue_no_hint_renders_blank() {
        let queue: Vec<String> = vec![];
        let rows = render_rows(
            QueuedProps {
                queue: &queue,
                hint: None,
            },
            60,
            3,
        )
        .join("\n");
        assert_eq!(rows.trim(), "", "expected blank, got: {rows}");
    }
}
