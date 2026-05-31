//! Tool-call block render — turns a `ToolCallEntry` into a header strip + a
//! body (live spinner / args / result / diff). The `ask_user` tool gets a
//! purpose-built compact trace (`ask_user_resume_trace`) instead of the
//! generic body so the picker isn't represented twice in scrollback.
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::path::Path;

use crate::console::app::{ToolCallEntry, ToolStatus};
use crate::console::highlight;
use crate::console::{
    format_duration, sanitize, truncate, BORDER, DIFF_ADD_BG, DIFF_DEL_BG, GREEN, RED, SPINNERS,
    TEXT, TEXT_DIM, YELLOW,
};
use crate::tools::{CreateFileTool, EditFileTool};
use unicode_width::UnicodeWidthStr;

/// `inline_picker::trace_lines` so resumed sessions read identically.
pub(crate) fn ask_user_resume_trace(entry: &ToolCallEntry) -> Vec<Line<'static>> {
    let (result_text, is_error) = match &entry.status {
        ToolStatus::Success(s) => (s.clone(), false),
        ToolStatus::Error(s) => (s.clone(), true),
        ToolStatus::Pending => return Vec::new(), // nothing to commit yet
    };
    let mut out = vec![Line::from("")];
    if is_error {
        // Distinguish a real cancellation (the tool's literal message) from
        // other error paths (schema validation, console-closed, headless run,
        // already-open). Anything that's not the cancellation sentence
        // preserves the real error text instead of pretending the user
        // cancelled.
        let is_cancel = result_text.contains("User cancelled the question");
        let label = if is_cancel {
            " · cancelled by user".to_string()
        } else {
            format!(" · {}", result_text.trim())
        };
        out.push(Line::from(vec![
            Span::styled("  ✗ ", Style::default().fg(RED)),
            Span::styled(
                "ask_user",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(label, Style::default().fg(TEXT_DIM)),
        ]));
        return out;
    }
    // Parse the success JSON shape. Fall back to a generic line if it's
    // anything unexpected — preserves the resume invariant ("never silent").
    let parsed = serde_json::from_str::<serde_json::Value>(&result_text)
        .ok()
        .and_then(|v| {
            v.get("answers")
                .and_then(|a| a.as_array())
                .map(|a| a.to_vec())
        });
    match parsed {
        Some(answers) if !answers.is_empty() => {
            // Header chips come from the request args; the result JSON only
            // carries the question text, so reuse that as the label.
            for a in answers {
                let question = a
                    .get("question")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string();
                let answer_text = match a.get("answer") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Array(items)) => items
                        .iter()
                        .filter_map(|i| i.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                        .join(", "),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                out.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(GREEN)),
                    Span::styled(
                        "ask_user",
                        Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" · ", Style::default().fg(BORDER)),
                    Span::styled(
                        question,
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(": ", Style::default().fg(TEXT_DIM)),
                    Span::styled(answer_text, Style::default().fg(TEXT)),
                ]));
            }
        }
        _ => {
            out.push(Line::from(vec![
                Span::styled("  ✓ ", Style::default().fg(GREEN)),
                Span::styled(
                    "ask_user",
                    Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }
    out
}

pub(crate) fn render_tool_block(
    lines: &mut Vec<Line<'static>>,
    entry: &ToolCallEntry,
    tick: u64,
    cwd: &Path,
    width: u16,
) {
    let (icon, color, status_line, elapsed) = match &entry.status {
        ToolStatus::Pending => {
            let spinner = SPINNERS[(tick as usize / 10) % SPINNERS.len()];
            let ms = entry.started_at.elapsed().as_millis();
            (
                spinner,
                YELLOW,
                format!("running… {}", format_duration(ms)),
                String::new(),
            )
        }
        ToolStatus::Success(out) => {
            let elapsed = format_duration(entry.elapsed_ms);
            let preview = truncate(out.trim(), 300);
            ("+", GREEN, preview, elapsed)
        }
        ToolStatus::Error(err) => {
            let elapsed = format_duration(entry.elapsed_ms);
            let preview = truncate(&sanitize(err.trim()), 300);
            ("x", RED, preview, elapsed)
        }
    };

    // Parse tool arguments for a compact display
    let args_compact = sanitize(&compact_tool_args(&entry.name, &entry.arguments, cwd));

    lines.push(Line::from(""));
    // Header line: ┌─ ⚙ tool_name(args) [1.2s]
    let mut header = vec![
        Span::styled("  ┌ ", Style::default().fg(color)),
        Span::styled(
            format!("{} ", icon),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            entry.name.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if !args_compact.is_empty() {
        header.push(Span::styled(
            format!("({})", args_compact),
            Style::default().fg(TEXT_DIM),
        ));
    }
    if !elapsed.is_empty() {
        header.push(Span::styled(
            format!(" {}", elapsed),
            Style::default().fg(TEXT_DIM),
        ));
    }
    lines.push(Line::from(header));

    // Status / output lines (collapsed for success, expanded for errors)
    match &entry.status {
        ToolStatus::Pending => {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(color)),
                Span::styled(status_line, Style::default().fg(TEXT_DIM)),
            ]));
        }
        ToolStatus::Success(out) => {
            // edit_file returns a git-style diff: render the hunk with solid
            // red/green backgrounds and syntax-highlighted code. Other tools get
            // a compact 3-line preview.
            let is_diff = entry.name == EditFileTool::NAME;
            let max = if is_diff { 30 } else { 3 };
            if is_diff {
                let ext = diff_file_ext(&entry.arguments);
                for sl in out.lines().take(max) {
                    push_diff_line(lines, sl, &ext, color, width);
                }
            } else {
                for sl in out.lines().take(max) {
                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(color)),
                        Span::styled(truncate(&sanitize(sl), 200), Style::default().fg(TEXT_DIM)),
                    ]));
                }
            }
            let total_lines = out.lines().count();
            if total_lines > max {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(color)),
                    Span::styled(
                        format!("… {} more lines", total_lines - max),
                        Style::default().fg(TEXT_DIM),
                    ),
                ]));
            }
        }
        ToolStatus::Error(_) => {
            for sl in status_line.lines().take(5) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(color)),
                    Span::styled(sl.to_string(), Style::default().fg(RED)),
                ]));
            }
        }
    }

    lines.push(Line::from(Span::styled("  └", Style::default().fg(color))));
}

/// The active file's extension (for syntax highlighting), from the tool args.
fn diff_file_ext(args_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args_json)
        .ok()
        .and_then(|v| {
            ["path", "file_path"]
                .iter()
                .find_map(|k| v.get(k).and_then(|p| p.as_str()).map(String::from))
        })
        .and_then(|p| {
            Path::new(&p)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Render one diff line. Added (`+`) and removed (`-`) lines get a solid
/// background filling the row and syntax-highlighted code; other lines render
/// plain. `width` is the messages-area width (for the full-row background).
fn push_diff_line(
    lines: &mut Vec<Line<'static>>,
    raw: &str,
    ext: &str,
    border: ratatui::style::Color,
    width: u16,
) {
    let prefix = Span::styled("  │ ", Style::default().fg(border));
    let (sign, bg, sign_fg) = match raw.as_bytes().first() {
        Some(b'+') => ('+', DIFF_ADD_BG, GREEN),
        Some(b'-') => ('-', DIFF_DEL_BG, RED),
        _ => {
            lines.push(Line::from(vec![
                prefix,
                Span::styled(truncate(&sanitize(raw), 200), Style::default().fg(TEXT_DIM)),
            ]));
            return;
        }
    };

    // Content area = width − "  │ " (4) − "± " (2); fill it so the bg spans the row.
    let content_w = (width as usize).saturating_sub(6).max(8);
    // `truncate` appends `…` past its limit, so cap at content_w − 1: a truncated
    // line is then exactly content_w cells and never wraps off the bg bar.
    let code = truncate(&sanitize(raw.get(2..).unwrap_or("")), content_w - 1);

    let mut spans = vec![
        prefix,
        Span::styled(
            format!("{sign} "),
            Style::default()
                .fg(sign_fg)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let mut used = 0usize;
    for (fg, text) in highlight::highlight_line(&code, ext) {
        used += UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, Style::default().fg(fg).bg(bg)));
    }
    if let Some(pad) = content_w.checked_sub(used).filter(|p| *p > 0) {
        spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    }
    lines.push(Line::from(spans));
}

/// Produce a compact arg summary from JSON, showing **values only** (never the
/// parameter names): `grep("fn main")`, `read_file(src/main.rs)`. Path-valued
/// args render bare and relative to `cwd`; other strings keep their quotes.
///
/// `edit_file` and `create_file` carry large `old_string`/`new_string`/`content`
/// payloads that drown out the path; for those tools we render only `file_path`.
pub(crate) fn compact_tool_args(tool_name: &str, json_str: &str, cwd: &Path) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return truncate(json_str, 60);
    };
    let Some(obj) = val.as_object() else {
        return truncate(json_str, 60);
    };
    if matches!(tool_name, EditFileTool::NAME | CreateFileTool::NAME) {
        if let Some(serde_json::Value::String(p)) = obj.get("file_path") {
            return truncate(&relativize_path(p, cwd), 60);
        }
    }
    let mut parts = Vec::new();
    for (k, v) in obj {
        let s = match v {
            serde_json::Value::String(s) if is_path_key(k) => {
                truncate(&relativize_path(s, cwd), 60)
            }
            serde_json::Value::String(s) => format!("\"{}\"", truncate(s, 40)),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            other => truncate(&other.to_string(), 30),
        };
        parts.push(s);
    }
    let joined = parts.join(", ");
    truncate(&joined, 80)
}

fn is_path_key(key: &str) -> bool {
    matches!(key, "path" | "file_path")
}

/// Shorten a path for display by dropping the current-directory prefix: an
/// absolute path under `cwd`, a leading `./`, or a leading `<cwd-name>/`
/// (e.g. running in `…/ignis`, `ignis/src/x` → `src/x`).
fn relativize_path(p: &str, cwd: &Path) -> String {
    let p = p.trim();
    if let Ok(stripped) = Path::new(p).strip_prefix(cwd) {
        return stripped.to_string_lossy().into_owned();
    }
    let rel = p.strip_prefix("./").unwrap_or(p);
    if let Some(name) = cwd.file_name().and_then(|n| n.to_str()) {
        if let Some(rest) = rel.strip_prefix(&format!("{name}/")) {
            return rest.to_string();
        }
    }
    rel.to_string()
}
