//! `ignis sessions …` subcommand. Walks the on-disk session store under
//! `~/.ignis/projects/` and writes an HTML report. See
//! `docs/superpowers/specs/2026-05-28-sessions-html-export-design.md`.

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use crate::session::project_slug;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
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
                rec.started_at = record.get("timestamp").and_then(|v| v.as_u64());
            }
            "message" => {
                rec.message_count += 1;
                let payload = match record.get("payload") {
                    Some(p) => p,
                    None => continue,
                };
                let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");

                if role == "assistant" {
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

pub fn walk_sessions(
    projects_dir: &Path,
    scope: Scope,
    cwd: &Path,
) -> Result<Vec<SessionRecord>> {
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

/// Format a Unix-epoch second count as `YYYY-MM-DD-HHMMSS` in UTC. Pure
/// `std` — avoids pulling chrono/time as a dep just for one filename.
pub fn format_timestamp_utc(epoch_secs: u64) -> String {
    const SECS_PER_DAY: u64 = 86_400;
    let days = epoch_secs / SECS_PER_DAY;
    let rem = epoch_secs % SECS_PER_DAY;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;

    // Civil-from-days: derive (year, month, day) from days since 1970-01-01.
    // Howard Hinnant's algorithm — shift the epoch so we count from 0000-03-01.
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

    format!("{year:04}-{month:02}-{d:02}-{h:02}{m:02}{s:02}")
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
            let modified = r.last_modified.map(format_timestamp_utc).unwrap_or_default();
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

pub async fn run(_cmd: SessionsCmd) -> Result<()> {
    anyhow::bail!("not yet implemented");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_jsonl() -> String {
        [
            r#"{"type":"session_meta","timestamp":1779800000,"payload":{"id":"sess-abc","start_dir":"/home/u/proj"}}"#,
            r#"{"type":"message","timestamp":1779800001,"payload":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","timestamp":1779800002,"payload":{"role":"assistant","tool_calls":[{"id":"1","type":"function","function":{"name":"read","arguments":"{}"}},{"id":"2","type":"function","function":{"name":"bash","arguments":"{}"}}]}}"#,
            r#"{"type":"message","timestamp":1779800003,"payload":{"role":"tool","name":"read","tool_call_id":"1","content":"{\"result\":\"ok\",\"is_error\":false}"}}"#,
            r#"{"type":"message","timestamp":1779800004,"payload":{"role":"tool","name":"bash","tool_call_id":"2","content":"{\"result\":\"boom\",\"is_error\":true}"}}"#,
            r#"{"type":"message","timestamp":1779800005,"payload":{"role":"assistant","content":"done"}}"#,
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
        std::fs::write(dir.join(format!("{session_id}.usage.json")), fixture_usage()).unwrap();
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
