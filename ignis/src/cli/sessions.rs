//! `ignis sessions …` subcommand. Walks the on-disk session store under
//! `~/.ignis/projects/` and writes an HTML report. See
//! `docs/superpowers/specs/2026-05-28-sessions-html-export-design.md`.

use crate::session::project_slug;
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

pub fn walk_sessions(projects_dir: &Path, scope: Scope, cwd: &Path) -> Result<Vec<SessionRecord>> {
    let mut out = Vec::new();
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
            if fp.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let stem = match fp.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let jsonl = match std::fs::read_to_string(&fp) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if jsonl.trim().is_empty() {
                continue;
            }
            let usage_path = path.join(format!("{stem}.usage.json"));
            let usage_raw = std::fs::read_to_string(&usage_path).ok();
            let mut rec = parse_session(&stem, &slug, &jsonl, usage_raw.as_deref())?;
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

/// Human-friendly token count (`120` → `120`, `1500` → `1.5k`, `1_234_000` →
/// `1.2M`). Kept local so this module doesn't pull in `crate::console`.
fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Truncate a session id for inline display: keep the first N chars + `…`.
fn truncate_id(id: &str) -> String {
    const KEEP: usize = 15;
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= KEEP {
        id.to_string()
    } else {
        format!("{}…", chars.into_iter().take(KEEP).collect::<String>())
    }
}

/// Render a compact stats block for the TUI `/sessions` slash command. Plain
/// (non-fenced) text — markdown renderer preserves multi-space runs within a
/// span, and a fenced block would draw a stray "code" label. Shows the top-N
/// most recently modified sessions plus an "N older" hint, marks the current
/// session with `▸ ` in a 2-char gutter when it's in the visible page, and
/// always surfaces the current session in a "You are here" line above the
/// table (covers the case where it's not in the top-N).
pub fn render_sessions_inline(
    records: &[SessionRecord],
    project_name: &str,
    current_session_id: Option<&str>,
) -> String {
    const TOP_N: usize = 5;

    let n = records.len();
    let total_tokens: u64 = records
        .iter()
        .map(|r| r.input_tokens + r.output_tokens)
        .sum();
    let total_tools: u64 = records.iter().map(|r| r.tool_call_count).sum();
    let total_errors: u64 = records.iter().map(|r| r.tool_error_count).sum();

    let summary = format!(
        "{} · {} session{} · {} tokens · {} tool calls · {} error{}",
        project_name,
        n,
        if n == 1 { "" } else { "s" },
        fmt_tokens(total_tokens),
        total_tools,
        total_errors,
        if total_errors == 1 { "" } else { "s" },
    );

    let mut out = String::new();
    out.push_str("**Sessions stats**\n\n");
    out.push_str(&summary);
    out.push_str("\n\n");

    if records.is_empty() {
        out.push_str(
            "No sessions found in this project.\n\
             → Run `ignis sessions export --html --scope all` to see other projects.",
        );
        return out;
    }

    // Always-on current-session line (covers the case where the current
    // session isn't in the top-N rows, where the `▸` marker would hide).
    if let Some(cur_id) = current_session_id {
        let line = match records.iter().find(|r| r.session_id == cur_id) {
            Some(r) => {
                let started = r
                    .started_at
                    .map(format_timestamp_short)
                    .unwrap_or_else(|| "?".to_string());
                format!(
                    "You are here · {} · started {} · {} msgs / {} turn{}",
                    truncate_id(&r.session_id),
                    started,
                    r.agent_messages,
                    r.user_queries,
                    if r.user_queries == 1 { "" } else { "s" },
                )
            }
            None => format!(
                "You are here · {} · no messages persisted yet",
                truncate_id(cur_id)
            ),
        };
        out.push_str(&line);
        out.push_str("\n\n");
    }

    let mut sorted: Vec<&SessionRecord> = records.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.last_modified.unwrap_or(0)));
    let visible = sorted.iter().take(TOP_N);
    let older = sorted.len().saturating_sub(TOP_N);

    // 2-char leftmost gutter holds the current-row marker; header + non-current
    // rows pad with two spaces to keep the column grid aligned.
    out.push_str(&format!(
        "  {:<17}{:>6}{:>8}{:>9}{:>8}\n",
        "STARTED", "MSGS", "TURNS", "TOK", "TOOLS"
    ));
    for r in visible {
        let marker = if current_session_id == Some(r.session_id.as_str()) {
            "▸ "
        } else {
            "  "
        };
        let started = r
            .started_at
            .map(format_timestamp_short)
            .unwrap_or_else(|| "?".to_string());
        let tokens = fmt_tokens(r.input_tokens + r.output_tokens);
        out.push_str(&format!(
            "{}{:<17}{:>6}{:>8}{:>9}{:>8}\n",
            marker, started, r.agent_messages, r.user_queries, tokens, r.tool_call_count
        ));
    }
    if older > 0 {
        out.push_str(&format!(
            "  ─ {} older session{} ─\n",
            older,
            if older == 1 { "" } else { "s" }
        ));
    }
    out.push('\n');
    out.push_str("→ Run `ignis sessions export --html` for the full sortable report.");
    out
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
    fn fmt_tokens_buckets_correctly() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1500), "1.5k");
        assert_eq!(fmt_tokens(120_000), "120.0k");
        assert_eq!(fmt_tokens(1_234_000), "1.2M");
    }

    fn record_with(started: u64, agent: u64, user: u64, in_tok: u64, tools: u64) -> SessionRecord {
        SessionRecord {
            session_id: format!("sess-{started}"),
            project_slug: "p".to_string(),
            started_at: Some(started),
            last_modified: Some(started),
            agent_messages: agent,
            user_queries: user,
            input_tokens: in_tok,
            tool_call_count: tools,
            ..Default::default()
        }
    }

    #[test]
    fn render_sessions_inline_shows_summary_and_pointer() {
        let recs = vec![record_with(1735787045, 38, 4, 18_000, 67)];
        let s = render_sessions_inline(&recs, "ignis", None);
        assert!(s.contains("**Sessions stats**"));
        assert!(s.contains("ignis · 1 session ·"));
        assert!(s.contains("18.0k tokens"));
        assert!(s.contains("67 tool calls"));
        assert!(s.contains("ignis sessions export --html"));
    }

    #[test]
    fn render_sessions_inline_caps_at_top_n_and_emits_older_hint() {
        let recs: Vec<SessionRecord> = (0..7)
            .map(|i| record_with(1_735_787_045 + i * 86_400, 10, 1, 1000, 5))
            .collect();
        let s = render_sessions_inline(&recs, "ignis", None);
        // 5 rendered rows, 2 older.
        assert_eq!(s.matches("2025-").count(), 5, "rendered = {s}");
        assert!(s.contains("─ 2 older sessions ─"));
    }

    #[test]
    fn render_sessions_inline_empty_shows_notice() {
        let s = render_sessions_inline(&[], "ignis", None);
        assert!(s.contains("ignis · 0 sessions"));
        assert!(s.contains("No sessions found"));
    }

    #[test]
    fn truncate_id_keeps_short_unchanged_and_clips_long() {
        assert_eq!(truncate_id("sess-abc"), "sess-abc");
        assert_eq!(
            truncate_id("session-1779599272-9d2f16cd"),
            "session-1779599…"
        );
    }

    #[test]
    fn render_sessions_inline_marks_current_row_when_in_top_n() {
        let recs = vec![record_with(1_735_787_045 + 86_400, 38, 4, 18_000, 67)];
        let s = render_sessions_inline(&recs, "ignis", Some("sess-1735873445"));
        // "You are here" line + ▸ marker on the matching row.
        assert!(
            s.contains("You are here · sess-1735873445"),
            "missing You are here line: {s}"
        );
        assert!(s.contains("▸ 2025-"), "missing ▸ on current row: {s}");
        // Header still gets the 2-char gutter so columns stay aligned.
        assert!(
            s.contains("  STARTED"),
            "header lost its leading gutter: {s}"
        );
    }

    #[test]
    fn render_sessions_inline_you_are_here_when_current_not_in_top_n() {
        // 7 records, current is the 6th-newest → falls in the "older" bucket
        // so the ▸ marker can't render. The "You are here" line must.
        let recs: Vec<SessionRecord> = (0..7)
            .map(|i| record_with(1_735_787_045 + i * 86_400, 10, 1, 1000, 5))
            .collect();
        let cur = recs[1].session_id.clone(); // second-oldest, sorted-newest-first goes to bottom
        let s = render_sessions_inline(&recs, "ignis", Some(&cur));
        assert!(s.contains(&format!("You are here · {}", cur)), "{s}");
        // No marker row in the visible top-5.
        assert!(!s.contains("▸ "), "marker leaked into off-page row: {s}");
    }

    #[test]
    fn render_sessions_inline_falls_back_when_current_id_unknown() {
        let recs = vec![record_with(1_735_787_045, 10, 1, 1000, 5)];
        let s = render_sessions_inline(&recs, "ignis", Some("sess-not-yet-persisted"));
        assert!(
            s.contains("You are here · sess-not-yet-pe… · no messages persisted yet"),
            "fallback line missing or wrong: {s}"
        );
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
