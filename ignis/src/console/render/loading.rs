//! The loading / status line shown directly above the input box, as a
//! self-contained view component (the `XProps + Widget + From<&App>` pattern;
//! see [`super::footer`]). Carries the spinner, elapsed timer, and live token
//! stats while the agent works, plus the exit-pending and error-flash states.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
    Frame,
};

use crate::console::app::{App, Mode};
use crate::console::{format_tokens, ACCENT, BG, RED, SUBTEXT, TEXT_DIM, YELLOW};

/// Everything the loading line renders from — its props. The states are
/// resolved here (mode → label, error flash → text) so render is a pure
/// priority cascade: exit-pending > error > idle (blank) > busy.
pub(crate) struct LoadingProps<'a> {
    pub exit_pending: bool,
    pub error: Option<&'a str>,
    pub idle: bool,
    pub spinner: &'a str,
    pub label: &'static str,
    pub elapsed: String,
    pub ctx_tokens: u64,
    pub stream_tokens: usize,
    pub stream_rate: usize,
}

impl<'a> From<&'a App> for LoadingProps<'a> {
    fn from(app: &'a App) -> Self {
        let (ctx_tokens, _) = app.context_usage();
        let label = if app.compacting {
            "Compacting"
        } else {
            match app.mode {
                Mode::Thinking => app.thinking_label(),
                Mode::ToolRunning => "Running tool",
                Mode::Idle => "",
            }
        };
        LoadingProps {
            exit_pending: app.exit_pending,
            error: app.error_flash.as_ref().map(|(m, _)| m.as_str()),
            idle: !app.compacting && app.mode == Mode::Idle,
            spinner: app.spinner(),
            label,
            elapsed: app.elapsed_str(),
            ctx_tokens,
            stream_tokens: app.stream_tokens(),
            stream_rate: app.stream_rate(),
        }
    }
}

impl Widget for LoadingProps<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let line = if self.exit_pending {
            Line::from(Span::styled(
                "  Press Ctrl-D again to exit",
                Style::default().fg(YELLOW),
            ))
        } else if let Some(msg) = self.error {
            Line::from(Span::styled(
                format!("  ✗ {}", msg),
                Style::default().fg(RED),
            ))
        } else if self.idle {
            Line::from("")
        } else {
            let mut spans = vec![
                Span::styled(format!("  {} ", self.spinner), Style::default().fg(ACCENT)),
                Span::styled(format!("{}… ", self.label), Style::default().fg(SUBTEXT)),
                Span::styled(self.elapsed, Style::default().fg(TEXT_DIM)),
            ];
            // Token stats: ↑ input/context (real when known) and ↓ live output
            // (chars/4 estimate) + rate once the reply is flowing.
            let tok_segment = if self.stream_tokens > 0 {
                format!(
                    "  ·  ↑ {} ↓ {} tok · {}/s",
                    format_tokens(self.ctx_tokens as usize),
                    format_tokens(self.stream_tokens),
                    format_tokens(self.stream_rate),
                )
            } else {
                format!("  ·  ↑ {} tok", format_tokens(self.ctx_tokens as usize))
            };
            spans.push(Span::styled(tok_segment, Style::default().fg(TEXT_DIM)));
            spans.push(Span::styled(
                "  ·  ctrl+c to interrupt",
                Style::default().fg(TEXT_DIM),
            ));
            Line::from(spans)
        };
        Paragraph::new(line)
            .style(Style::default().bg(BG))
            .render(area, buf);
    }
}

/// Render the loading/status line into `area`. Thin adapter; the work lives in
/// the [`LoadingProps`] component.
pub(crate) fn draw_loading(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(LoadingProps::from(app), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_text(props: LoadingProps, w: u16) -> String {
        let area = Rect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        props.render(area, &mut buf);
        (0..w).map(|x| buf.get(x, 0).symbol()).collect()
    }

    fn busy(stream_tokens: usize) -> LoadingProps<'static> {
        LoadingProps {
            exit_pending: false,
            error: None,
            idle: false,
            spinner: "⠋",
            label: "Thinking",
            elapsed: "5s".to_string(),
            ctx_tokens: 1500,
            stream_tokens,
            stream_rate: 40,
        }
    }

    #[test]
    fn exit_pending_takes_priority() {
        let mut props = busy(0);
        props.exit_pending = true;
        props.error = Some("ignored"); // exit-pending outranks error
        let out = render_text(props, 80);
        assert!(out.contains("Press Ctrl-D again to exit"), "got: {out}");
    }

    #[test]
    fn error_flash_shown_when_not_exiting() {
        let mut props = busy(0);
        props.error = Some("boom");
        let out = render_text(props, 80);
        assert!(out.contains("✗ boom"), "got: {out}");
    }

    #[test]
    fn idle_renders_blank() {
        let mut props = busy(0);
        props.idle = true;
        let out = render_text(props, 80);
        assert_eq!(out.trim(), "", "idle must be blank, got: {out}");
    }

    #[test]
    fn busy_without_stream_shows_input_tokens_only() {
        let out = render_text(busy(0), 100);
        assert!(out.contains("Thinking…"), "label missing: {out}");
        assert!(out.contains("5s"), "elapsed missing: {out}");
        assert!(out.contains("↑ 1.5k tok"), "ctx tokens missing: {out}");
        assert!(
            !out.contains("↓"),
            "no output arrow before streaming: {out}"
        );
        assert!(
            out.contains("ctrl+c to interrupt"),
            "interrupt hint missing: {out}"
        );
    }

    #[test]
    fn busy_with_stream_shows_output_and_rate() {
        let out = render_text(busy(800), 100);
        assert!(
            out.contains("↑ 1.5k ↓ 800 tok · 40/s"),
            "stream stats: {out}"
        );
    }

    #[test]
    fn compacting_label_renders() {
        let mut props = busy(0);
        props.label = "Compacting";
        let out = render_text(props, 100);
        assert!(out.contains("Compacting…"), "compacting label: {out}");
    }
}
