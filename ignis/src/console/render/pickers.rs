//! Render functions for the slash-command pickers (`/model`, `/session`,
//! `/skills`, `/mcp`). The tool-initiated `ask_user` picker has its own
//! split-layout path in `console/render/mod.rs`.
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::cli::sessions::{format_timestamp_short, SessionDetail, TurnEvent};
use crate::console::app::{
    App, McpPicker, ModelPicker, SessionPicker, SessionPickerMode, SettingsPanel, SettingsTab,
    SkillPicker, STATUSLINE_SEGMENTS,
};
use crate::console::{
    format_context, format_elapsed, format_tokens, truncate, ACCENT, BG, GREEN, MAUVE, RED,
    SUBTEXT, TEXT, TEXT_DIM,
};

use super::widgets::picker_window;

pub(crate) fn render_session_picker(
    lines: &mut Vec<Line<'static>>,
    picker: &SessionPicker,
    max_rows: usize,
) {
    match &picker.mode {
        SessionPickerMode::List => render_session_picker_list(lines, picker, max_rows),
        SessionPickerMode::Detail(detail) => render_session_picker_detail(lines, detail),
    }
}

fn render_session_picker_list(
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

    // Each session is a two-line row — a full-width title line over a dim meta
    // line — so half as many rows fit in the band as there are lines available
    // (windowing keeps the selected row in view; closes #62).
    let visible = (max_rows / 2).max(1);
    let (start, end) = picker_window(picker.selected, visible, picker.sessions.len());
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {} more above", start),
            Style::default().fg(TEXT_DIM),
        )));
    }
    for (offset, r) in picker.sessions[start..end].iter().enumerate() {
        let idx = start + offset;
        let selected = idx == picker.selected;

        // Line 1: the title, or a dim fallback when there's no user text yet.
        let title = if r.title.is_empty() {
            "(no message yet)".to_string()
        } else {
            fit_title(&r.title, TITLE_MAX_CELLS)
        };
        let title_style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else if r.title.is_empty() {
            Style::default().fg(TEXT_DIM)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::from(Span::styled(format!("  {title}"), title_style)));

        // Line 2: dim meta — started · short id · turns · tokens.
        let started = r
            .started_at
            .map(format_timestamp_short)
            .unwrap_or_else(|| "?".to_string());
        let tokens = fmt_tokens(r.input_tokens + r.output_tokens);
        let meta = format!(
            "{} · {} · {} msgs · {} tok",
            started,
            truncate_id_short(&r.session_id),
            r.agent_messages,
            tokens,
        );
        let meta_style = if selected {
            Style::default().fg(BG).bg(ACCENT)
        } else {
            Style::default().fg(TEXT_DIM)
        };
        lines.push(Line::from(Span::styled(format!("    {meta}"), meta_style)));
    }
    let below = picker.sessions.len().saturating_sub(end);
    if below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more below", below),
            Style::default().fg(TEXT_DIM),
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

/// Visible cap for the title line so a long or CJK-heavy first message can't
/// overflow the band on a normal-width terminal.
const TITLE_MAX_CELLS: usize = 72;

/// Truncate `title` to at most `max_cells` display columns, appending `…` when
/// it has to cut. Width-aware (CJK glyphs are 2 cells) so the line never
/// overflows by counting chars where it should count cells.
fn fit_title(title: &str, max_cells: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if UnicodeWidthStr::width(title) <= max_cells {
        return title.to_string();
    }
    let budget = max_cells.saturating_sub(1); // reserve a cell for the ellipsis
    let mut out = String::new();
    let mut used = 0usize;
    for ch in title.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn render_session_picker_detail(lines: &mut Vec<Line<'static>>, detail: &SessionDetail) {
    let r = &detail.record;
    let started = r
        .started_at
        .map(format_timestamp_short)
        .unwrap_or_else(|| "—".to_string());

    // Header row: ◆ <id>
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ◆ ", Style::default().fg(MAUVE)),
        Span::styled(
            truncate_id(&r.session_id),
            Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
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

            // User prompt preview — without this, identical-looking timing
            // bars are indistinguishable when scrolling a long session.
            if let Some(prompt) = t.user_prompt.as_deref() {
                let mut line = " ".repeat(PREFIX_W);
                line.push('"');
                line.push_str(prompt);
                line.push('"');
                lines.push(Line::from(Span::styled(line, Style::default().fg(TEXT))));
            }

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
    // Window the selection so it stays visible when the list overflows.
    let sel = picker.selected.min(skills.len().saturating_sub(1));
    let visible = max_rows.max(1);
    let (start, end) = picker_window(sel, visible, skills.len());
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {} more above", start),
            Style::default().fg(TEXT_DIM),
        )));
    }
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
    let below = skills.len().saturating_sub(end);
    if below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more below", below),
            Style::default().fg(TEXT_DIM),
        )));
    }

    if let Some(status) = &picker.status {
        lines.push(Line::from(Span::styled(
            format!("  {status}"),
            Style::default().fg(GREEN),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  Up/Down move · Space/Enter toggle · r reload · Esc close",
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
    let (start, end) = picker_window(sel, visible, entries.len());
    if start > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↑ {} more above", start),
            Style::default().fg(TEXT_DIM),
        )));
    }
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
    let below = entries.len().saturating_sub(end);
    if below > 0 {
        lines.push(Line::from(Span::styled(
            format!("  ↓ {} more below", below),
            Style::default().fg(TEXT_DIM),
        )));
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

/// `/settings` control panel — a tab bar (Stats | Statusline) over the active
/// tab's body. Reads `App` directly for the live Stats view.
/// App-derived data the `/settings` tabs render from, resolved once per frame
/// so the tab renderers are pure (data → lines) and unit-testable without an
/// `App`. The only place the settings panel touches the `App` god-object.
pub(crate) struct SettingsData {
    pub ctx_tokens: u64,
    pub ctx_pct: u8,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub turns: usize,
    pub msgs: usize,
    pub tools: Vec<(String, usize)>,
    pub uptime_ms: u128,
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    /// Which footer segments are on, aligned to `STATUSLINE_SEGMENTS`.
    pub segment_shown: [bool; STATUSLINE_SEGMENTS.len()],
}

impl From<&App> for SettingsData {
    fn from(app: &App) -> Self {
        let (ctx_tokens, ctx_pct) = app.context_usage();
        let u = &app.cumulative_usage;
        let mut segment_shown = [false; STATUSLINE_SEGMENTS.len()];
        for (i, (id, _)) in STATUSLINE_SEGMENTS.iter().enumerate() {
            segment_shown[i] = app.statusline_shows(id);
        }
        SettingsData {
            ctx_tokens,
            ctx_pct,
            in_tokens: u.input_tokens,
            out_tokens: u.output_tokens,
            cache_read: u.cache_read_tokens,
            cache_write: u.cache_write_tokens,
            turns: app.turn_count(),
            msgs: app.message_count(),
            tools: app.tool_tally(),
            uptime_ms: app.session_uptime().as_millis(),
            provider: app.provider.clone(),
            model: app.model.clone(),
            effort: app.effort.clone(),
            segment_shown,
        }
    }
}

pub(crate) fn render_settings_panel(
    lines: &mut Vec<Line<'static>>,
    panel: &SettingsPanel,
    app: &App,
) {
    let data = SettingsData::from(app);
    lines.push(Line::from(""));
    // Title + tab bar: active tab reverse-highlighted in the accent color.
    let mut header = vec![Span::styled(
        "  ⚙ Settings   ",
        Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
    )];
    for (label, active) in [
        ("Stats", panel.tab == SettingsTab::Stats),
        ("Statusline", panel.tab == SettingsTab::Statusline),
    ] {
        let style = if active {
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_DIM)
        };
        header.push(Span::styled(format!(" {label} "), style));
        header.push(Span::raw(" "));
    }
    lines.push(Line::from(header));
    lines.push(Line::from(""));

    match panel.tab {
        SettingsTab::Stats => render_stats_tab(lines, &data),
        SettingsTab::Statusline => render_statusline_tab(lines, panel, &data),
    }

    lines.push(Line::from(""));
    let hint = match panel.tab {
        SettingsTab::Stats => "  ←/→ tab · Esc close",
        SettingsTab::Statusline => "  ←/→ tab · ↑/↓ move · Space toggle · Esc close",
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(TEXT_DIM),
    )));
}

/// Footer-segment checklist — Space/Enter toggles each segment on/off. The
/// mode badge is intentionally not listed (always shown for safety).
fn render_statusline_tab(
    lines: &mut Vec<Line<'static>>,
    panel: &SettingsPanel,
    data: &SettingsData,
) {
    for (i, (_id, label)) in STATUSLINE_SEGMENTS.iter().enumerate() {
        let selected = i == panel.statusline_idx;
        let marker = if selected { ">" } else { " " };
        let check = if data.segment_shown[i] { "[x]" } else { "[ ]" };
        let style = if selected {
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {marker} {check} "),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled((*label).to_string(), style),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "      ( AFK / HANDS-FREE badge is always shown )",
        Style::default().fg(TEXT_DIM),
    )));
}

/// Live read-only stats for the current session.
fn render_stats_tab(lines: &mut Vec<Line<'static>>, data: &SettingsData) {
    use super::stack::{self, Cell};

    let label = Style::default().fg(TEXT_DIM);
    let val = Style::default().fg(TEXT);

    // Each stat is `(key, value-cell)` data. The key column content-sizes to
    // its widest entry below (replacing the old hardcoded `{:<9}`), so adding
    // a longer-named stat keeps every value aligned automatically.
    let mut rows: Vec<(&str, Cell)> = Vec::new();

    // Context gauge (against the active model's window).
    let bar_w = 14usize;
    let filled = (data.ctx_pct as usize * bar_w / 100).min(bar_w);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_w - filled);
    rows.push((
        "context",
        stack::spans(vec![
            Span::styled(bar, Style::default().fg(ACCENT)),
            Span::styled(
                format!(
                    "  {}%  ({} tok)",
                    data.ctx_pct,
                    format_tokens(data.ctx_tokens as usize)
                ),
                val,
            ),
        ]),
    ));

    // Cumulative session tokens.
    rows.push((
        "tokens",
        stack::text(
            format!(
                "\u{2191} {}   \u{2193} {}",
                format_tokens(data.in_tokens as usize),
                format_tokens(data.out_tokens as usize)
            ),
            val,
        ),
    ));
    if data.cache_read > 0 || data.cache_write > 0 {
        rows.push((
            "cache",
            stack::text(
                format!(
                    "read {}   write {}",
                    format_tokens(data.cache_read as usize),
                    format_tokens(data.cache_write as usize)
                ),
                val,
            ),
        ));
    }

    // Turns / messages.
    rows.push((
        "turns",
        stack::text(
            format!("{}   \u{00b7}   {} msgs", data.turns, data.msgs),
            val,
        ),
    ));

    // Top tools.
    if data.tools.is_empty() {
        rows.push(("tools", stack::text("\u{2014}", label)));
    } else {
        let shown: Vec<String> = data
            .tools
            .iter()
            .take(4)
            .map(|(n, c)| format!("{n} {c}"))
            .collect();
        let mut s = shown.join(" \u{00b7} ");
        let extra = data.tools.len().saturating_sub(4);
        if extra > 0 {
            s.push_str(&format!(" \u{00b7} +{extra}"));
        }
        rows.push(("tools", stack::text(s, val)));
    }

    // Uptime + active model.
    rows.push(("uptime", stack::text(format_elapsed(data.uptime_ms), val)));
    let model = match &data.effort {
        Some(e) => format!("{}/{} ({e})", data.provider, data.model),
        None => format!("{}/{}", data.provider, data.model),
    };
    rows.push(("model", stack::text(model, val)));

    // Lay out: a 2-space indent + the content-sized key column + the value.
    // `LABEL_GUTTER` is the gap between the key and value columns — a named
    // spacing, where the old `{:<9}` baked width + gap into one opaque literal.
    const LABEL_GUTTER: usize = 2;
    let keys: Vec<Cell> = rows
        .iter()
        .map(|(k, _)| stack::text(format!("  {k}"), label))
        .collect();
    let key_w = stack::column_width(&keys, LABEL_GUTTER);
    for ((_, value), key) in rows.into_iter().zip(keys) {
        lines.push(stack::row([stack::pad_right(key, key_w), value]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn fit_title_passes_short_title_through() {
        assert_eq!(fit_title("fix the bug", 72), "fix the bug");
    }

    #[test]
    fn fit_title_truncates_long_ascii_to_exact_width() {
        let out = fit_title(&"a".repeat(100), 10);
        assert!(out.ends_with('…'));
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 10);
    }

    #[test]
    fn fit_title_truncates_cjk_by_display_width_not_chars() {
        // 6 CJK glyphs = 12 cells; capped to 7 must land on exactly 7 cells
        // (3 double-width glyphs + the ellipsis), proving width-awareness.
        let out = fit_title("添加会话标题", 7);
        assert!(out.ends_with('…'));
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 7);
    }

    // --- /settings tabs, rendered straight from SettingsData (no App) ---

    fn stats(tools: Vec<(String, usize)>) -> SettingsData {
        SettingsData {
            ctx_tokens: 1500,
            ctx_pct: 50,
            in_tokens: 12000,
            out_tokens: 3400,
            cache_read: 0,
            cache_write: 0,
            turns: 3,
            msgs: 7,
            tools,
            uptime_ms: 65_000,
            provider: "minimax".to_string(),
            model: "MiniMax-M3".to_string(),
            effort: None,
            segment_shown: [true; STATUSLINE_SEGMENTS.len()],
        }
    }

    fn render_to_string(data: &SettingsData) -> String {
        let mut lines: Vec<Line<'static>> = Vec::new();
        render_stats_tab(&mut lines, data);
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn stats_tab_shows_context_gauge_tokens_turns_and_model() {
        let out = render_to_string(&stats(vec![]));
        assert!(out.contains("50%  (1.5k tok)"), "context: {out}");
        assert!(out.contains("↑ 12.0k   ↓ 3.4k"), "tokens: {out}");
        assert!(out.contains("3   ·   7 msgs"), "turns: {out}");
        assert!(out.contains("minimax/MiniMax-M3"), "model: {out}");
    }

    #[test]
    fn stats_tab_value_column_is_content_aligned() {
        // Pin the value-column offset. The widest key `context` (7) sets the
        // column; every shorter key pads to the same width so all values start
        // at the same place. The `contains` tests above wouldn't catch an
        // off-by-one in the gutter — this one does, keeping the content-sized
        // column a tested invariant rather than an incidental match.
        let out = render_to_string(&stats(vec![]));
        let prefix = |key: &str| {
            out.lines()
                .find(|l| l.contains(key))
                .unwrap_or_else(|| panic!("missing {key} row"))[..11]
                .to_string()
        };
        // 2-space indent + key + gutter, all landing on column 11.
        assert_eq!(prefix("context"), "  context  "); // 2 + 7 + 2
        assert_eq!(prefix("tokens"), "  tokens   "); // 2 + 6 + 3
        assert_eq!(prefix("uptime"), "  uptime   "); // 2 + 6 + 3
    }

    #[test]
    fn stats_tab_omits_cache_row_when_zero() {
        let out = render_to_string(&stats(vec![]));
        assert!(!out.contains("cache"), "cache row should be hidden: {out}");
    }

    #[test]
    fn stats_tab_shows_cache_row_when_present() {
        let mut d = stats(vec![]);
        d.cache_read = 800;
        d.cache_write = 200;
        let out = render_to_string(&d);
        assert!(out.contains("read 800   write 200"), "cache: {out}");
    }

    #[test]
    fn stats_tab_empty_tools_shows_dash() {
        let out = render_to_string(&stats(vec![]));
        assert!(
            out.contains("tools") && out.contains('—'),
            "expected dash: {out}"
        );
    }

    #[test]
    fn stats_tab_tools_collapse_overflow_past_four() {
        let tools = vec![
            ("read".to_string(), 5),
            ("edit".to_string(), 4),
            ("bash".to_string(), 3),
            ("grep".to_string(), 2),
            ("glob".to_string(), 1),
            ("web".to_string(), 1),
        ];
        let out = render_to_string(&stats(tools));
        assert!(out.contains("read 5"), "top tool: {out}");
        assert!(out.contains("+2"), "overflow marker for the 2 extra: {out}");
    }

    #[test]
    fn statusline_tab_reflects_segment_shown_flags() {
        let mut d = stats(vec![]);
        d.segment_shown = [true, false, true, false, true];
        let mut lines: Vec<Line<'static>> = Vec::new();
        let panel = SettingsPanel {
            tab: SettingsTab::Statusline,
            statusline_idx: 0,
        };
        render_statusline_tab(&mut lines, &panel, &d);
        let checks: Vec<bool> = lines
            .iter()
            .filter_map(|l| {
                let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
                if s.contains("[x]") {
                    Some(true)
                } else if s.contains("[ ]") {
                    Some(false)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            checks,
            vec![true, false, true, false, true],
            "got: {checks:?}"
        );
    }
}
