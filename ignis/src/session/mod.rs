use crate::agent::Agent;
use crate::config::CompactionConfig;
use crate::provider::LlmProvider;
use crate::storage::SessionStorage;
use crate::tools::tool::{AgentTool, ToolHooks};
use crate::{AgentEvent, Message};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub mod storage;

pub const DEFAULT_SESSION_ID: &str = "default";

/// The core conversational model. Owns the message `history` and its
/// persistence, and wraps an [`Agent`] (the execution engine) to advance the
/// conversation via [`Session::prompt`].
pub struct Session {
    id: String,
    history: Vec<Message>,
    storage: Box<dyn SessionStorage>,
    start_dir: String,
    agent: Agent,
    compaction: CompactionConfig,
    /// Cumulative real token usage for this session (persisted alongside it).
    usage: crate::Usage,
    /// Per-run inject channel (set by the console runner before each prompt);
    /// drained between rounds by the agent. `None` = no live inject source.
    inject_rx: Option<tokio::sync::mpsc::Receiver<String>>,
}

impl Session {
    /// Open a session, loading any persisted history for `id`.
    pub async fn open(
        id: String,
        system_prompt: String,
        provider: Box<dyn LlmProvider>,
        storage: Box<dyn SessionStorage>,
        start_dir: String,
    ) -> Result<Self, anyhow::Error> {
        let history = storage.load_session(&id).await?;
        let usage = storage.load_usage(&id).await.unwrap_or_default();
        Ok(Self {
            id,
            history,
            storage,
            start_dir,
            agent: Agent::new(system_prompt, provider),
            compaction: CompactionConfig::default(),
            usage,
            inject_rx: None,
        })
    }

    /// Cumulative real token usage recorded for this session.
    pub fn usage(&self) -> crate::Usage {
        self.usage
    }

    /// Configure context-compaction behavior (auto-trigger + token budgets).
    pub fn set_compaction(&mut self, compaction: CompactionConfig) {
        self.compaction = compaction;
    }

    /// Install the inject source for the next `prompt` run (per-run channel).
    pub fn set_inject_source(&mut self, rx: tokio::sync::mpsc::Receiver<String>) {
        self.inject_rx = Some(rx);
    }

    pub fn register_tool(&mut self, tool: Arc<dyn AgentTool>) {
        self.agent.register_tool(tool);
    }

    pub fn set_hooks(&mut self, hooks: Box<dyn ToolHooks>) {
        self.agent.set_hooks(hooks);
    }

    /// Apply the shared skill registry to this session's agent (enables the
    /// per-turn skill catalog and is paired with registering `SkillTool`).
    pub fn set_skills(&mut self, skills: std::sync::Arc<crate::skills::SkillRegistry>) {
        self.agent.set_skills(skills);
    }

    /// Apply the shared MCP registry — pairs with `register_mcp_tools` to make
    /// MCP servers' instructions appear in the system prompt.
    pub fn set_mcp(&mut self, mcp: std::sync::Arc<crate::mcp::McpRegistry>) {
        self.agent.set_mcp(mcp);
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Append the user's message, run the agent loop over the history, and
    /// persist the result.
    pub async fn prompt(
        &mut self,
        text: &str,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<(), anyhow::Error> {
        // Auto-compact when the estimated context grows past the threshold.
        // Best-effort: a compaction failure must not block the user's prompt.
        if self.compaction.auto && estimate_tokens(&self.history) > self.compaction.threshold_tokens
        {
            let _ = self.compact().await;
        }
        self.history.push(Message {
            role: "user".to_string(),
            content: Some(text.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
        let turn_usage = self
            .agent
            .run(&mut self.history, tx, self.inject_rx.as_mut())
            .await?;
        if !turn_usage.is_zero() {
            self.usage.add(&turn_usage);
            let _ = self.storage.save_usage(&self.id, &self.usage).await;
        }
        self.persist().await
    }

    /// Summarize older history into a single message, keeping the most recent
    /// turns (by token budget) verbatim. Returns the number of messages
    /// replaced by the summary (0 if nothing was compacted).
    pub async fn compact(&mut self) -> Result<usize, anyhow::Error> {
        let n = self.history.len();
        if n == 0 {
            return Ok(0);
        }
        // Keep the most recent messages up to the token budget (walking from the
        // end), then snap the cut forward to a user turn boundary so a tool
        // result is never orphaned from the assistant message that requested it.
        let budget = self.compaction.keep_recent_tokens;
        let mut acc = 0usize;
        let mut raw_start = n;
        for i in (0..n).rev() {
            acc += estimate_tokens(std::slice::from_ref(&self.history[i]));
            if acc > budget {
                break;
            }
            raw_start = i;
        }
        let cut = match (raw_start..n).find(|&i| self.history[i].role == "user") {
            Some(c) if c > 0 => c,
            _ => return Ok(0),
        };

        let transcript = render_transcript(&self.history[..cut]);
        let raw = self
            .agent
            .complete(
                SUMMARY_SYSTEM_PROMPT,
                &[Message {
                    role: "user".to_string(),
                    content: Some(format!("Conversation so far:\n\n{transcript}")),
                    reasoning_content: None,
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                }],
            )
            .await?;
        let summary = extract_summary(&raw);

        let summary_msg = Message {
            role: "user".to_string(),
            content: Some(format!("[Summary of earlier conversation]\n{summary}")),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let mut compacted = Vec::with_capacity(1 + n - cut);
        compacted.push(summary_msg);
        compacted.extend_from_slice(&self.history[cut..]);
        self.history = compacted;
        self.persist().await?;
        Ok(cut)
    }

    async fn persist(&self) -> Result<(), anyhow::Error> {
        self.storage
            .save_session(&self.id, &self.history, Some(&self.start_dir))
            .await
    }
}

/// Rough token estimate (~4 chars/token) — avoids a tokenizer dependency.
fn estimate_tokens(messages: &[Message]) -> usize {
    let mut chars = 0usize;
    for m in messages {
        chars += m.content.as_deref().map_or(0, str::len);
        chars += m.reasoning_content.as_deref().map_or(0, str::len);
        if let Some(tool_calls) = &m.tool_calls {
            for tc in tool_calls {
                chars += tc.function.name.len() + tc.function.arguments.len();
            }
        }
    }
    chars / 4 + 1
}

/// Render messages as a transcript for summarization, including tool calls and
/// (truncated) tool results so the summary reflects tool activity, not just chat.
fn render_transcript(messages: &[Message]) -> String {
    const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;
    let mut out = String::new();
    for m in messages {
        match m.role.as_str() {
            "tool" => {
                let name = m.name.as_deref().unwrap_or("tool");
                let raw = m.content.as_deref().unwrap_or("");
                let body: String = raw.chars().take(TOOL_OUTPUT_MAX_CHARS).collect();
                let suffix = if raw.chars().count() > TOOL_OUTPUT_MAX_CHARS {
                    "… [truncated]"
                } else {
                    ""
                };
                out.push_str(&format!("tool[{name}] result: {body}{suffix}\n"));
            }
            role => {
                if let Some(c) = m.content.as_deref().filter(|c| !c.is_empty()) {
                    out.push_str(&format!("{role}: {c}\n"));
                }
                if let Some(r) = m.reasoning_content.as_deref().filter(|r| !r.is_empty()) {
                    out.push_str(&format!("{role} (reasoning): {r}\n"));
                }
                if let Some(tool_calls) = &m.tool_calls {
                    for tc in tool_calls {
                        out.push_str(&format!(
                            "{role} called {}({})\n",
                            tc.function.name, tc.function.arguments
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Pull the `<summary>…</summary>` body from the model's response, dropping the
/// `<analysis>` scratchpad; fall back to the whole text if the tags are absent.
fn extract_summary(text: &str) -> String {
    const OPEN: &str = "<summary>";
    const CLOSE: &str = "</summary>";
    if let (Some(s), Some(e)) = (text.find(OPEN), text.find(CLOSE)) {
        if e > s + OPEN.len() {
            return text[s + OPEN.len()..e].trim().to_string();
        }
    }
    text.trim().to_string()
}

/// Claude Code's 9-section conversation-summarization prompt (condensed).
const SUMMARY_SYSTEM_PROMPT: &str = "Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions. Capture technical details, code patterns, file paths, and architectural decisions needed to continue the work without losing context.

First, wrap your analysis in <analysis> tags: review the conversation chronologically, noting the user's intent, your approach, key decisions, exact file names / code snippets / function signatures, and errors and how you fixed them. Preserve any security-relevant instructions or constraints verbatim.

Then provide the summary inside <summary> tags with these numbered sections:
1. Primary Request and Intent
2. Key Technical Concepts
3. Files and Code Sections (include important snippets and why each matters)
4. Errors and Fixes (include any user feedback)
5. Problem Solving
6. All User Messages (every non-tool-result user message; preserve security constraints verbatim)
7. Pending Tasks
8. Current Work (what was happening immediately before this summary)
9. Optional Next Step (only if directly in line with the most recent request; quote it verbatim)

Use terse, accurate bullets. Preserve exact paths, commands, identifiers, and error strings. Do not mention that the conversation was compacted.";

pub fn project_slug(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let mut slug = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    if slug.is_empty() {
        "unknown".to_string()
    } else {
        slug
    }
}

pub fn project_sessions_dir(root: &Path, cwd: &Path) -> PathBuf {
    root.join("projects").join(project_slug(cwd))
}

// ==========================================
// Session Metadata
// ==========================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub message_count: usize,
    pub last_modified: u64,
    pub preview: String,
    pub start_dir: Option<String>,
}

impl SessionMeta {
    /// Human-friendly relative time string
    pub fn age_str(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let delta = now.saturating_sub(self.last_modified);
        if delta < 60 {
            "just now".to_string()
        } else if delta < 3600 {
            format!("{} min ago", delta / 60)
        } else if delta < 86400 {
            let h = delta / 3600;
            if h == 1 {
                "1 hour ago".to_string()
            } else {
                format!("{} hours ago", h)
            }
        } else {
            let d = delta / 86400;
            if d == 1 {
                "1 day ago".to_string()
            } else {
                format!("{} days ago", d)
            }
        }
    }
}

// ==========================================
// Session Manager
// ==========================================

pub struct SessionManager {
    storage_dir: PathBuf,
}

impl SessionManager {
    pub fn new(storage_dir: PathBuf) -> Self {
        // Ensure the directory exists
        if !storage_dir.exists() {
            let _ = std::fs::create_dir_all(&storage_dir);
        }
        Self { storage_dir }
    }

    /// Generate a new session ID based on timestamp
    pub fn create_id() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let suffix = uuid::Uuid::new_v4().to_string();
        format!("session-{}-{}", now, &suffix[..8])
    }

    /// List all sessions, sorted by last_modified descending (most recent first)
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut sessions = Vec::new();
        let entries = match std::fs::read_dir(&self.storage_dir) {
            Ok(e) => e,
            Err(_) => return sessions,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if ext != Some("jsonl") && ext != Some("json") {
                continue;
            }
            // Skip tmp files
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains(".tmp"))
                .unwrap_or(false)
            {
                continue;
            }

            if let Some(meta) = self.read_session_meta(&path) {
                sessions.push(meta);
            }
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
        sessions
    }

    /// Get the most recently modified session
    pub fn latest(&self) -> Option<SessionMeta> {
        self.list().into_iter().next()
    }

    /// Check if a session exists
    pub fn exists(&self, session_id: &str) -> bool {
        self.session_path(session_id).exists()
    }

    /// Delete a session file
    pub fn delete(&self, session_id: &str) -> Result<(), std::io::Error> {
        let path = self.session_path(session_id);
        if path.exists() {
            std::fs::remove_file(path)
        } else {
            Ok(())
        }
    }

    /// Session file path for a given ID
    fn session_path(&self, session_id: &str) -> PathBuf {
        let sanitized = crate::util::sanitize_session_id(session_id);
        self.storage_dir.join(format!("{}.jsonl", sanitized))
    }

    /// Read metadata from a session file without loading the full message history
    fn read_session_meta(&self, path: &Path) -> Option<SessionMeta> {
        let file_name = path.file_stem()?.to_str()?.to_string();

        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();

        let content = std::fs::read_to_string(path).ok()?;
        let messages = if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            crate::util::parse_jsonl_messages(&content)
        } else {
            serde_json::from_str::<Vec<Message>>(&content).ok()?
        };

        let preview = messages
            .iter()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.as_ref())
            .map(|c| {
                let trimmed = c.trim();
                if trimmed.len() > 80 {
                    format!("{}…", &trimmed[..80])
                } else {
                    trimmed.to_string()
                }
            })
            .unwrap_or_default();

        // Extract start_dir from session_meta line
        let start_dir = content.lines().next().and_then(|first_line| {
            let record: serde_json::Value = serde_json::from_str(first_line.trim()).ok()?;
            if record.get("type")?.as_str()? == "session_meta" {
                record
                    .get("payload")?
                    .get("start_dir")?
                    .as_str()
                    .map(|s| s.to_string())
            } else {
                None
            }
        });

        Some(SessionMeta {
            id: file_name,
            message_count: messages.len(),
            last_modified: mtime,
            preview,
            start_dir,
        })
    }
}

/// Print a formatted table of sessions to stdout
pub fn print_sessions(sessions: &[SessionMeta]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    // Header
    println!(
        "{:<30} {:>6}  {:<15}  PREVIEW",
        "SESSION", "MSGS", "LAST ACTIVE"
    );
    println!("{}", "─".repeat(90));

    for s in sessions {
        let preview = if s.preview.len() > 40 {
            format!("{}…", &s.preview[..40])
        } else {
            s.preview.clone()
        };
        println!(
            "{:<30} {:>6}  {:<15}  {}",
            s.id,
            s.message_count,
            s.age_str(),
            preview
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{FileStorage, SessionStorage};

    #[test]
    fn project_slug_matches_claude_style_path_slug() {
        assert_eq!(
            project_slug(Path::new("/home/zht/ignis")),
            "-home-zht-ignis"
        );
        assert_eq!(
            project_slug(Path::new("/tmp/with space")),
            "-tmp-with-space"
        );
    }

    #[test]
    fn project_sessions_dir_scopes_sessions_by_cwd() {
        let root = PathBuf::from("/tmp/ignis-sessions");
        assert_eq!(
            project_sessions_dir(&root, Path::new("/home/zht/ignis")),
            PathBuf::from("/tmp/ignis-sessions/projects/-home-zht-ignis")
        );
    }

    #[tokio::test]
    async fn file_storage_round_trips_jsonl_messages() {
        let dir = crate::util::unique_temp_dir("ignis-jsonl-storage");
        let storage = FileStorage::new(dir.clone());
        let messages = vec![
            crate::Message {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            crate::Message {
                role: "assistant".to_string(),
                content: Some("hi".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        storage
            .save_session("default", &messages, Some("/tmp"))
            .await
            .unwrap();

        let session_path = dir.join("default.jsonl");
        assert!(session_path.exists());
        let raw = std::fs::read_to_string(&session_path).unwrap();
        assert!(raw
            .lines()
            .any(|line| line.contains(r#""type":"session_meta""#)));
        assert!(raw.lines().any(|line| line.contains(r#""type":"message""#)));

        let loaded = storage.load_session("default").await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, "user");
        assert_eq!(loaded[0].content.as_deref(), Some("hello"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn file_storage_round_trips_usage() {
        let dir = crate::util::unique_temp_dir("ignis-usage-storage");
        let storage = FileStorage::new(dir.clone());

        // No file yet → default.
        assert!(storage.load_usage("s").await.unwrap().is_zero());

        let usage = crate::Usage {
            input_tokens: 1234,
            output_tokens: 56,
            cache_read_tokens: 789,
            cache_write_tokens: 0,
        };
        storage.save_usage("s", &usage).await.unwrap();
        let loaded = storage.load_usage("s").await.unwrap();
        assert_eq!(loaded, usage);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn manager_lists_jsonl_sessions_with_preview() {
        let dir = crate::util::unique_temp_dir("ignis-session-manager");
        std::fs::create_dir_all(&dir).unwrap();
        let session_path = dir.join("default.jsonl");
        std::fs::write(
            &session_path,
            concat!(
                r#"{"type":"session_meta","timestamp":1,"payload":{"id":"default"}}"#,
                "\n",
                r#"{"type":"message","timestamp":2,"payload":{"role":"user","content":"first prompt"}}"#,
                "\n",
                r#"{"type":"message","timestamp":3,"payload":{"role":"assistant","content":"answer"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let manager = SessionManager::new(dir.clone());
        let sessions = manager.list();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "default");
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[0].preview, "first prompt");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn manager_reads_start_dir_from_session_meta() {
        let dir = crate::util::unique_temp_dir("ignis-session-start-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("with-dir.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":1,"payload":{"id":"with-dir","start_dir":"/home/project"}}"#,
                "\n",
                r#"{"type":"message","timestamp":2,"payload":{"role":"user","content":"hello"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let manager = SessionManager::new(dir.clone());
        let sessions = manager.list();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "with-dir");
        assert_eq!(sessions[0].start_dir.as_deref(), Some("/home/project"));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
