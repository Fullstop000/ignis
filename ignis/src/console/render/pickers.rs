//! Render functions for the slash-command pickers (`/model`, `/session`,
//! `/skills`, `/mcp`). The tool-initiated `ask_user` picker has its own
//! split-layout path in `console/render/mod.rs`.
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::cli::sessions::{format_timestamp_short, SessionDetail, TurnEvent};
use crate::console::app::{McpPicker, ModelPicker, SessionPicker, SessionPickerMode, SkillPicker};
use crate::console::{
    format_context, truncate, ACCENT, BG, GREEN, MAUVE, RED, SUBTEXT, TEXT, TEXT_DIM,
};

use super::widgets::slash_window_start;

pub(crate) fn render_session_picker(lines: &mut Vec<Line<'static>>, picker: &SessionPicker) {
    match &picker.mode {
        SessionPickerMode::List => render_session_picker_list(lines, picker),
        SessionPickerMode::Detail(detail) => render_session_picker_detail(lines, picker, detail),
    }
}

fn render_session_picker_list(lines: &mut Vec<Line<'static>>, picker: &SessionPicker) {
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

    // Column header. The leading "  " holds the same 2-char gutter the rows
    // use for the ▸ marker, so columns stay aligned regardless of selection.
    lines.push(Line::from(Span::styled(
        format!(
            "  {:<18} {:<17}{:>6}{:>8}{:>9}{:>8}",
            "ID", "STARTED", "MSGS", "TURNS", "TOK", "TOOLS"
        ),
        Style::default().fg(TEXT_DIM),
    )));

    for (idx, r) in picker.sessions.iter().enumerate() {
        let selected = idx == picker.selected;
        let is_current = r.session_id == picker.current_session_id;
        let style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT)
        };
        let marker = if is_current { "▸ " } else { "  " };
        let started = r
            .started_at
            .map(format_timestamp_short)
            .unwrap_or_else(|| "?".to_string());
        let tokens = fmt_tokens(r.input_tokens + r.output_tokens);
        let id_col = truncate_id_short(&r.session_id);
        lines.push(Line::from(Span::styled(
            format!(
                "{}{:<18} {:<17}{:>6}{:>8}{:>9}{:>8}",
                marker,
                id_col,
                started,
                r.agent_messages,
                r.user_queries,
                tokens,
                r.tool_call_count
            ),
            style,
        )));
    }

    lines.push(Line::from(Span::styled(
        "  ↑/↓ move · → details · ⏎ resume · Esc close",
        Style::default().fg(TEXT_DIM),
    )));
}

/// 18-char session id column. Short ids ("alpha", "beta") render as-is; long
/// ids like "session-1779800000-128508b8" get clipped with a trailing `…`.
fn truncate_id_short(id: &str) -> String {
    const COL: usize = 18;
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= COL {
        id.to_string()
    } else {
        format!("{}…", chars.into_iter().take(COL - 1).collect::<String>())
    }
}

fn render_session_picker_detail(
    lines: &mut Vec<Line<'static>>,
    picker: &SessionPicker,
    detail: &SessionDetail,
) {
    let r = &detail.record;
    let started = r
        .started_at
        .map(format_timestamp_short)
        .unwrap_or_else(|| "—".to_string());
    let is_current = r.session_id == picker.current_session_id;

    // Header row: ◆ <id>  (current)
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            truncate_id(&r.session_id),
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if is_current { "  (current)" } else { "" },
            Style::default().fg(GREEN),
        ),
    ]));

    // Empty-state branch: synthetic row or session with no persisted activity.
    // Skip the bars/rollups entirely so we don't render a misleading "all-output"
    // zero-token bar; tell the user explicitly what to do.
    let has_activity = !detail.turns.is_empty()
        || r.input_tokens + r.output_tokens > 0
        || !r.tool_calls.is_empty();
    if !has_activity {
        lines.push(Line::from(Span::styled(
            "  no messages persisted yet",
            Style::default().fg(TEXT_DIM),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Start a turn to see token usage, tool calls, and per-turn timing here.",
            Style::default().fg(TEXT_DIM),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ←/Esc back · ⏎ resume",
            Style::default().fg(TEXT_DIM),
        )));
        return;
    }

    lines.push(Line::from(Span::styled(
        format!(
            "  started {started} · {} msgs / {} turn{} · {} tokens · {} tool call{}{}",
            r.agent_messages,
            r.user_queries,
            if r.user_queries == 1 { "" } else { "s" },
            fmt_tokens(r.input_tokens + r.output_tokens),
            r.tool_call_count,
            if r.tool_call_count == 1 { "" } else { "s" },
            if r.tool_error_count > 0 {
                format!(" ({} failed)", r.tool_error_count)
            } else {
                String::new()
            }
        ),
        Style::default().fg(SUBTEXT),
    )));
    lines.push(Line::from(""));

    // Tokens bar — split input vs output, proportional. Only when there's
    // anything to show.
    let in_tok = r.input_tokens;
    let out_tok = r.output_tokens;
    if in_tok + out_tok > 0 {
        let total = in_tok + out_tok;
        let bar_width = 24usize;
        let in_w = (in_tok as usize * bar_width) / total as usize;
        let out_w = bar_width.saturating_sub(in_w);
        lines.push(Line::from(vec![
            Span::styled("  tokens  ", Style::default().fg(TEXT_DIM)),
            Span::styled("input ", Style::default().fg(TEXT_DIM)),
            Span::styled("█".repeat(in_w), Style::default().fg(ACCENT)),
            Span::styled(
                format!(" {} ", fmt_tokens(in_tok)),
                Style::default().fg(TEXT),
            ),
            Span::raw(" "),
            Span::styled("output ", Style::default().fg(TEXT_DIM)),
            Span::styled("█".repeat(out_w), Style::default().fg(GREEN)),
            Span::styled(
                format!(" {}", fmt_tokens(out_tok)),
                Style::default().fg(TEXT),
            ),
        ]));
    }

    // Tool rollup — top 6 by count.
    if !r.tool_calls.is_empty() {
        let mut entries: Vec<(&String, &u64)> = r.tool_calls.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        const MAX_CHIPS: usize = 6;
        let mut spans = vec![Span::styled("  tools   ", Style::default().fg(TEXT_DIM))];
        for (i, (name, count)) in entries.iter().enumerate().take(MAX_CHIPS) {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("{}×{}", name, count),
                Style::default().fg(TEXT),
            ));
        }
        if entries.len() > MAX_CHIPS {
            spans.push(Span::styled(
                format!("  +{} more", entries.len() - MAX_CHIPS),
                Style::default().fg(TEXT_DIM),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));

    // Per-turn waterfall, otel-tui-style. Bars share one coordinate system:
    // each cell is `max_total / BAR` ms wide. Outer turn bars start at column 0
    // (each turn is its own context); inner event bars start at their real
    // *offset* inside the turn so wait time and tool gaps become visible
    // (today's left-aligned bars hid them). A top ruler anchors the eye to
    // wall-clock time. Inspired by otel-tui's grid.go.
    if detail.turns.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no turns yet)",
            Style::default().fg(TEXT_DIM),
        )));
    } else {
        const MAX_TURNS: usize = 5;
        const BAR: usize = 24;
        // Unified prefix width — outer "turn N" rows and inner event rows
        // both pad to this so the bar area aligns vertically across rows.
        const PREFIX_W: usize = 13;
        let visible_turns: Vec<_> = detail.turns.iter().take(MAX_TURNS).collect();
        let max_total_ms = visible_turns
            .iter()
            .map(|t| t.total_ms.max(1))
            .max()
            .unwrap_or(1);
        let max_total = max_total_ms as f64;

        // Time ruler — 5 tick labels at 0%, 25%, 50%, 75%, 100% of the bar
        // span, aligned to where the bars start. The dashed underline below
        // gives the eye a baseline.
        let mut ruler = " ".repeat(PREFIX_W);
        let tick_count = 5;
        let mut cursor = 0usize;
        for i in 0..tick_count {
            let col = i * BAR / (tick_count - 1);
            let ms = (i as u64 * max_total_ms) / (tick_count - 1) as u64;
            let label = fmt_duration_ms(ms);
            if col > cursor {
                ruler.push_str(&" ".repeat(col - cursor));
            }
            ruler.push_str(&label);
            cursor = col + label.chars().count();
        }
        lines.push(Line::from(Span::styled(
            ruler,
            Style::default().fg(TEXT_DIM),
        )));
        let mut sep = " ".repeat(PREFIX_W);
        sep.push_str(&"─".repeat(BAR));
        lines.push(Line::from(Span::styled(sep, Style::default().fg(TEXT_DIM))));

        for (i, t) in visible_turns.iter().enumerate() {
            if i > 0 {
                lines.push(Line::from(""));
            }
            // Outer turn bar — starts at column 0 (each turn is its own scope).
            let outer_w = ((t.total_ms as f64 / max_total) * BAR as f64).round() as usize;
            let outer_w = outer_w.clamp(1, BAR);
            let outer_bar: String = format!("{}{}", "█".repeat(outer_w), "░".repeat(BAR - outer_w));
            let outer_style = if t.any_tool_failed() {
                Style::default().fg(RED)
            } else {
                Style::default().fg(ACCENT)
            };
            let outer_label = format!("  turn {:<2}", t.turn_idx + 1);
            lines.push(Line::from(vec![
                Span::styled(
                    pad_to(&outer_label, PREFIX_W),
                    Style::default().fg(TEXT_DIM),
                ),
                Span::styled(outer_bar, outer_style),
                Span::styled(
                    format!(
                        "  {}  ·  {} llm  ·  {} tool{}",
                        fmt_duration_ms(t.total_ms),
                        t.llm_count(),
                        t.tool_count(),
                        if t.tool_count() == 1 { "" } else { "s" },
                    ),
                    Style::default().fg(SUBTEXT),
                ),
            ]));

            // Inner per-event bars. Width-and-position both proportional to
            // `max_total`. `cum_ms` tracks the start offset; in our model
            // events run sequentially so cumulative duration IS the offset.
            const MAX_INNER: usize = 6;
            let shown = t.events.len().min(MAX_INNER);
            let mut cum_ms: u64 = 0;
            for ev in t.events.iter().take(MAX_INNER) {
                let (name_label, dur_ms, style, is_error) = match ev {
                    TurnEvent::LlmCall { approx_ms } => (
                        "llm ~".to_string(),
                        *approx_ms,
                        Style::default().fg(SUBTEXT),
                        false,
                    ),
                    TurnEvent::ToolCall {
                        name,
                        duration_ms,
                        success,
                    } => (
                        truncate_tool_name(name),
                        *duration_ms,
                        if *success {
                            Style::default().fg(GREEN)
                        } else {
                            Style::default().fg(RED)
                        },
                        !*success,
                    ),
                };
                // `[!]` prefix on failed events — keeps the color channel free
                // for future per-tool-kind coloring.
                let label = if is_error {
                    format!("[!] {name_label}")
                } else {
                    format!("    {name_label}")
                };
                let inner_label = format!("    {label}");

                // Compute start column from cumulative offset, width from
                // duration. Both in the same 0..BAR coordinate system as the
                // outer bar.
                let start_col = ((cum_ms as f64 / max_total) * BAR as f64).round() as usize;
                let start_col = start_col.min(BAR - 1);
                let dur_cells = ((dur_ms as f64 / max_total) * BAR as f64).round() as usize;
                // Zero-width events render as a single │ tick (otel-tui's
                // convention — preserves visibility for sub-cell events).
                let bar_chars = if dur_cells == 0 {
                    "│".to_string()
                } else {
                    "█".repeat(dur_cells.min(BAR - start_col))
                };
                let bar_w = bar_chars.chars().count();
                let pre = " ".repeat(start_col);
                let suf = " ".repeat(BAR.saturating_sub(start_col + bar_w));
                cum_ms = cum_ms.saturating_add(dur_ms);

                lines.push(Line::from(vec![
                    Span::styled(
                        pad_to(&inner_label, PREFIX_W),
                        Style::default().fg(TEXT_DIM),
                    ),
                    Span::raw(pre),
                    Span::styled(bar_chars, style),
                    Span::raw(suf),
                    Span::styled(
                        format!("  {}", fmt_duration_ms(dur_ms)),
                        Style::default().fg(SUBTEXT),
                    ),
                ]));
            }
            if t.events.len() > shown {
                lines.push(Line::from(Span::styled(
                    format!("    +{} more events", t.events.len() - shown),
                    Style::default().fg(TEXT_DIM),
                )));
            }
        }
        if detail.turns.len() > MAX_TURNS {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  ─ {} more turns ─", detail.turns.len() - MAX_TURNS),
                Style::default().fg(TEXT_DIM),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ←/Esc back · ⏎ resume",
        Style::default().fg(TEXT_DIM),
    )));
}

/// Pad a label to exactly `w` columns (truncating with `…` if it overflows).
/// Used so outer "turn N" rows and inner event rows hit the bar column at
/// the same horizontal position — otel-tui-style vertical alignment.
fn pad_to(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > w {
        let mut out: String = chars.into_iter().take(w.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        format!("{:<w$}", s, w = w)
    }
}

/// 8-char tool-name column for inner waterfall bars. Keeps long names from
/// pushing the bar off-grid.
fn truncate_tool_name(name: &str) -> String {
    const COL: usize = 8;
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= COL {
        name.to_string()
    } else {
        format!("{}…", chars.into_iter().take(COL - 1).collect::<String>())
    }
}

/// Human-friendly token count (`120` → `120`, `1500` → `1.5k`).
fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// `1234` → `1.2s`, `87` → `87ms`, `91000` → `1.5m`.
fn fmt_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{:.1}m", ms as f64 / 60_000.0)
    }
}

fn truncate_id(id: &str) -> String {
    const KEEP: usize = 24;
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= KEEP {
        id.to_string()
    } else {
        format!("{}…", chars.into_iter().take(KEEP).collect::<String>())
    }
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
    // Row budget so a many-tool server doesn't push later entries off-screen.
    // Each iteration reserves one row per remaining-in-window server *before*
    // adding tool sub-rows for the current one, so the selected entry (always
    // inside [start, end) by construction) is guaranteed to render its main
    // row. Tools near the band's bottom may get truncated — fine; full list
    // is in `ignis mcp get <name>`.
    let mut rows_used: usize = 0;
    let mut servers_remaining_in_window = end.saturating_sub(start);
    for (idx, entry) in entries.iter().enumerate().take(end).skip(start) {
        if rows_used >= visible {
            break;
        }
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
        // `(stdio)` and `(http)` both fit in 7 chars — pad to 7 so the status
        // column aligns regardless of which transport this row uses.
        let tag = format!("({})", entry.transport);
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker} {check} "),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<14}", truncate(&entry.name, 14)),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {tag:<7}"),
                style.fg(if selected { BG } else { TEXT_DIM }),
            ),
            Span::styled(format!(" {}", entry.status.label()), status_style),
        ]));
        rows_used += 1;
        servers_remaining_in_window = servers_remaining_in_window.saturating_sub(1);

        // For connected servers, list the tools indented underneath. Cap at
        // TOOL_PREVIEW_MAX so a single 23-tool server (e.g. GitHub) doesn't
        // monopolise the band; `ignis mcp get <name>` shows the full list.
        // Reserve one row per still-to-render server so a many-tool first
        // entry can't starve the rest of the window.
        if matches!(entry.status, crate::mcp::McpStatus::Connected { .. }) {
            const TOOL_PREVIEW_MAX: usize = 8;
            let tool_budget = visible
                .saturating_sub(rows_used)
                .saturating_sub(servers_remaining_in_window);
            let tools = reg.mcp_tool_list(&entry.name);
            let show = tools.len().min(TOOL_PREVIEW_MAX).min(tool_budget);
            for tool in tools.iter().take(show) {
                lines.push(Line::from(Span::styled(
                    format!("        · {tool}"),
                    Style::default().fg(SUBTEXT),
                )));
                rows_used += 1;
            }
            // Show the overflow hint when the cap (not the budget) elided
            // tools — otherwise users get "+N more" lines that are really
            // "screen ran out" lines.
            if tools.len() > show && show == TOOL_PREVIEW_MAX && rows_used < visible {
                lines.push(Line::from(Span::styled(
                    format!(
                        "        … +{n} more (see `ignis mcp get {name}`)",
                        n = tools.len() - show,
                        name = entry.name
                    ),
                    Style::default().fg(TEXT_DIM),
                )));
                rows_used += 1;
            }
        }
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

    for (idx, opt) in options.iter().enumerate() {
        let selected = idx == picker.selected;
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

    lines.push(Line::from(Span::styled(
        "  Up/Down model · ←/→ effort · Enter apply · Esc cancel",
        Style::default().fg(TEXT_DIM),
    )));
}
