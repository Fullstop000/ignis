use crate::agent::Agent;
use crate::config::CompactionConfig;
use crate::hooks::{HookContext, HookRegistry, PromptHookResult};
use crate::llm::LlmProvider;
use crate::storage::SessionStorage;
use crate::tools::tool::{AgentTool, ToolHooks};
use crate::{AgentEvent, Message};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    /// Storage handle. Held as `Arc` so the per-prompt checkpoint can be
    /// dispatched into a detached `tokio::spawn` that survives a Ctrl+C
    /// cancel of the parent `session.prompt` future (the runner wraps prompt
    /// in `tokio::select!`; the cancel branch drops the future, and an
    /// awaited persist inside it would never complete). `Session::open` still
    /// accepts `Box<dyn SessionStorage>` for caller ergonomics.
    storage: Arc<dyn SessionStorage>,
    start_dir: String,
    agent: Agent,
    compaction: CompactionConfig,
    /// Cumulative real token usage for this session (persisted alongside it).
    usage: crate::Usage,
    /// Per-run inject channel (set by the console runner before each prompt);
    /// drained between rounds by the agent. `None` = no live inject source.
    inject_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    /// External-subprocess hook registry. Shared so the console layer can
    /// drive the `AssistantMessageRender` chain over the same registry, and
    /// so `/hooks reload` can swap the live config without rebuilding the
    /// session.
    hooks: HookRegistry,
}

impl Session {
    /// Open a session, loading any persisted history for `id`.
    #[tracing::instrument(
        name = "ignis.session",
        skip_all,
        fields(
            session.id = %id,
            provider = %provider.provider_name(),
            cwd = %start_dir,
        ),
    )]
    pub async fn open(
        id: String,
        system_prompt: String,
        provider: Box<dyn LlmProvider>,
        storage: Box<dyn SessionStorage>,
        start_dir: String,
    ) -> Result<Self, anyhow::Error> {
        let history = storage.load_session(&id).await?;
        let usage = storage.load_usage(&id).await.unwrap_or_default();
        let mut agent = Agent::new(system_prompt, provider);
        agent.set_project_instructions(crate::agent::agents_md::load(
            Path::new(&start_dir),
            dirs::home_dir().as_deref(),
        ));
        // External-subprocess hook registry. Loaded once per Session::open;
        // `/hooks reload` swaps the parsed config in place via the
        // RwLock-backed registry. Absent file = no-op, fast path.
        let hooks = match dirs::home_dir() {
            Some(home) => HookRegistry::from_config_dir(&home)?,
            None => HookRegistry::empty(),
        };
        // The registry is plumbed into each `agent.run` call in
        // `Session::prompt` rather than stored on the Agent; that keeps a
        // single source of truth (the Session's `hooks` field) and makes
        // `/hooks reload` a simple field swap.
        Ok(Self {
            id,
            history,
            storage: Arc::from(storage),
            start_dir,
            agent,
            compaction: CompactionConfig::default(),
            usage,
            inject_rx: None,
            hooks,
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

    /// The shared hook registry. The console's render path takes a clone of
    /// this handle so it can run the `AssistantMessageRender` chain over
    /// the same parsed config the session uses for `UserPromptSubmit`.
    pub fn hooks(&self) -> &HookRegistry {
        &self.hooks
    }

    /// Replace the hook registry — used by the console runner so
    /// `/hooks reload` reaches the live registry instance, and by tests
    /// so they don't have to touch the real `~/.ignis/hooks.json`.
    pub fn set_hook_registry(&mut self, registry: HookRegistry) {
        self.hooks = registry;
    }

    pub fn start_dir(&self) -> &str {
        &self.start_dir
    }

    /// Append the user's message, run the agent loop over the history, and
    /// persist the result.
    #[tracing::instrument(
        name = "ignis.turn",
        skip_all,
        fields(
            session.id = %self.id,
            prompt.length = text.len(),
            prompt.text = tracing::field::Empty,
        ),
    )]
    pub async fn prompt(
        &mut self,
        text: &str,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> Result<(), anyhow::Error> {
        // Prompt body is recorded only when IGNIS_LOG_USER_PROMPTS=1 — redacted
        // by default per the privacy gate.
        if crate::telemetry::log_user_prompts() {
            tracing::Span::current().record("prompt.text", text);
        }

        // Heal a turn the user interrupted with Ctrl+C: the dropped future can
        // leave the last assistant message holding `tool_calls` with no matching
        // `tool_result`, which providers reject. Close out the orphans before we
        // build on this history (and before compaction walks it).
        heal_interrupted_tool_calls(&mut self.history);

        // Auto-compact when the estimated context grows past the threshold.
        // Best-effort: a compaction failure must not block the user's prompt.
        if self.compaction.auto && estimate_tokens(&self.history) > self.compaction.threshold_tokens
        {
            let _ = self.compact().await;
        }

        // Run UserPromptSubmit hooks. Soft failures fall back to the
        // previous value and emit a Warning. A hard block (exit 2 / a
        // hook returning `continue: false`) short-circuits the turn —
        // the spec's one explicit exception to "hooks never kill a turn"
        // — so the prompt MUST NOT reach the model. Without this, a
        // DLP-style hook returning exit 2 would leak the original prompt.
        let effective = match self
            .hooks
            .run_user_prompt_submit(
                text,
                HookContext {
                    session_id: &self.id,
                    cwd: &self.start_dir,
                },
                &tx,
            )
            .await
        {
            PromptHookResult::Continue(t) => t,
            PromptHookResult::Blocked { .. } => {
                // The hook chain already emitted a Warning event carrying
                // the stderr reason. Do NOT push to history, do NOT call
                // the agent. The console handler renders Warning lines
                // into scrollback, so the user sees the block reason.
                return Ok(());
            }
        };

        // Announce the post-hook text so the console can render it to
        // scrollback. Without this, the console would echo the user's
        // pre-hook typed buffer and the visible block would diverge from
        // history — the model would see one string and the user another.
        let _ = tx
            .send(AgentEvent::UserPromptCommitted {
                text: effective.clone(),
            })
            .await;

        self.history.push(
            Message {
                role: "user".to_string(),
                content: Some(effective),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            }
            .stamp_now(),
        );
        // Checkpoint the user message to disk BEFORE running the model.
        //
        // The runner wraps `session.prompt` in `tokio::select!` against
        // Ctrl+C. Awaiting the persist here would be cancellable — the
        // rename step (after the temp-file write) would never run, leaving
        // disk state stale and the next prompt's `Session::open` blind to
        // the turn that was cancelled. Dispatch the write into a detached
        // `tokio::spawn` so it survives the parent future being dropped:
        // dropping a `JoinHandle` detaches, it does not abort. Errors are
        // logged, not fatal — a flaky disk shouldn't block the model call.
        let checkpoint = {
            let storage = Arc::clone(&self.storage);
            let id = self.id.clone();
            let snapshot = self.history.clone();
            let start_dir = self.start_dir.clone();
            tokio::spawn(async move {
                if let Err(e) = storage.save_session(&id, &snapshot, Some(&start_dir)).await {
                    log::error!("session checkpoint after user push failed: {e}");
                }
            })
        };
        let run_usage = self
            .agent
            .run(
                &mut self.history,
                tx,
                self.inject_rx.as_mut(),
                Some(&self.hooks),
                Some(HookContext {
                    session_id: &self.id,
                    cwd: &self.start_dir,
                }),
            )
            .await?;
        if !run_usage.is_zero() {
            self.usage.add(&run_usage);
            let _ = self.storage.save_usage(&self.id, &self.usage).await;
        }
        // Order the checkpoint write before the final persist. Without this
        // join, the spawned task could be scheduled AFTER `self.persist()`
        // and stomp the full final history with the user-only snapshot. On
        // the cancel path this join is never reached (the future is dropped
        // first), so the spawn detaches and still writes the user message.
        let _ = checkpoint.await;
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
                    created_at_ms: None,
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
            created_at_ms: None,
        };

        // Replace the summarized prefix with the summary message in-place, moving
        // the kept tail without cloning it.
        self.history.splice(..cut, std::iter::once(summary_msg));
        self.persist().await?;
        Ok(cut)
    }

    async fn persist(&self) -> Result<(), anyhow::Error> {
        self.storage
            .save_session(&self.id, &self.history, Some(&self.start_dir))
            .await
    }
}

/// Close out tool calls orphaned by a turn the user interrupted with Ctrl+C.
///
/// Cancellation drops the in-flight `prompt` future (the runner's
/// `tokio::select!` against the cancel channel). If the drop lands inside
/// `Agent::execute_tool_calls`, the trailing assistant message is left holding
/// `tool_calls` whose matching `tool_result`s were never pushed (the results
/// are appended only after every tool finishes). Providers — Anthropic
/// strictly — reject a `tool_use` with no `tool_result`, so the *next* prompt
/// would send an invalid history and fail. Before resuming, synthesize an
/// `is_error` "interrupted" result for each unanswered call.
///
/// No-op when the history is already balanced. Only the final assistant
/// tool-turn can be orphaned: a completed turn always pushed all its results,
/// and any `user`/`assistant` message after the turn means we are already past
/// it. The partial-interrupt case (some results pushed before the cancel) is
/// handled by filling only the ids not already answered.
fn heal_interrupted_tool_calls(history: &mut Vec<Message>) {
    let Some(a) = history
        .iter()
        .rposition(|m| m.role == "assistant" && m.tool_calls.is_some())
    else {
        return;
    };
    if history[a + 1..]
        .iter()
        .any(|m| m.role == "user" || m.role == "assistant")
    {
        return;
    }
    let calls = history[a].tool_calls.as_ref().unwrap();
    let answered: std::collections::HashSet<&str> = history[a + 1..]
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let missing: Vec<crate::ToolCall> = calls
        .iter()
        .filter(|tc| !answered.contains(tc.id.as_str()))
        .cloned()
        .collect();
    for tc in missing {
        let envelope = serde_json::json!({
            "result": "Tool call interrupted by user (Ctrl+C) before it completed.",
            "is_error": true,
        });
        history.push(
            Message {
                role: "tool".to_string(),
                content: Some(envelope.to_string()),
                reasoning_content: None,
                name: Some(tc.function.name),
                tool_call_id: Some(tc.id),
                tool_calls: None,
                created_at_ms: None,
            }
            .stamp_now(),
        );
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
    fn sanitize(comp: &str) -> String {
        comp.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }

    let mut parts = Vec::new();
    for comp in cwd.components() {
        let s = match comp {
            std::path::Component::Normal(os) => sanitize(&os.to_string_lossy()),
            std::path::Component::Prefix(p) => sanitize(&p.as_os_str().to_string_lossy()),
            _ => continue,
        };
        if !s.is_empty() {
            parts.push(s);
        }
    }

    let mut slug = parts.join("-");
    if slug.is_empty() {
        return "unknown".to_string();
    }

    // The root separator is gone because we built the slug from components.
    // If the first real path component starts with '-' or '.', escape it so
    // the directory name is not treated as a shell flag or hidden file.
    if slug.starts_with('-') || slug.starts_with('.') {
        slug.insert(0, '%');
    }
    slug
}

pub fn project_sessions_dir(root: &Path, cwd: &Path) -> PathBuf {
    root.join("projects").join(project_slug(cwd))
}

/// Legacy slug used before leading dashes were trimmed. Kept so existing
/// session directories can be migrated to the new slug on first access.
fn legacy_project_slug(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Merge legacy session files into the current project sessions directory.
/// Entries whose names already exist in `new_dir` are left in place in `legacy`
/// to avoid overwriting newer sessions. A session file and its `.usage.json`
/// sidecar are treated as a single unit: the sidecar is only migrated when the
/// corresponding session file is migrated, preventing legacy usage data from
/// attaching to a newer session that happens to share the same ID.
fn merge_legacy_sessions(legacy: &Path, new_dir: &Path) -> std::io::Result<()> {
    if !new_dir.exists() {
        std::fs::create_dir_all(new_dir)?;
    }

    let entries: Vec<_> = std::fs::read_dir(legacy)?.collect::<Result<_, _>>()?;
    let mut migrated_stems = HashSet::new();

    // First pass: migrate session files (.jsonl / .json) and record migrated IDs.
    for entry in &entries {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.ends_with(".usage.json") {
            continue;
        }
        let dst = new_dir.join(&name);
        if !dst.exists() {
            std::fs::rename(entry.path(), &dst)?;
            if let Some(stem) = Path::new(name_str).file_stem().and_then(|s| s.to_str()) {
                migrated_stems.insert(stem.to_string());
            }
        }
    }

    // Second pass: migrate usage sidecars only when their session file moved too.
    for entry in &entries {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.ends_with(".usage.json") {
            continue;
        }
        let base = &name_str[..name_str.len() - ".usage.json".len()];
        if !migrated_stems.contains(base) {
            continue;
        }
        let dst = new_dir.join(&name);
        if !dst.exists() {
            std::fs::rename(entry.path(), &dst)?;
        }
    }

    std::fs::remove_dir(legacy)
}

/// Return the current project sessions directory, migrating any legacy
/// leading-dash directory into it. If both directories already exist, legacy
/// entries are merged in without overwriting newer entries.
pub fn project_sessions_dir_with_migration(root: &Path, cwd: &Path) -> PathBuf {
    let new_dir = project_sessions_dir(root, cwd);
    let legacy = root.join("projects").join(legacy_project_slug(cwd));
    if legacy.exists() {
        let outcome = if !new_dir.exists() {
            std::fs::rename(&legacy, &new_dir)
        } else {
            merge_legacy_sessions(&legacy, &new_dir)
        };
        if let Err(e) = outcome {
            tracing::warn!(
                "failed to migrate legacy session dir {} to {}: {e}",
                legacy.display(),
                new_dir.display()
            );
        }
    }
    new_dir
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
            .map(|c| truncate_preview(c.trim(), 80))
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

/// Truncate `s` to at most `max` characters (not bytes), appending `…` when it
/// was shortened. Char-based so a multibyte first message (CJK/emoji) can't
/// panic the `/sessions` list by slicing mid-codepoint (#181).
fn truncate_preview(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
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
        let preview = truncate_preview(&s.preview, 40);
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
        assert_eq!(project_slug(Path::new("/home/zht/ignis")), "home-zht-ignis");
        assert_eq!(project_slug(Path::new("/tmp/with space")), "tmp-with-space");
    }

    #[test]
    fn project_slug_escapes_leading_dashes_and_dots() {
        // The root separator no longer leaks a leading dash.  If the first real
        // path component starts with '-' or '.', escape it so the directory is
        // not treated as a shell flag or hidden file.
        assert_eq!(project_slug(Path::new("/-foo/bar")), "%-foo-bar");
        assert_eq!(project_slug(Path::new("/...foo")), "%...foo");
        // Normal paths are unchanged.
        assert_eq!(project_slug(Path::new("/foo/bar")), "foo-bar");
        assert_ne!(
            project_slug(Path::new("/-foo/bar")),
            project_slug(Path::new("/foo/bar"))
        );
        // Trailing separators from path components are preserved.
        assert_eq!(project_slug(Path::new("/foo/bar-")), "foo-bar-");
    }

    #[test]
    #[cfg(windows)]
    fn project_slug_preserves_windows_drive_prefix() {
        // Paths on different drives must not collide.
        assert_eq!(
            project_slug(Path::new("C:\\Users\\me\\proj")),
            project_slug(Path::new("C:\\Users\\me\\proj"))
        );
        assert_ne!(
            project_slug(Path::new("C:\\Users\\me\\proj")),
            project_slug(Path::new("D:\\Users\\me\\proj"))
        );
    }

    #[test]
    fn project_sessions_dir_scopes_sessions_by_cwd() {
        let root = PathBuf::from("/tmp/ignis-sessions");
        assert_eq!(
            project_sessions_dir(&root, Path::new("/home/zht/ignis")),
            PathBuf::from("/tmp/ignis-sessions/projects/home-zht-ignis")
        );
    }

    #[test]
    fn project_sessions_dir_migrates_legacy_dir() {
        let root = crate::util::unique_temp_dir("ignis-session-migrate");
        let cwd = Path::new("/home/zht/ignis");
        let legacy = root.join("projects").join(legacy_project_slug(cwd));
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("default.jsonl"), b"{}\n").unwrap();

        let new_dir = project_sessions_dir_with_migration(&root, cwd);
        assert!(new_dir.exists());
        assert!(!legacy.exists());
        assert!(new_dir.join("default.jsonl").exists());
    }

    #[test]
    fn project_sessions_dir_merges_legacy_dir_when_new_exists() {
        let root = crate::util::unique_temp_dir("ignis-session-merge");
        let cwd = Path::new("/home/zht/ignis");
        let legacy = root.join("projects").join(legacy_project_slug(cwd));
        let new_dir = project_sessions_dir(&root, cwd);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(legacy.join("old.jsonl"), b"{}\n").unwrap();
        std::fs::write(new_dir.join("new.jsonl"), b"{}\n").unwrap();

        let result = project_sessions_dir_with_migration(&root, cwd);
        assert_eq!(result, new_dir);
        assert!(!legacy.exists());
        assert!(new_dir.join("old.jsonl").exists());
        assert!(new_dir.join("new.jsonl").exists());
    }

    #[test]
    fn project_sessions_dir_merge_skips_name_collisions() {
        let root = crate::util::unique_temp_dir("ignis-session-merge-collision");
        let cwd = Path::new("/home/zht/ignis");
        let legacy = root.join("projects").join(legacy_project_slug(cwd));
        let new_dir = project_sessions_dir(&root, cwd);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(legacy.join("default.jsonl"), b"legacy\n").unwrap();
        std::fs::write(new_dir.join("default.jsonl"), b"current\n").unwrap();

        let result = project_sessions_dir_with_migration(&root, cwd);
        assert_eq!(result, new_dir);
        assert!(legacy.exists());
        assert!(legacy.join("default.jsonl").exists());
        assert_eq!(
            std::fs::read_to_string(new_dir.join("default.jsonl")).unwrap(),
            "current\n"
        );
    }

    #[test]
    fn project_sessions_dir_merge_keeps_usage_sidecar_with_colliding_session() {
        let root = crate::util::unique_temp_dir("ignis-session-merge-usage-collision");
        let cwd = Path::new("/home/zht/ignis");
        let legacy = root.join("projects").join(legacy_project_slug(cwd));
        let new_dir = project_sessions_dir(&root, cwd);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(legacy.join("default.jsonl"), b"legacy\n").unwrap();
        std::fs::write(legacy.join("default.usage.json"), b"legacy usage\n").unwrap();
        std::fs::write(new_dir.join("default.jsonl"), b"current\n").unwrap();
        std::fs::write(new_dir.join("default.usage.json"), b"current usage\n").unwrap();

        let result = project_sessions_dir_with_migration(&root, cwd);
        assert_eq!(result, new_dir);
        // The legacy session file and its usage sidecar must stay together.
        assert!(legacy.exists());
        assert!(legacy.join("default.jsonl").exists());
        assert!(legacy.join("default.usage.json").exists());
        assert_eq!(
            std::fs::read_to_string(new_dir.join("default.jsonl")).unwrap(),
            "current\n"
        );
        assert_eq!(
            std::fs::read_to_string(new_dir.join("default.usage.json")).unwrap(),
            "current usage\n"
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
                created_at_ms: None,
            },
            crate::Message {
                role: "assistant".to_string(),
                content: Some("hi".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
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
            reasoning_tokens: 0,
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

    fn call(id: &str, name: &str) -> crate::ToolCall {
        crate::ToolCall {
            id: id.to_string(),
            r#type: "function".to_string(),
            function: crate::ToolCallFunction {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn assistant_calls(calls: Vec<crate::ToolCall>) -> Message {
        Message {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(calls),
            created_at_ms: None,
        }
    }

    fn tool_result(id: &str, name: &str, is_error: bool) -> Message {
        Message {
            role: "tool".to_string(),
            content: Some(format!("{{\"result\":\"ok\",\"is_error\":{is_error}}}")),
            reasoning_content: None,
            name: Some(name.to_string()),
            tool_call_id: Some(id.to_string()),
            tool_calls: None,
            created_at_ms: None,
        }
    }

    fn tool_ids(history: &[Message]) -> Vec<&str> {
        history
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect()
    }

    /// The Ctrl+C bug: a turn interrupted while tools were running leaves the
    /// assistant `tool_calls` with no matching `tool_result`s. Heal must close
    /// every orphaned call so the next prompt sends a valid call→result chain.
    #[test]
    fn heal_synthesizes_results_for_every_interrupted_call() {
        let mut history = vec![
            Message {
                role: "user".to_string(),
                content: Some("go".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
            assistant_calls(vec![call("c1", "bash"), call("c2", "read")]),
        ];

        heal_interrupted_tool_calls(&mut history);

        assert_eq!(tool_ids(&history), vec!["c1", "c2"]);
        for m in history.iter().filter(|m| m.role == "tool") {
            let v: serde_json::Value = serde_json::from_str(m.content.as_deref().unwrap()).unwrap();
            assert_eq!(v["is_error"], serde_json::json!(true));
        }
    }

    /// Partial interrupt: one tool finished and pushed its result before the
    /// cancel. Heal must fill only the missing call and keep the real result.
    #[test]
    fn heal_fills_only_missing_results() {
        let mut history = vec![
            assistant_calls(vec![call("c1", "bash"), call("c2", "read")]),
            tool_result("c1", "bash", false),
        ];

        heal_interrupted_tool_calls(&mut history);

        assert_eq!(tool_ids(&history), vec!["c1", "c2"]);
        let c1: serde_json::Value =
            serde_json::from_str(history[1].content.as_deref().unwrap()).unwrap();
        assert_eq!(c1["is_error"], serde_json::json!(false)); // real result kept
        let c2: serde_json::Value =
            serde_json::from_str(history[2].content.as_deref().unwrap()).unwrap();
        assert_eq!(c2["is_error"], serde_json::json!(true)); // synthesized
    }

    /// A fully balanced history (normal completion) must be left untouched.
    #[test]
    fn heal_is_noop_when_balanced() {
        let mut history = vec![
            assistant_calls(vec![call("c1", "bash")]),
            tool_result("c1", "bash", false),
        ];
        let before = history.clone();
        heal_interrupted_tool_calls(&mut history);
        assert_eq!(history.len(), before.len());
        assert_eq!(tool_ids(&history), vec!["c1"]);
    }

    /// A completed prior tool-turn followed by a later user message must not be
    /// retro-healed — it is already past and balanced.
    #[test]
    fn heal_ignores_earlier_completed_tool_turns() {
        let mut history = vec![
            assistant_calls(vec![call("c1", "bash")]),
            tool_result("c1", "bash", false),
            Message {
                role: "user".to_string(),
                content: Some("next".to_string()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        ];
        let before_len = history.len();
        heal_interrupted_tool_calls(&mut history);
        assert_eq!(history.len(), before_len);
    }

    #[test]
    fn truncate_preview_is_char_safe_on_multibyte() {
        // 100 CJK chars (3 bytes each = 300 bytes); a byte slice at 80 would
        // land mid-codepoint and panic.
        let s = "中".repeat(100);
        let out = truncate_preview(&s, 80);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 81, "80 chars plus the ellipsis");
        // An emoji straddling the boundary is likewise safe.
        let emoji = "😀".repeat(50);
        let out2 = truncate_preview(&emoji, 40);
        assert_eq!(out2.chars().filter(|&c| c == '😀').count(), 40);
        // Short input passes through unchanged with no ellipsis.
        assert_eq!(truncate_preview("hi", 80), "hi");
    }

    /// End-to-end: a session whose first user message is multibyte-heavy must
    /// list without panicking the byte-slice in `read_session_meta`/`print_sessions`
    /// (#181) — the same class as the fixed `get_git_diff` startup crash.
    #[tokio::test]
    async fn list_does_not_panic_on_multibyte_first_message() {
        let dir = crate::util::unique_temp_dir("ignis-sessions-multibyte");
        std::fs::create_dir_all(&dir).unwrap();
        let storage = FileStorage::new(dir.clone());

        let msg = Message {
            role: "user".to_string(),
            content: Some("中".repeat(100)),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: Some(1_000_000),
        };
        storage
            .save_session("multibyte-sess", &[msg], Some("/proj"))
            .await
            .unwrap();

        let manager = SessionManager::new(dir.clone());
        let sessions = manager.list(); // would panic before the fix
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].preview.chars().count() <= 81);

        // The CLI table re-truncates the preview at 40 — also byte-sliced before.
        print_sessions(&sessions);

        std::fs::remove_dir_all(&dir).ok();
    }
}
