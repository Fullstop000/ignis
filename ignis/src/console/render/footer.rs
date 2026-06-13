//! The status footer, as a self-contained view component.
//!
//! This is the first panel carved out of the `draw_*(f, area, &App)` style into
//! a "React-like" component: a [`FooterProps`] struct holds exactly the slice of
//! state the footer needs (props), a ratatui [`Widget`] impl turns those props
//! into cells (render), and a single [`From<&App>`](FooterProps::from) maps the
//! `App` god-object down to props (the parent passing props to its child). All
//! `App` coupling lives in that one `From`; the render logic is pure props →
//! cells and is unit-testable without constructing an `App` or a terminal.
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
    Frame,
};

use crate::console::app::App;
use crate::console::{format_tokens, GREEN, PEACH, RED, SUBTEXT, SURFACE, TEXT_DIM, YELLOW};
use crate::permissions::Mode as PermissionMode;

/// Everything the status footer renders from — the footer's props.
///
/// Borrowed from `App` for the duration of one frame; resolved (statusline
/// visibility already decided, context usage already computed) so the render
/// path is pure presentation with no `App` method calls.
pub(crate) struct FooterProps<'a> {
    pub cwd: &'a std::path::Path,
    pub git_branch: Option<&'a str>,
    pub turns: usize,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub ctx_tokens: u64,
    pub ctx_pct: u8,
    /// Auto-approve mode, drives the badge. `None` = default (no badge).
    pub mode: Option<PermissionMode>,
    pub update_available: bool,
    pub show_cwd: bool,
    pub show_git: bool,
    pub show_turns: bool,
    pub show_model: bool,
    pub show_tokens: bool,
}

impl<'a> From<&'a App> for FooterProps<'a> {
    /// Map the `App` god-object down to the footer's props — the only place the
    /// footer touches `App`.
    fn from(app: &'a App) -> Self {
        let (ctx_tokens, ctx_pct) = app.context_usage();
        FooterProps {
            cwd: app.cwd.as_path(),
            git_branch: app.git_branch.as_deref(),
            turns: app.turn_count(),
            provider: app.provider.as_deref(),
            model: app.model.as_deref(),
            effort: app.effort.as_deref(),
            ctx_tokens,
            ctx_pct,
            mode: app.permissions.as_ref().map(|p| p.mode()),
            update_available: app.update_notice.is_some(),
            show_cwd: app.statusline_shows("cwd"),
            show_git: app.statusline_shows("git"),
            show_turns: app.statusline_shows("turns"),
            show_model: app.statusline_shows("model"),
            show_tokens: app.statusline_shows("tokens"),
        }
    }
}

impl Widget for FooterProps<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Right cluster: model + tokens, each independently toggleable via
        // /settings → Statusline. `·`-joined; empty string when both are hidden.
        let mut right_parts: Vec<String> = Vec::new();
        if self.show_model {
            if let (Some(p), Some(m)) = (self.provider, self.model) {
                right_parts.push(match self.effort {
                    Some(e) => format!("{}/{} ({})", p, m, e),
                    None => format!("{}/{}", p, m),
                });
            }
        }
        if self.show_tokens {
            right_parts.push(format!(
                "{} tok ({}%)",
                format_tokens(self.ctx_tokens as usize),
                self.ctx_pct
            ));
        }
        let right_str = if right_parts.is_empty() {
            String::new()
        } else {
            format!(" {}  ", right_parts.join("  ·  "))
        };

        // Mode badge: empty under Off (default), peach " HANDS-FREE ", red " AFK ".
        // Always shown (not user-hideable) — it's the only signal you're
        // auto-approving tool calls.
        let badge = match self.mode {
            Some(PermissionMode::HandsFree) => Some((" HANDS-FREE ", PEACH)),
            Some(PermissionMode::FullyUnattended) => Some((" AFK ", RED)),
            _ => None,
        };

        let badge_w = badge.map(|(s, _)| s.chars().count() as u16).unwrap_or(0);
        let right_w = right_str.chars().count() as u16 + badge_w;
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(right_w)])
            .split(area);

        // Left side: cwd · git branch · turns, each toggleable, then the
        // always-on update notice. All on the flexible `Min(0)` cell so the
        // optional segments truncate before the fixed-width right cluster.
        let mut left_spans: Vec<Span> = Vec::new();
        if self.show_cwd {
            left_spans.push(Span::styled(
                format!("  {}", display_cwd(self.cwd)),
                Style::default().fg(TEXT_DIM),
            ));
        }
        // Git branch (oh-my-zsh `git:(branch)` style) when cwd is in a work tree.
        if self.show_git {
            if let Some(branch) = self.git_branch {
                left_spans.push(Span::styled("  git:(", Style::default().fg(TEXT_DIM)));
                left_spans.push(Span::styled(branch.to_string(), Style::default().fg(GREEN)));
                left_spans.push(Span::styled(")", Style::default().fg(TEXT_DIM)));
            }
        }
        // Live turn count.
        if self.show_turns {
            left_spans.push(Span::styled(
                format!("   {} turns", self.turns),
                Style::default().fg(TEXT_DIM),
            ));
        }
        if self.update_available {
            left_spans.push(Span::styled(
                "   ● new version available — run `ignis upgrade`",
                Style::default().fg(YELLOW),
            ));
        }
        let left = Line::from(left_spans);
        let right = if let Some((label, color)) = badge {
            Line::from(vec![
                Span::styled(
                    label,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(right_str, Style::default().fg(SUBTEXT)),
            ])
        } else {
            Line::from(Span::styled(right_str, Style::default().fg(SUBTEXT)))
        };

        Paragraph::new(left)
            .style(Style::default().bg(SURFACE))
            .render(split[0], buf);
        Paragraph::new(right)
            .style(Style::default().bg(SURFACE))
            .alignment(ratatui::layout::Alignment::Right)
            .render(split[1], buf);
    }
}

/// Abbreviate `cwd` to `~`-relative form when under `$HOME`, else leave it
/// absolute (oh-my-zsh style).
fn display_cwd(cwd: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rel) = cwd.strip_prefix(&home) {
            return if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rel.display())
            };
        }
    }
    cwd.display().to_string()
}

/// Render the status footer into `area`. Thin adapter so existing callsites stay
/// `draw_footer(f, area, app)`; the work lives in the [`FooterProps`] component.
pub(crate) fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(FooterProps::from(app), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::path::PathBuf;

    /// Render a footer straight from props — no `App`, no real terminal — and
    /// collect the painted text. This is the payoff of the component split: the
    /// view is exercised in isolation.
    fn render_props(props: FooterProps, w: u16) -> String {
        let area = Rect::new(0, 0, w, 1);
        let mut buf = Buffer::empty(area);
        props.render(area, &mut buf);
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    fn base_props<'a>(cwd: &'a std::path::Path) -> FooterProps<'a> {
        FooterProps {
            cwd,
            git_branch: None,
            turns: 0,
            provider: Some("minimax"),
            model: Some("MiniMax-M3"),
            effort: None,
            ctx_tokens: 1500,
            ctx_pct: 1,
            mode: None,
            update_available: false,
            show_cwd: true,
            show_git: true,
            show_turns: true,
            show_model: true,
            show_tokens: true,
        }
    }

    #[test]
    fn props_render_model_and_tokens_in_right_cluster() {
        let cwd = PathBuf::from("/tmp");
        let out = render_props(base_props(&cwd), 120);
        assert!(out.contains("minimax/MiniMax-M3"), "model missing: {out}");
        assert!(out.contains("1.5k tok (1%)"), "tokens missing: {out}");
    }

    #[test]
    fn props_badge_shows_hands_free() {
        let cwd = PathBuf::from("/tmp");
        let mut props = base_props(&cwd);
        props.mode = Some(PermissionMode::HandsFree);
        let out = render_props(props, 120);
        assert!(out.contains("HANDS-FREE"), "badge missing: {out}");
    }

    #[test]
    fn props_hidden_segments_are_omitted() {
        let cwd = PathBuf::from("/tmp");
        let mut props = base_props(&cwd);
        props.show_model = false;
        props.show_tokens = false;
        props.show_turns = false;
        let out = render_props(props, 120);
        assert!(!out.contains("MiniMax"), "model should be hidden: {out}");
        assert!(!out.contains("tok ("), "tokens should be hidden: {out}");
        assert!(!out.contains("turns"), "turns should be hidden: {out}");
    }

    // --- behavior-preservation: the same assertions the old draw_footer had,
    // now flowing through From<&App> + the component. ---

    fn footer_text(app: &App, w: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, 1)).unwrap();
        term.draw(|f| draw_footer(f, f.size(), app)).unwrap();
        term.backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn footer_shows_git_branch_when_present() {
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        app.git_branch = Some("feature/login".into());
        let out = footer_text(&app, 120);
        assert!(out.contains("git:("), "expected git segment, got: {out}");
        assert!(out.contains("feature/login"), "expected branch, got: {out}");
    }

    #[test]
    fn footer_omits_git_segment_outside_repo() {
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        app.git_branch = None;
        let out = footer_text(&app, 120);
        assert!(
            !out.contains("git:("),
            "expected no git segment, got: {out}"
        );
    }

    #[test]
    fn display_cwd_abbreviates_home() {
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            assert_eq!(display_cwd(&home), "~");
            assert_eq!(display_cwd(&home.join("proj/src")), "~/proj/src");
        }
        assert_eq!(display_cwd(&PathBuf::from("/etc/x")), "/etc/x");
    }
}
