//! `ignis sessions …` subcommand. Walks the on-disk session store under
//! `~/.ignis/projects/` and writes an HTML report. See
//! `docs/superpowers/specs/2026-05-28-sessions-html-export-design.md`.

use crate::session::{project_sessions_dir_with_migration, project_slug};
use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
pub struct SessionsCmd {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Export per-session stats as a self-contained HTML report.
    Export(ExportArgs),
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Emit an HTML report. (Required in v1; reserves room for `--csv`,
    /// `--json` later.)
    #[arg(long)]
    pub html: bool,

    /// Which sessions to include. When omitted and stdin is a TTY, a prompt
    /// asks; when omitted and stdin is not a TTY, the command errors.
    #[arg(long, value_enum)]
    pub scope: Option<Scope>,

    /// Output file path. Default: `./ignis-sessions-<YYYY-MM-DD-HHMMSS>.html`
    /// in the current working directory.
    #[arg(long, short)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Scope {
    /// Only sessions from the current working directory's project.
    Current,
    /// All sessions across every project.
    All,
}

#[derive(Debug, Clone, Default)]
pub struct SessionRecord {
    pub session_id: String,
    pub project_slug: String,
    pub project_start_dir: Option<String>,
    /// One-line, length-bounded preview of the first user message, derived
    /// lazily at parse time and shown as the session's title in the picker.
    /// Empty when the session has no user text yet (or for legacy `.json`).
    pub title: String,
    pub started_at: Option<u64>,
    pub last_modified: Option<u64>,
    pub message_count: u64,
    /// Messages with `role == "assistant"` (text + tool-call announcements).
    pub agent_messages: u64,
    /// Messages with `role == "user"` (each is one "turn" in the inline view).
    pub user_queries: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    pub tool_calls: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct UsageFile {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

/// One event inside a turn, in chronological order. LLM durations are
/// approximate: assistant_ts − max(prior_user_ts, prior_tool_ts) — JSONL only
/// records message-finalized timestamps, not stream start. Tool durations are
/// exact: result_ts − assistant_with_tool_calls_ts.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    LlmCall {
        approx_ms: u64,
    },
    ToolCall {
        name: String,
        duration_ms: u64,
        success: bool,
    },
}

/// One user prompt and everything that runs in response to it, until the next
/// user prompt. `total_ms` is end-minus-start of the turn window; `events` are
/// in the order they happened so a waterfall can render them.
#[derive(Debug, Clone)]
pub struct TurnSummary {
    pub turn_idx: usize,
    pub started_at_ms: u64,
    pub total_ms: u64,
    pub events: Vec<TurnEvent>,
    /// One-line preview of the user message that opened this turn, so the
    /// detail view can distinguish "fix the bug" from "add a test" instead
    /// of showing identical-looking timing bars. `None` when the opening
    /// message has no text content.
    pub user_prompt: Option<String>,
}

impl TurnSummary {
    pub fn llm_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, TurnEvent::LlmCall { .. }))
            .count()
    }
    pub fn tool_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, TurnEvent::ToolCall { .. }))
            .count()
    }
    pub fn any_tool_failed(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e, TurnEvent::ToolCall { success: false, .. }))
    }
}

/// Full per-session detail for the `/sessions` Miller-column drill-in.
/// Composes the existing `SessionRecord` aggregates with per-turn timing
/// derived from the JSONL event stream.
#[derive(Debug, Clone)]
pub struct SessionDetail {
    pub record: SessionRecord,
    pub turns: Vec<TurnSummary>,
}

/// Collapse a user message into a single-line preview (~60 chars). Returns
/// `None` for whitespace-only payloads so the detail view stays uncluttered.
fn preview_one_line(s: &str) -> Option<String> {
    const MAX: usize = 60;
    let mut buf = String::with_capacity(s.len().min(MAX * 2));
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !buf.is_empty() {
                buf.push(' ');
            }
            prev_ws = true;
        } else {
            buf.push(ch);
            prev_ws = false;
        }
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    Some(if chars.len() > MAX {
        let mut out: String = chars.into_iter().take(MAX - 1).collect();
        out.push('…');
        out
    } else {
        trimmed.to_string()
    })
}

/// Parse JSONL into per-turn timing. A turn opens at each `role=user` message
/// and closes at the next user message (or the last event). Tool durations are
/// exact (result_ts − assistant_with_tool_calls_ts). LLM durations are
/// approximate: assistant_ts − max(prior_user_ts, prior_tool_ts).
pub fn extract_turns(jsonl: &str) -> Vec<TurnSummary> {
    enum Event {
        User {
            ts: u64,
            content: Option<String>,
        },
        Assistant {
            ts: u64,
        },
        Tool {
            ts: u64,
            call_id: String,
            success: bool,
        },
    }

    let mut events: Vec<Event> = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Storage writes tool replies as `type: "tool_result"` and assistant /
        // user messages as `type: "message"` — accept both so real persisted
        // sessions render tool events in the waterfall.
        let kind = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "message" && kind != "tool_result" {
            continue;
        }
        let ts = record
            .get("timestamp")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let payload = match record.get("payload") {
            Some(p) => p,
            None => continue,
        };
        let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
        match role {
            "user" => {
                let content = payload
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                events.push(Event::User { ts, content });
            }
            "assistant" => {
                events.push(Event::Assistant { ts });
            }
            "tool" => {
                let call_id = payload
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let success = payload
                    .get("content")
                    .and_then(|v| v.as_str())
                    .and_then(|c| serde_json::from_str::<Value>(c).ok())
                    .and_then(|p| p.get("is_error").and_then(|v| v.as_bool()))
                    .map(|err| !err)
                    .unwrap_or(true);
                events.push(Event::Tool {
                    ts,
                    call_id,
                    success,
                });
            }
            _ => {}
        }
    }

    // Index assistant tool-call emissions by call_id so we can join tool results
    // back to their originating call to compute exact durations.
    let mut assistant_emit_ts: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    let mut assistant_emit_tool_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for line in jsonl.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = record.get("type").and_then(|v| v.as_str());
        if kind != Some("message") && kind != Some("tool_result") {
            continue;
        }
        let ts = record
            .get("timestamp")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let payload = match record.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if payload.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(tcs) = payload.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    assistant_emit_ts.insert(id.to_string(), ts);
                    if let Some(name) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                    {
                        assistant_emit_tool_name.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }

    // Split events into turn windows. Each window starts at a user message and
    // ends just before the next one (or at the final event).
    let user_indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matches!(e, Event::User { .. }).then_some(i))
        .collect();

    let mut turns = Vec::new();
    for (turn_idx, &start) in user_indices.iter().enumerate() {
        let end = user_indices
            .get(turn_idx + 1)
            .copied()
            .unwrap_or(events.len());
        let (started_at_ms, user_prompt) = match &events[start] {
            Event::User { ts, content } => (*ts, content.as_deref().and_then(preview_one_line)),
            _ => (0, None),
        };
        let end_ms = if end == events.len() {
            match events.last() {
                Some(Event::User { ts, .. })
                | Some(Event::Assistant { ts, .. })
                | Some(Event::Tool { ts, .. }) => *ts,
                None => started_at_ms,
            }
        } else {
            match &events[end] {
                Event::User { ts, .. } => *ts,
                _ => started_at_ms,
            }
        };
        let total_ms = end_ms.saturating_sub(started_at_ms);

        let mut prior_ts = started_at_ms;
        let mut turn_events: Vec<TurnEvent> = Vec::new();
        for ev in &events[start + 1..end] {
            match ev {
                Event::Assistant { ts } => {
                    turn_events.push(TurnEvent::LlmCall {
                        approx_ms: ts.saturating_sub(prior_ts),
                    });
                    prior_ts = *ts;
                }
                Event::Tool {
                    ts,
                    call_id,
                    success,
                } => {
                    let emit_ts = assistant_emit_ts.get(call_id).copied().unwrap_or(prior_ts);
                    let name = assistant_emit_tool_name
                        .get(call_id)
                        .cloned()
                        .unwrap_or_else(|| "?".to_string());
                    turn_events.push(TurnEvent::ToolCall {
                        name,
                        duration_ms: ts.saturating_sub(emit_ts),
                        success: *success,
                    });
                    prior_ts = *ts;
                }
                Event::User { .. } => {}
            }
        }

        turns.push(TurnSummary {
            turn_idx,
            started_at_ms,
            total_ms,
            events: turn_events,
            user_prompt,
        });
    }
    turns
}

/// Compose `parse_session` + `extract_turns` for the picker drill-in. Pure data.
pub fn session_detail(
    session_id: &str,
    project_slug: &str,
    jsonl: &str,
    usage_json: Option<&str>,
) -> Result<SessionDetail> {
    let record = parse_session(session_id, project_slug, jsonl, usage_json)?;
    let turns = extract_turns(jsonl);
    Ok(SessionDetail { record, turns })
}

/// Collapse a user message into a single-line, length-bounded title: all
/// whitespace runs (incl. newlines) become single spaces, trimmed, then capped
/// by char count so we never carry a whole pasted block around. The final
/// width-aware truncation to the band happens at render time.
fn first_message_title(content: &str) -> String {
    const MAX_CHARS: usize = 120;
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() > MAX_CHARS {
        normalized.chars().take(MAX_CHARS).collect()
    } else {
        normalized
    }
}

pub fn parse_session(
    session_id: &str,
    project_slug: &str,
    jsonl: &str,
    usage_json: Option<&str>,
) -> Result<SessionRecord> {
    let mut rec = SessionRecord {
        session_id: session_id.to_string(),
        project_slug: project_slug.to_string(),
        ..Default::default()
    };

    if let Some(raw) = usage_json {
        if let Ok(u) = serde_json::from_str::<UsageFile>(raw) {
            rec.input_tokens = u.input_tokens;
            rec.output_tokens = u.output_tokens;
            rec.reasoning_tokens = u.reasoning_tokens;
            rec.cache_read_tokens = u.cache_read_tokens;
            rec.cache_write_tokens = u.cache_write_tokens;
        }
    }

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "session_meta" => {
                let p = record.get("payload");
                rec.project_start_dir = p
                    .and_then(|p| p.get("start_dir"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                // The writer stamps `timestamp` in milliseconds
                // (`storage::Persister::record` → `as_millis()`). Convert to
                // seconds so `format_timestamp_*` doesn't render year 58371.
                rec.started_at = record
                    .get("timestamp")
                    .and_then(|v| v.as_u64())
                    .map(|ms| ms / 1000);
            }
            "message" => {
                rec.message_count += 1;
                let payload = match record.get("payload") {
                    Some(p) => p,
                    None => continue,
                };
                let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");

                if role == "assistant" {
                    rec.agent_messages += 1;
                    if let Some(tcs) = payload.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if !name.is_empty() {
                                rec.tool_call_count += 1;
                                *rec.tool_calls.entry(name.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                } else if role == "user" {
                    rec.user_queries += 1;
                    // Derive the title from the first user message that has
                    // text; keep trying until one is non-empty.
                    if rec.title.is_empty() {
                        if let Some(text) = payload.get("content").and_then(|v| v.as_str()) {
                            rec.title = first_message_title(text);
                        }
                    }
                } else if role == "tool" {
                    if let Some(content) = payload.get("content").and_then(|v| v.as_str()) {
                        if let Ok(parsed) = serde_json::from_str::<Value>(content) {
                            if parsed.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                                rec.tool_error_count += 1;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(rec)
}

/// Load the per-session detail for one session by id, from disk. Returns
/// `None` if the JSONL file isn't found (e.g., session not yet persisted).
pub fn load_session_detail(
    projects_dir: &Path,
    project_slug: &str,
    session_id: &str,
) -> Option<SessionDetail> {
    let dir = projects_dir.join(project_slug);
    let jsonl_path = dir.join(format!("{session_id}.jsonl"));
    let jsonl = std::fs::read_to_string(&jsonl_path).ok()?;
    if jsonl.trim().is_empty() {
        return None;
    }
    let usage_path = dir.join(format!("{session_id}.usage.json"));
    let usage_raw = std::fs::read_to_string(&usage_path).ok();
    session_detail(session_id, project_slug, &jsonl, usage_raw.as_deref()).ok()
}

pub fn walk_sessions(projects_dir: &Path, scope: Scope, cwd: &Path) -> Result<Vec<SessionRecord>> {
    let mut out = Vec::new();

    if matches!(scope, Scope::Current) {
        if let Some(root) = projects_dir.parent() {
            let _ = project_sessions_dir_with_migration(root, cwd);
        }
    }

    let entries = match std::fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };

    let want_slug = match scope {
        Scope::Current => Some(project_slug(cwd)),
        Scope::All => None,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let slug = match path.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if let Some(want) = &want_slug {
            if &slug != want {
                continue;
            }
        }
        for file in std::fs::read_dir(&path)?.flatten() {
            let fp = file.path();
            // Scan both .jsonl (current) and .json (legacy) so older sessions
            // saved before the JSONL migration still show up — matches the
            // discoverability of `SessionManager::list()`.
            let ext = fp.extension().and_then(|e| e.to_str());
            if ext != Some("jsonl") && ext != Some("json") {
                continue;
            }
            // Skip atomic-write temp files (".json.tmp").
            if fp
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains(".tmp"))
                .unwrap_or(false)
            {
                continue;
            }
            let stem = match fp.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Sibling usage files are `<id>.usage.json` — their stem ends in
            // `.usage`. Skip so they don't show up as phantom sessions.
            if stem.ends_with(".usage") {
                continue;
            }
            let raw = match std::fs::read_to_string(&fp) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if raw.trim().is_empty() {
                continue;
            }
            let usage_path = path.join(format!("{stem}.usage.json"));
            let usage_raw = std::fs::read_to_string(&usage_path).ok();
            // For legacy `.json` files we only have the message array — no
            // envelope timestamps — so parse_session sees an empty stream and
            // reports zero turn/tool counts. That's the best we can derive
            // without re-parsing the JSON; the file still lists in the picker.
            let mut rec = if ext == Some("jsonl") {
                parse_session(&stem, &slug, &raw, usage_raw.as_deref())?
            } else {
                SessionRecord {
                    session_id: stem.clone(),
                    project_slug: slug.clone(),
                    ..Default::default()
                }
            };
            if let Ok(meta) = std::fs::metadata(&fp) {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                        rec.last_modified = Some(d.as_secs());
                    }
                }
            }
            out.push(rec);
        }
    }
    Ok(out)
}

/// Decompose a Unix-epoch second count into UTC civil-time parts. Pure `std` —
/// Howard Hinnant's algorithm; avoids pulling chrono/time as a dep.
fn epoch_to_civil(epoch_secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    const SECS_PER_DAY: u64 = 86_400;
    let days = epoch_secs / SECS_PER_DAY;
    let rem = epoch_secs % SECS_PER_DAY;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;

    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, d, h, m, s)
}

/// Format a Unix-epoch second count as `YYYY-MM-DD-HHMMSS` in UTC. Used for
/// the default HTML output filename.
pub fn format_timestamp_utc(epoch_secs: u64) -> String {
    let (y, mo, d, h, m, s) = epoch_to_civil(epoch_secs);
    format!("{y:04}-{mo:02}-{d:02}-{h:02}{m:02}{s:02}")
}

/// Format a Unix-epoch second count as `YYYY-MM-DD HH:MM` in UTC. Used for the
/// `/sessions` inline table, which doesn't need second-precision.
pub fn format_timestamp_short(epoch_secs: u64) -> String {
    let (y, mo, d, h, m, _) = epoch_to_civil(epoch_secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

pub fn default_output_path(cwd: &Path, epoch_secs: u64) -> PathBuf {
    cwd.join(format!(
        "ignis-sessions-{}.html",
        format_timestamp_utc(epoch_secs)
    ))
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLES: &str = r#"
:root { color-scheme: light dark; --fg: #1a1a1a; --muted: #777; --border: #ddd; --row-alt: #f6f6f6; --accent: #4a6df5; }
@media (prefers-color-scheme: dark) { :root { --fg: #ddd; --muted: #888; --border: #333; --row-alt: #1e1e1e; --accent: #7ea3ff; } }
* { box-sizing: border-box; }
body { font-family: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; color: var(--fg); margin: 24px; line-height: 1.4; }
h1 { font-size: 20px; margin: 0 0 4px 0; }
.sub { color: var(--muted); font-size: 13px; margin-bottom: 16px; }
.cards { display: flex; gap: 12px; margin: 16px 0 24px 0; flex-wrap: wrap; }
.card { border: 1px solid var(--border); border-radius: 6px; padding: 12px 16px; min-width: 140px; }
.card .label { color: var(--muted); font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; }
.card .value { font-size: 20px; font-weight: 600; margin-top: 2px; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { padding: 6px 10px; text-align: left; border-bottom: 1px solid var(--border); }
th { position: sticky; top: 0; background: var(--row-alt); cursor: pointer; user-select: none; }
th .chev { color: var(--muted); margin-left: 4px; }
tr.row:nth-child(even) { background: var(--row-alt); }
.mono { font-family: "SFMono-Regular", Consolas, "Liberation Mono", monospace; font-size: 12px; }
.chip { display: inline-block; padding: 1px 6px; margin: 0 3px 2px 0; background: var(--row-alt); border: 1px solid var(--border); border-radius: 3px; font-size: 11px; font-family: "SFMono-Regular", Consolas, monospace; }
.more { color: var(--muted); font-size: 11px; }
footer { margin-top: 32px; color: var(--muted); font-size: 12px; }
footer a { color: var(--accent); text-decoration: none; }
.notice { padding: 32px; text-align: center; color: var(--muted); }
"#;

const SORT_JS: &str = r#"
function sortBy(idx, type) {
  const tbl = document.querySelector('table');
  const tbody = tbl.tBodies[0];
  const rows = Array.from(tbody.querySelectorAll('tr.row'));
  const ths = tbl.querySelectorAll('th');
  const th = ths[idx];
  const asc = th.dataset.sort !== 'asc';
  ths.forEach(h => { h.dataset.sort = ''; const c = h.querySelector('.chev'); if (c) c.textContent = ''; });
  th.dataset.sort = asc ? 'asc' : 'desc';
  const chev = th.querySelector('.chev'); if (chev) chev.textContent = asc ? ' ▲' : ' ▼';
  const get = r => r.cells[idx].dataset.sort ?? r.cells[idx].textContent.trim();
  rows.sort((a, b) => {
    const A = get(a), B = get(b);
    if (type === 'num') return asc ? (Number(A) - Number(B)) : (Number(B) - Number(A));
    return asc ? A.localeCompare(B) : B.localeCompare(A);
  });
  rows.forEach(r => tbody.appendChild(r));
}
"#;

fn render_chip_cloud(tool_calls: &BTreeMap<String, u64>) -> String {
    let mut entries: Vec<(&String, &u64)> = tool_calls.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    const MAX_CHIPS: usize = 6;
    let mut html = String::new();
    for (i, (name, count)) in entries.iter().enumerate() {
        if i == MAX_CHIPS {
            let rest = entries.len() - MAX_CHIPS;
            let full: String = entries[MAX_CHIPS..]
                .iter()
                .map(|(n, c)| format!("{}×{}", n, c))
                .collect::<Vec<_>>()
                .join(", ");
            html.push_str(&format!(
                r#"<span class="more" title="{}">+{} more</span>"#,
                escape_html(&full),
                rest
            ));
            break;
        }
        html.push_str(&format!(
            r#"<span class="chip">{}×{}</span>"#,
            escape_html(name),
            count
        ));
    }
    html
}

pub fn render_html(records: &[SessionRecord], scope: Scope, generated_at: u64) -> String {
    let scope_label = match scope {
        Scope::Current => "current project",
        Scope::All => "all projects",
    };
    let total_sessions = records.len();
    let total_messages: u64 = records.iter().map(|r| r.message_count).sum();
    let total_tokens: u64 = records
        .iter()
        .map(|r| r.input_tokens + r.output_tokens)
        .sum();
    let total_tool_calls: u64 = records.iter().map(|r| r.tool_call_count).sum();

    let version = env!("CARGO_PKG_VERSION");
    let generated = format_timestamp_utc(generated_at);

    let header = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>ignis sessions report</title>
<style>{styles}</style>
</head>
<body>
<h1>ignis sessions — {scope}</h1>
<div class="sub">Generated {generated} UTC · {n} session{plural}</div>
<div class="cards">
  <div class="card"><div class="label">Sessions</div><div class="value">{n}</div></div>
  <div class="card"><div class="label">Messages</div><div class="value">{msgs}</div></div>
  <div class="card"><div class="label">Tokens (in+out)</div><div class="value">{tok}</div></div>
  <div class="card"><div class="label">Tool calls</div><div class="value">{tcs}</div></div>
</div>
"#,
        styles = STYLES,
        scope = escape_html(scope_label),
        generated = generated,
        n = total_sessions,
        plural = if total_sessions == 1 { "" } else { "s" },
        msgs = total_messages,
        tok = total_tokens,
        tcs = total_tool_calls,
    );

    let body = if records.is_empty() {
        r#"<div class="notice">No sessions found in scope.</div>"#.to_string()
    } else {
        let mut rows = String::new();
        for r in records {
            let started = r.started_at.map(format_timestamp_utc).unwrap_or_default();
            let modified = r
                .last_modified
                .map(format_timestamp_utc)
                .unwrap_or_default();
            rows.push_str(&format!(
                r#"<tr class="row"><td class="mono">{slug}</td><td class="mono">{id}</td><td data-sort="{ts_s}">{started}</td><td data-sort="{ts_m}">{modified}</td><td>{msgs}</td><td>{tok}</td><td>{tcc}</td><td>{tec}</td><td>{chips}</td></tr>"#,
                slug = escape_html(&r.project_slug),
                id = escape_html(&r.session_id),
                ts_s = r.started_at.unwrap_or(0),
                ts_m = r.last_modified.unwrap_or(0),
                started = started,
                modified = modified,
                msgs = r.message_count,
                tok = r.input_tokens + r.output_tokens,
                tcc = r.tool_call_count,
                tec = r.tool_error_count,
                chips = render_chip_cloud(&r.tool_calls),
            ));
        }
        format!(
            r#"<table>
<thead><tr>
  <th onclick="sortBy(0,'str')">Project<span class="chev"></span></th>
  <th onclick="sortBy(1,'str')">Session<span class="chev"></span></th>
  <th onclick="sortBy(2,'num')">Started<span class="chev"></span></th>
  <th onclick="sortBy(3,'num')">Modified<span class="chev"></span></th>
  <th onclick="sortBy(4,'num')">Messages<span class="chev"></span></th>
  <th onclick="sortBy(5,'num')">Tokens<span class="chev"></span></th>
  <th onclick="sortBy(6,'num')">Tools<span class="chev"></span></th>
  <th onclick="sortBy(7,'num')">Errors<span class="chev"></span></th>
  <th>Tools used</th>
</tr></thead>
<tbody>{rows}</tbody>
</table>
<script>{js}</script>"#,
            rows = rows,
            js = SORT_JS,
        )
    };

    let footer = format!(
        r#"<footer>Generated by <a href="https://github.com/Fullstop000/ignis/releases">ignis v{version}</a></footer>
</body>
</html>"#,
        version = version,
    );

    format!("{}{}{}", header, body, footer)
}

/// Inner, IO-mockable variant for tests. `is_tty` is the caller's view of
/// whether stdin is interactive.
fn resolve_scope_inner(flag: Option<Scope>, is_tty: bool) -> Result<Scope> {
    if let Some(s) = flag {
        return Ok(s);
    }
    if !is_tty {
        anyhow::bail!(
            "--scope required when stdin is not a TTY (try --scope current or --scope all)"
        );
    }
    let mut stderr = std::io::stderr().lock();
    write!(stderr, "Scope: 1) Current project  2) All projects [1/2]: ")?;
    stderr.flush()?;
    let mut buf = [0u8; 1];
    std::io::stdin().lock().read_exact(&mut buf)?;
    match buf[0] {
        b'1' => Ok(Scope::Current),
        b'2' => Ok(Scope::All),
        _ => anyhow::bail!("expected '1' or '2'"),
    }
}

fn resolve_scope(flag: Option<Scope>) -> Result<Scope> {
    resolve_scope_inner(flag, std::io::stdin().is_terminal())
}

pub async fn run(cmd: SessionsCmd) -> Result<()> {
    let Cmd::Export(args) = cmd.cmd;
    if !args.html {
        anyhow::bail!("--html is required (no other formats in v1)");
    }
    let scope = resolve_scope(args.scope)?;

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not locate home directory"))?;
    let projects_dir = home.join(".ignis/projects");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let records = walk_sessions(&projects_dir, scope, &cwd)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let html = render_html(&records, scope, now);

    let out_path = args
        .output
        .unwrap_or_else(|| default_output_path(&cwd, now));
    std::fs::write(&out_path, html)?;
    let abs = std::fs::canonicalize(&out_path).unwrap_or(out_path);
    println!("{}", abs.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_jsonl() -> String {
        // Timestamps are milliseconds — that's what `storage::Persister::record`
        // writes via `SystemTime::duration_since(UNIX_EPOCH).as_millis()`.
        [
            r#"{"type":"session_meta","timestamp":1779800000000,"payload":{"id":"sess-abc","start_dir":"/home/u/proj"}}"#,
            r#"{"type":"message","timestamp":1779800001000,"payload":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","timestamp":1779800002000,"payload":{"role":"assistant","tool_calls":[{"id":"1","type":"function","function":{"name":"read","arguments":"{}"}},{"id":"2","type":"function","function":{"name":"bash","arguments":"{}"}}]}}"#,
            r#"{"type":"message","timestamp":1779800003000,"payload":{"role":"tool","name":"read","tool_call_id":"1","content":"{\"result\":\"ok\",\"is_error\":false}"}}"#,
            r#"{"type":"message","timestamp":1779800004000,"payload":{"role":"tool","name":"bash","tool_call_id":"2","content":"{\"result\":\"boom\",\"is_error\":true}"}}"#,
            r#"{"type":"message","timestamp":1779800005000,"payload":{"role":"assistant","content":"done"}}"#,
        ]
        .join("\n")
    }

    fn fixture_usage() -> &'static str {
        r#"{"input_tokens":1000,"output_tokens":50,"reasoning_tokens":10,"cache_read_tokens":200,"cache_write_tokens":0}"#
    }

    #[test]
    fn parse_session_aggregates_counters() {
        let jsonl = fixture_jsonl();
        let rec = parse_session("sess-abc", "proj-slug", &jsonl, Some(fixture_usage())).unwrap();

        assert_eq!(rec.session_id, "sess-abc");
        assert_eq!(rec.project_slug, "proj-slug");
        assert_eq!(rec.project_start_dir.as_deref(), Some("/home/u/proj"));
        assert_eq!(rec.started_at, Some(1779800000));
        // Title is derived from the first user message ("hi").
        assert_eq!(rec.title, "hi");
        assert_eq!(rec.message_count, 5);
        // 5 messages in fixture = 1 user + 2 assistant + 2 tool.
        assert_eq!(rec.agent_messages, 2);
        assert_eq!(rec.user_queries, 1);
        assert_eq!(rec.input_tokens, 1000);
        assert_eq!(rec.output_tokens, 50);
        assert_eq!(rec.reasoning_tokens, 10);
        assert_eq!(rec.tool_call_count, 2);
        assert_eq!(rec.tool_error_count, 1);
        let mut expected = BTreeMap::new();
        expected.insert("bash".to_string(), 1);
        expected.insert("read".to_string(), 1);
        assert_eq!(rec.tool_calls, expected);
    }

    #[test]
    fn first_message_title_collapses_whitespace_and_caps() {
        assert_eq!(first_message_title("  fix\n  the   bug \n"), "fix the bug");
        assert_eq!(first_message_title("   "), "");
        let long = "x".repeat(200);
        assert_eq!(first_message_title(&long).chars().count(), 120);
    }

    #[test]
    fn parse_session_title_uses_first_nonempty_user_message() {
        // A blank first user turn is skipped; the next one with text wins.
        let jsonl = [
            r#"{"type":"message","timestamp":1,"payload":{"role":"user","content":"   "}}"#,
            r#"{"type":"message","timestamp":2,"payload":{"role":"user","content":"real first prompt"}}"#,
        ]
        .join("\n");
        let rec = parse_session("s", "p", &jsonl, None).unwrap();
        assert_eq!(rec.title, "real first prompt");
    }

    #[test]
    fn parse_session_without_usage_json_zeroes_tokens() {
        let jsonl = fixture_jsonl();
        let rec = parse_session("sess-abc", "proj-slug", &jsonl, None).unwrap();
        assert_eq!(rec.input_tokens, 0);
        assert_eq!(rec.output_tokens, 0);
        assert_eq!(rec.reasoning_tokens, 0);
        assert_eq!(rec.cache_read_tokens, 0);
        assert_eq!(rec.cache_write_tokens, 0);
    }

    fn write_fixture_session(projects_dir: &std::path::Path, slug: &str, session_id: &str) {
        let dir = projects_dir.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{session_id}.jsonl")), fixture_jsonl()).unwrap();
        std::fs::write(
            dir.join(format!("{session_id}.usage.json")),
            fixture_usage(),
        )
        .unwrap();
    }

    #[test]
    fn walk_sessions_all_scope_returns_every_project() {
        let tmp = crate::util::unique_temp_dir("ignis-walk-all");
        let projects = tmp.join("projects");
        write_fixture_session(&projects, "proj-a", "s1");
        write_fixture_session(&projects, "proj-b", "s2");

        let recs = walk_sessions(&projects, Scope::All, &PathBuf::from("/anywhere")).unwrap();
        let mut ids: Vec<&str> = recs.iter().map(|r| r.session_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["s1", "s2"]);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn resolve_scope_explicit_flag_wins() {
        let s = resolve_scope_inner(Some(Scope::All), false).unwrap();
        assert!(matches!(s, Scope::All));
        let s = resolve_scope_inner(Some(Scope::Current), true).unwrap();
        assert!(matches!(s, Scope::Current));
    }

    #[test]
    fn resolve_scope_non_tty_without_flag_errors() {
        let err = resolve_scope_inner(None, false).unwrap_err();
        assert!(err.to_string().contains("--scope"));
    }

    fn sample_record() -> SessionRecord {
        let mut tc = BTreeMap::new();
        tc.insert("bash".to_string(), 3);
        tc.insert("read".to_string(), 5);
        SessionRecord {
            session_id: "sess-<dangerous>".to_string(),
            project_slug: "-home-u-proj".to_string(),
            project_start_dir: Some("/home/u/proj".to_string()),
            title: "fix the bug".to_string(),
            started_at: Some(1735787045),
            last_modified: Some(1735787100),
            message_count: 10,
            agent_messages: 6,
            user_queries: 3,
            input_tokens: 1234,
            output_tokens: 56,
            reasoning_tokens: 7,
            cache_read_tokens: 200,
            cache_write_tokens: 0,
            tool_call_count: 8,
            tool_error_count: 1,
            tool_calls: tc,
        }
    }

    #[test]
    fn render_html_escapes_special_chars() {
        let html = render_html(&[sample_record()], Scope::Current, 1735787100);
        assert!(html.contains("sess-&lt;dangerous&gt;"));
        assert!(!html.contains("sess-<dangerous>"));
    }

    #[test]
    fn render_html_renders_one_row_per_record() {
        let html = render_html(&[sample_record(), sample_record()], Scope::All, 1735787100);
        assert_eq!(html.matches(r#"<tr class="row""#).count(), 2);
    }

    #[test]
    fn render_html_emits_summary_totals() {
        let html = render_html(&[sample_record()], Scope::Current, 1735787100);
        assert!(html.contains(">1290<"));
        assert!(html.contains(">8<"));
    }

    #[test]
    fn render_html_includes_sort_script_and_styles() {
        let html = render_html(&[sample_record()], Scope::Current, 1735787100);
        assert!(html.contains("<style>"));
        assert!(html.contains("function sortBy("));
    }

    #[test]
    fn render_html_empty_records_shows_notice() {
        let html = render_html(&[], Scope::Current, 1735787100);
        assert!(html.contains("No sessions found"));
    }

    #[test]
    fn format_timestamp_utc_matches_y_m_d_hms() {
        assert_eq!(format_timestamp_utc(1735787045), "2025-01-02-030405");
    }

    #[test]
    fn format_timestamp_utc_handles_leap_year_feb_29() {
        assert_eq!(format_timestamp_utc(1709164800), "2024-02-29-000000");
    }

    #[test]
    fn default_output_path_uses_cwd_and_timestamp() {
        let cwd = std::path::PathBuf::from("/tmp/work");
        let p = default_output_path(&cwd, 1735787045);
        assert_eq!(
            p.to_string_lossy(),
            "/tmp/work/ignis-sessions-2025-01-02-030405.html"
        );
    }

    #[test]
    fn format_timestamp_short_drops_seconds() {
        assert_eq!(format_timestamp_short(1735787045), "2025-01-02 03:04");
    }

    #[test]
    fn extract_turns_empty_input_returns_no_turns() {
        assert!(extract_turns("").is_empty());
    }

    #[test]
    fn extract_turns_one_turn_with_one_llm_call_no_tools() {
        let jsonl = [
            r#"{"type":"session_meta","timestamp":1000,"payload":{"id":"s","start_dir":"/p"}}"#,
            r#"{"type":"message","timestamp":1500,"payload":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","timestamp":2500,"payload":{"role":"assistant","content":"hello"}}"#,
        ]
        .join("\n");
        let turns = extract_turns(&jsonl);
        assert_eq!(turns.len(), 1);
        let t = &turns[0];
        assert_eq!(t.turn_idx, 0);
        assert_eq!(t.started_at_ms, 1500);
        assert_eq!(t.total_ms, 1000);
        assert_eq!(t.llm_count(), 1);
        assert_eq!(t.tool_count(), 0);
        // First (and only) event is the assistant LLM reply.
        match &t.events[0] {
            TurnEvent::LlmCall { approx_ms } => assert_eq!(*approx_ms, 1000),
            other => panic!("expected LlmCall, got {other:?}"),
        }
    }

    #[test]
    fn extract_turns_tool_duration_is_exact_join_by_call_id() {
        let jsonl = [
            r#"{"type":"message","timestamp":1000,"payload":{"role":"user","content":"do x"}}"#,
            r#"{"type":"message","timestamp":1200,"payload":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]}}"#,
            r#"{"type":"message","timestamp":1800,"payload":{"role":"tool","tool_call_id":"c1","content":"{\"is_error\":false}"}}"#,
            r#"{"type":"message","timestamp":2000,"payload":{"role":"assistant","content":"ok"}}"#,
        ]
        .join("\n");
        let turns = extract_turns(&jsonl);
        assert_eq!(turns.len(), 1);
        let t = &turns[0];
        assert_eq!(t.tool_count(), 1);
        assert_eq!(t.llm_count(), 2, "tool-call message + final reply");
        // Order: assistant(tool_calls) → tool result → assistant(text).
        let names: Vec<&str> = t
            .events
            .iter()
            .filter_map(|e| match e {
                TurnEvent::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["bash"]);
        match &t.events[1] {
            TurnEvent::ToolCall {
                duration_ms,
                success,
                ..
            } => {
                // Exact: result_ts (1800) − assistant_emit_ts (1200) = 600
                assert_eq!(*duration_ms, 600);
                assert!(*success);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn extract_turns_tool_failure_records_success_false() {
        let jsonl = [
            r#"{"type":"message","timestamp":1000,"payload":{"role":"user","content":"x"}}"#,
            r#"{"type":"message","timestamp":1100,"payload":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{}"}}]}}"#,
            r#"{"type":"message","timestamp":1500,"payload":{"role":"tool","tool_call_id":"c1","content":"{\"is_error\":true}"}}"#,
        ]
        .join("\n");
        let turns = extract_turns(&jsonl);
        assert_eq!(turns[0].tool_count(), 1);
        assert!(turns[0].any_tool_failed());
    }

    #[test]
    fn extract_turns_splits_at_each_user_message() {
        let jsonl = [
            r#"{"type":"message","timestamp":1000,"payload":{"role":"user","content":"q1"}}"#,
            r#"{"type":"message","timestamp":1200,"payload":{"role":"assistant","content":"a1"}}"#,
            r#"{"type":"message","timestamp":2000,"payload":{"role":"user","content":"q2"}}"#,
            r#"{"type":"message","timestamp":2300,"payload":{"role":"assistant","content":"a2"}}"#,
        ]
        .join("\n");
        let turns = extract_turns(&jsonl);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].started_at_ms, 1000);
        assert_eq!(turns[0].total_ms, 1000);
        assert_eq!(turns[1].started_at_ms, 2000);
        assert_eq!(turns[1].total_ms, 300);
    }

    #[test]
    fn session_detail_composes_record_and_turns() {
        let jsonl = fixture_jsonl();
        let detail =
            session_detail("sess-abc", "proj-slug", &jsonl, Some(fixture_usage())).unwrap();
        assert_eq!(detail.record.session_id, "sess-abc");
        assert_eq!(detail.turns.len(), 1);
        assert_eq!(detail.turns[0].tool_count(), 2);
        let names: Vec<&str> = detail.turns[0]
            .events
            .iter()
            .filter_map(|e| match e {
                TurnEvent::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"bash"));
    }

    #[test]
    fn extract_turns_captures_user_prompt_preview() {
        let jsonl = [
            r#"{"type":"message","timestamp":1000,"payload":{"role":"user","content":"add a test for parse_jsonl_messages"}}"#,
            r#"{"type":"message","timestamp":2000,"payload":{"role":"assistant","content":"ok"}}"#,
        ]
        .join("\n");
        let turns = extract_turns(&jsonl);
        assert_eq!(
            turns[0].user_prompt.as_deref(),
            Some("add a test for parse_jsonl_messages")
        );
    }

    #[test]
    fn extract_turns_collapses_whitespace_runs_in_preview() {
        let jsonl = r#"{"type":"message","timestamp":1000,"payload":{"role":"user","content":"line one\nline two\n  indented"}}"#;
        let turns = extract_turns(jsonl);
        assert_eq!(
            turns[0].user_prompt.as_deref(),
            Some("line one line two indented")
        );
    }

    #[test]
    fn extract_turns_truncates_long_prompt_to_60_chars() {
        let long = "x".repeat(200);
        let jsonl = format!(
            r#"{{"type":"message","timestamp":1000,"payload":{{"role":"user","content":"{long}"}}}}"#
        );
        let turns = extract_turns(&jsonl);
        let preview = turns[0].user_prompt.as_deref().unwrap();
        assert_eq!(preview.chars().count(), 60);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn extract_turns_returns_none_for_empty_user_message() {
        let jsonl =
            r#"{"type":"message","timestamp":1000,"payload":{"role":"user","content":"   "}}"#;
        let turns = extract_turns(jsonl);
        assert!(turns[0].user_prompt.is_none());
    }

    #[test]
    fn walk_sessions_current_scope_filters_by_cwd_slug() {
        let tmp = crate::util::unique_temp_dir("ignis-walk-current");
        let projects = tmp.join("projects");
        let cwd = PathBuf::from("/home/u/proj-a");
        let slug = project_slug(&cwd);
        write_fixture_session(&projects, &slug, "s1");
        write_fixture_session(&projects, "proj-b", "s2");

        let recs = walk_sessions(&projects, Scope::Current, &cwd).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].session_id, "s1");
        std::fs::remove_dir_all(&tmp).ok();
    }
}
