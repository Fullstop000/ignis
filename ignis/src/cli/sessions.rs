//! `ignis sessions …` subcommand. Walks the on-disk session store under
//! `~/.ignis/projects/` and writes an HTML report. See
//! `docs/superpowers/specs/2026-05-28-sessions-html-export-design.md`.

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
}
