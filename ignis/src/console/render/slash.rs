//! The slash-command suggestions list shown above the input while typing a
//! `/command`, as a self-contained view component (`XProps + Widget +
//! From<&App>`; see [`super::footer`]). The selection-window math
//! (`slash_window_start`) stays shared in [`super::widgets`]; this owns the
//! rendering and the scroll-to-keep-selection-visible behavior.
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
    Frame,
};

use crate::console::app::App;
use crate::console::render::widgets::slash_window_start;
use crate::console::slash::SlashCommand;
use crate::console::{ACCENT, BG, SURFACE, TEXT};

/// Props for the slash-suggestions list: the candidate commands and the
/// selected index. The visible window is derived from the render area's height.
pub(crate) struct SlashProps {
    pub suggestions: Vec<SlashCommand>,
    pub selection: usize,
}

impl From<&App> for SlashProps {
    fn from(app: &App) -> Self {
        SlashProps {
            suggestions: app.slash_suggestions(),
            selection: app.slash_selection,
        }
    }
}

impl Widget for SlashProps {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.suggestions.is_empty() || area.height == 0 {
            return;
        }
        let visible = (area.height as usize).max(1);
        let sel = self.selection.min(self.suggestions.len() - 1);
        // Scroll the window so the selected entry is always shown (the list can
        // be longer than `visible` once skills + `/skills` are present).
        let start = slash_window_start(sel, visible, self.suggestions.len());
        let end = (start + visible).min(self.suggestions.len());
        let mut lines = Vec::new();
        for (idx, suggestion) in self.suggestions.iter().enumerate().take(end).skip(start) {
            let selected = idx == sel;
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
        Paragraph::new(lines)
            .style(Style::default().bg(SURFACE))
            .render(area, buf);
    }
}

/// Render the slash-suggestions list into `area`. Thin adapter; work lives in
/// the [`SlashProps`] component.
pub(crate) fn draw_slash_suggestions(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(SlashProps::from(app), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &'static str, desc: &'static str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: desc.into(),
        }
    }

    fn render_rows(props: SlashProps, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        props.render(area, &mut buf);
        (0..h)
            .map(|y| (0..w).map(|x| buf.get(x, y).symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn empty_suggestions_render_nothing() {
        let rows = render_rows(
            SlashProps {
                suggestions: vec![],
                selection: 0,
            },
            40,
            3,
        )
        .join("\n");
        assert_eq!(rows.trim(), "", "got: {rows}");
    }

    #[test]
    fn renders_name_and_description() {
        let props = SlashProps {
            suggestions: vec![cmd("model", "switch model"), cmd("clear", "reset session")],
            selection: 0,
        };
        let rows = render_rows(props, 60, 3).join("\n");
        assert!(rows.contains("model"), "got: {rows}");
        assert!(rows.contains("switch model"), "got: {rows}");
        assert!(rows.contains("clear"), "got: {rows}");
    }

    #[test]
    fn marks_the_selected_row() {
        let props = SlashProps {
            suggestions: vec![cmd("model", "a"), cmd("clear", "b")],
            selection: 1,
        };
        let rows = render_rows(props, 40, 2);
        assert!(rows[1].contains(" > clear"), "selected marker: {:?}", rows);
        assert!(
            !rows[0].contains(" > "),
            "unselected has no marker: {:?}",
            rows
        );
    }

    #[test]
    fn window_scrolls_to_keep_selection_visible() {
        // 10 commands, only 3 rows of height: selecting #9 must scroll so it's
        // shown (and the first commands fall out of the window).
        let suggestions: Vec<SlashCommand> = (0..10)
            .map(|i| {
                let name: &'static str = Box::leak(format!("cmd{i}").into_boxed_str());
                cmd(name, "desc")
            })
            .collect();
        let props = SlashProps {
            suggestions,
            selection: 9,
        };
        let rows = render_rows(props, 40, 3).join("\n");
        assert!(rows.contains("cmd9"), "selected must be visible: {rows}");
        assert!(!rows.contains("cmd0"), "top must scroll off: {rows}");
    }
}
