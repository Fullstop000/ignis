//! Render functions for the slash-command pickers (`/model`, `/session`,
//! `/skills`, `/mcp`). The tool-initiated `ask_user` picker has its own
//! split-layout path in `console/render/mod.rs`.
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::console::app::{McpPicker, ModelPicker, SessionPicker, SkillPicker};
use crate::console::{
    format_context, truncate, ACCENT, BG, GREEN, MAUVE, RED, SUBTEXT, TEXT, TEXT_DIM,
};

use super::widgets::{picker_window, slash_window_start};

pub(crate) fn render_session_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &SessionPicker,
    max_rows: usize,
) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Sessions",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    if picker.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No sessions found.",
            Style::default().fg(TEXT_DIM),
        )));
        return;
    }

    let visible = max_rows.max(1);
    let (start, end) = picker_window(picker.selected, visible, picker.sessions.len());
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {} more above", start),
            Style::default().fg(TEXT_DIM),
        )));
    }
    for (idx, session) in picker.sessions[start..end].iter().enumerate() {
        let abs_idx = start + idx;
        let selected = abs_idx == picker.selected;
        let marker = if selected { ">" } else { " " };
        let preview = if session.preview.is_empty() {
            "(no preview)".to_string()
        } else {
            truncate(&session.preview, 48)
        };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", marker), style.add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{:<24}", truncate(&session.id, 24)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {} msg  {}  {}",
                    session.message_count,
                    session.age_str(),
                    preview
                ),
                style,
            ),
        ]));
    }
    let below = picker.sessions.len().saturating_sub(end);
    if below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more below", below),
            Style::default().fg(TEXT_DIM),
        )));
    }

    lines.push(Line::from(Span::styled(
        "  Use Up/Down to choose, Enter to resume.",
        Style::default().fg(TEXT_DIM),
    )));
}

pub(crate) fn render_skill_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &SkillPicker,
    registry: Option<&crate::skills::SkillRegistry>,
    max_rows: usize,
) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Manage skills",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    let Some(reg) = registry else { return };
    let skills = reg.all();
    // Scroll the window so the selected row stays visible when there are more
    // skills than fit in the band.
    let sel = picker.selected.min(skills.len().saturating_sub(1));
    let visible = max_rows.max(1);
    let start = slash_window_start(sel, visible, skills.len());
    let end = (start + visible).min(skills.len());
    for (idx, skill) in skills.iter().enumerate().take(end).skip(start) {
        let selected = idx == sel;
        let marker = if selected { ">" } else { " " };
        let check = if reg.is_enabled(&skill.name) {
            "[x]"
        } else {
            "[ ]"
        };
        let scope = match skill.scope {
            crate::skills::SkillScope::Project => "project",
            crate::skills::SkillScope::Global => "global",
        };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let desc = skill.description.clone().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker} {check} "),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<18}", truncate(&skill.name, 18)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}  ({scope})", truncate(&desc, 40)), style),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "  Up/Down to move, Space/Enter to toggle, Esc to close.",
        Style::default().fg(TEXT_DIM),
    )));
}

pub(crate) fn render_mcp_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &McpPicker,
    registry: Option<&crate::mcp::McpRegistry>,
    max_rows: usize,
) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Manage MCP servers",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    let Some(reg) = registry else { return };
    let entries = reg.entries();
    let sel = picker.selected.min(entries.len().saturating_sub(1));
    let visible = max_rows.max(1);
    let start = slash_window_start(sel, visible, entries.len());
    let end = (start + visible).min(entries.len());
    for (idx, entry) in entries.iter().enumerate().take(end).skip(start) {
        let selected = idx == sel;
        let marker = if selected { ">" } else { " " };
        let check = match entry.status {
            crate::mcp::McpStatus::Disabled => "[ ]",
            _ => "[x]",
        };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let status_style = match entry.status {
            crate::mcp::McpStatus::Failed { .. } => style.fg(if selected { BG } else { RED }),
            crate::mcp::McpStatus::Disabled => style.fg(if selected { BG } else { TEXT_DIM }),
            _ => style.fg(if selected { BG } else { GREEN }),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker} {check} "),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<14}", truncate(&entry.name, 14)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", entry.status.label()), status_style),
        ]));
    }

    lines.push(Line::from(Span::styled(
        "  Up/Down to move, Space/Enter to toggle, Esc to close.",
        Style::default().fg(TEXT_DIM),
    )));
}

pub(crate) fn render_model_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &ModelPicker,
    options: &[crate::llm::ModelOption],
    max_rows: usize,
) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            "Switch model",
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
    ]));

    if options.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No models configured.",
            Style::default().fg(TEXT_DIM),
        )));
        return;
    }

    // Effort row, shown only when the highlighted model declares levels.
    let sel = picker.selected.min(options.len() - 1);
    let levels = &options[sel].effort_levels;
    if !levels.is_empty() {
        let mut spans = vec![Span::styled("  effort:", Style::default().fg(TEXT_DIM))];
        for (i, level) in levels.iter().enumerate() {
            let style = if i == picker.effort_idx {
                Style::default()
                    .fg(BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(SUBTEXT)
            };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!(" {} ", level), style));
        }
        spans.push(Span::styled("   ←/→", Style::default().fg(TEXT_DIM)));
        lines.push(Line::from(spans));
    }

    let visible = max_rows.max(1);
    let (start, end) = picker_window(sel, visible, options.len());
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {} more above", start),
            Style::default().fg(TEXT_DIM),
        )));
    }
    for (idx, opt) in options[start..end].iter().enumerate() {
        let abs_idx = start + idx;
        let selected = abs_idx == picker.selected;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let label = if opt.effort_levels.is_empty() {
            format!("{}/{}", opt.provider, opt.model)
        } else {
            format!("{}/{} ◆", opt.provider, opt.model)
        };
        let mut spans = vec![
            Span::styled(format!("  {} ", marker), style.add_modifier(Modifier::BOLD)),
            Span::styled(label, style.add_modifier(Modifier::BOLD)),
        ];
        if let Some(ctx) = opt.context {
            let ctx_style = if selected {
                style
            } else {
                Style::default().fg(TEXT_DIM)
            };
            spans.push(Span::styled(
                format!("  {} ctx", format_context(ctx)),
                ctx_style,
            ));
        }
        lines.push(Line::from(spans));
    }
    let below = options.len().saturating_sub(end);
    if below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more below", below),
            Style::default().fg(TEXT_DIM),
        )));
    }

    lines.push(Line::from(Span::styled(
        "  Up/Down model · ←/→ effort · Enter apply · Esc cancel",
        Style::default().fg(TEXT_DIM),
    )));
}
