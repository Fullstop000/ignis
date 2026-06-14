use std::path::PathBuf;
use std::time::Instant;

use super::composer::Composer;
use super::connect::{ConnectAdvance, ConnectFlow, ConnectOutcome, ConnectResult};
use super::{
    format_elapsed, next_selection, slash_suggestions, SelectionDirection, SlashCommand, SPINNERS,
    THINKING_VERBS,
};
use crate::AgentEvent;

/// Clipboard function type: takes text, returns Ok/Err.
type ClipFn = for<'a> fn(&'a str) -> Result<(), String>;

/// Decode a persisted tool message's content `{"result": <str>, "is_error": <bool>}`
/// back into the (display text, is_error) the live UI shows. Falls back to the
/// raw string if it isn't the expected JSON shape.
pub(crate) fn parse_tool_result(content: &str) -> (String, bool) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(result) = v.get("result").and_then(|r| r.as_str()) {
            let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            return (result.to_string(), is_error);
        }
    }
    (content.to_string(), false)
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) status: ToolStatus,
    pub(crate) started_at: Instant,
    pub(crate) elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub(crate) enum ToolStatus {
    Pending,
    Success(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub(crate) enum UIBlock {
    User(String),
    Assistant(String),
    /// Streamed chain-of-thought (`reasoning_content` from OpenAI-compatible
    /// providers like DeepSeek-Reasoner and o-series). Rendered separately
    /// from Assistant so it can carry its own dimmed styling and a
    /// "✻ Thinking" header instead of being silently glued onto the reply.
    Reasoning(String),
    Tool(ToolCallEntry),
}

/// List = the session table; Detail = drill-in for one session showing turn
/// waterfall + token/tool rollups. Right pushes List→Detail; Left/Esc pops back.
#[derive(Debug, Clone)]
pub(crate) enum SessionPickerMode {
    List,
    // Boxed because SessionDetail is ~232 B vs the bare List variant — clippy
    // flags the size mismatch.
    Detail(Box<crate::cli::sessions::SessionDetail>),
}

#[derive(Debug, Clone)]
pub(crate) struct SessionPicker {
    pub(crate) sessions: Vec<crate::cli::sessions::SessionRecord>,
    pub(crate) selected: usize,
    pub(crate) mode: SessionPickerMode,
    pub(crate) projects_dir: std::path::PathBuf,
}

impl SessionPicker {
    pub(crate) fn new(
        sessions: Vec<crate::cli::sessions::SessionRecord>,
        projects_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            sessions,
            selected: 0,
            mode: SessionPickerMode::List,
            projects_dir,
        }
    }

    pub(crate) fn select(&mut self, direction: SelectionDirection) {
        // Navigation only applies to the list view; in Detail the keys are owned
        // by the detail panel (scroll later if needed).
        if !matches!(self.mode, SessionPickerMode::List) {
            return;
        }
        // `next_selection` no-ops on empty — no extra guard needed.
        self.selected = next_selection(self.selected, self.sessions.len(), direction);
    }

    pub(crate) fn selected_id(&self) -> Option<String> {
        self.sessions
            .get(self.selected)
            .map(|s| s.session_id.clone())
    }

    pub(crate) fn is_detail(&self) -> bool {
        matches!(self.mode, SessionPickerMode::Detail(_))
    }

    /// Push from List → Detail. Loads the highlighted session's per-turn
    /// detail from disk; if there's no persisted JSONL (synthetic current row,
    /// freshly started session), falls back to a synthetic `SessionDetail` so
    /// the panel still opens — empty rather than silently no-op.
    pub(crate) fn enter_detail(&mut self) {
        if self.is_detail() {
            return;
        }
        let Some(record) = self.sessions.get(self.selected) else {
            return;
        };
        let detail = crate::cli::sessions::load_session_detail(
            &self.projects_dir,
            &record.project_slug,
            &record.session_id,
        )
        .unwrap_or_else(|| crate::cli::sessions::SessionDetail {
            record: record.clone(),
            turns: Vec::new(),
        });
        self.mode = SessionPickerMode::Detail(Box::new(detail));
    }

    /// Pop from Detail → List. No-op if already in List (the keys.rs branch
    /// then closes the picker entirely instead).
    pub(crate) fn exit_detail(&mut self) {
        if self.is_detail() {
            self.mode = SessionPickerMode::List;
        }
    }
}

/// `/skills` picker state. Rows come from `App.skills` registry `all()`.
#[derive(Debug, Clone)]
pub(crate) struct SkillPicker {
    pub(crate) selected: usize,
    /// Transient one-line confirmation shown in the footer after `r` reloads
    /// the registry (e.g. `↻ reloaded — 7 skills`). Cleared on navigation.
    pub(crate) status: Option<String>,
}

impl SkillPicker {
    /// Open over a non-empty registry; returns `None` (so the caller can show
    /// a notice) when no skills are configured.
    pub(crate) fn open(registry: &crate::skills::SkillRegistry) -> Option<Self> {
        (!registry.is_empty()).then_some(Self {
            selected: 0,
            status: None,
        })
    }

    pub(crate) fn select(&mut self, direction: SelectionDirection, total: usize) {
        self.status = None;
        self.selected = next_selection(self.selected, total, direction);
    }

    /// Toggle the highlighted skill on the registry; returns `(name, now_enabled)`
    /// for the post-toggle notice.
    pub(crate) fn toggle(&self, registry: &crate::skills::SkillRegistry) -> Option<(String, bool)> {
        let name = registry.all().get(self.selected)?.name.clone();
        let now_enabled = registry.toggle(&name);
        Some((name, now_enabled))
    }
}

/// `/mcp` picker state. Rows come from `App.mcp` registry `entries()` —
/// includes connected, failed, and disabled servers in stable name order.
#[derive(Debug, Clone)]
pub(crate) struct McpPicker {
    pub(crate) selected: usize,
}

impl McpPicker {
    /// Open over a non-empty registry; returns `None` when no servers are
    /// configured so the caller can show the "add one with `ignis mcp add`"
    /// notice instead of an empty picker.
    pub(crate) fn open(registry: &crate::mcp::McpRegistry) -> Option<Self> {
        (!registry.is_empty()).then_some(Self { selected: 0 })
    }

    pub(crate) fn select(&mut self, direction: SelectionDirection, total: usize) {
        self.selected = next_selection(self.selected, total, direction);
    }

    /// Toggle the highlighted MCP server on the registry; returns
    /// `(name, now_enabled)`.
    pub(crate) fn toggle(&self, registry: &crate::mcp::McpRegistry) -> Option<(String, bool)> {
        let name = registry.entries().get(self.selected)?.name.clone();
        let now_enabled = registry.toggle(&name);
        Some((name, now_enabled))
    }
}

/// `/model` picker state. Options live on `App.model_options`; this tracks the
/// highlighted row and, for a reasoning-capable model, the chosen effort level.
#[derive(Debug, Clone)]
pub(crate) struct ModelPicker {
    pub(crate) selected: usize,
    /// Index into the selected option's `effort_levels` (ignored if empty).
    pub(crate) effort_idx: usize,
}

impl ModelPicker {
    /// Open the picker preselecting the currently active provider/model/effort
    /// (falls back to row 0 / level 0 when no match). Returns `None` when there
    /// are no options to show.
    pub(crate) fn open(
        options: &[crate::llm::ModelOption],
        provider: &str,
        model: &str,
        effort: Option<&str>,
    ) -> Option<Self> {
        if options.is_empty() {
            return None;
        }
        let selected = options
            .iter()
            .position(|o| o.provider == provider && o.model == model)
            .unwrap_or(0);
        let effort_idx = effort
            .and_then(|e| options[selected].effort_levels.iter().position(|l| l == e))
            .unwrap_or(0);
        Some(Self {
            selected,
            effort_idx,
        })
    }

    /// Move the highlighted model; resets effort to 0 when the new model has
    /// fewer levels than the current effort_idx points at.
    pub(crate) fn select(
        &mut self,
        direction: SelectionDirection,
        options: &[crate::llm::ModelOption],
    ) {
        if options.is_empty() {
            return;
        }
        self.selected = next_selection(self.selected, options.len(), direction);
        let levels = options[self.selected].effort_levels.len();
        if self.effort_idx >= levels {
            self.effort_idx = 0;
        }
    }

    /// Cycle the effort level within the currently highlighted model.
    pub(crate) fn cycle_effort(
        &mut self,
        direction: SelectionDirection,
        options: &[crate::llm::ModelOption],
    ) {
        let levels = options
            .get(self.selected)
            .map(|o| o.effort_levels.len())
            .unwrap_or(0);
        if levels == 0 {
            return;
        }
        self.effort_idx = next_selection(self.effort_idx, levels, direction);
    }

    /// Resolve the picker's selection into `(provider, model, effort)` for the
    /// caller to apply. Effort is `None` when the model declares no levels. The
    /// caller retargets the context gauge from `model_options` separately, so
    /// the window isn't surfaced here.
    pub(crate) fn resolve(
        &self,
        options: &[crate::llm::ModelOption],
    ) -> Option<(String, String, Option<String>)> {
        let opt = options.get(self.selected)?;
        let effort = if opt.effort_levels.is_empty() {
            None
        } else {
            opt.effort_levels.get(self.effort_idx).cloned()
        };
        Some((opt.provider.clone(), opt.model.clone(), effort))
    }
}

/// Which tab the `/settings` control panel is showing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SettingsTab {
    Stats,
    Statusline,
}

impl SettingsTab {
    /// Tab order, used to cycle with `←`/`→`/Tab.
    const ORDER: [SettingsTab; 2] = [SettingsTab::Stats, SettingsTab::Statusline];
}

/// Footer segments the user can show/hide via `/settings` → Statusline, in
/// footer render order. The AFK/HANDS-FREE mode badge and the update notice
/// are intentionally absent — always shown (safety / transient).
pub(crate) const STATUSLINE_SEGMENTS: [(&str, &str); 5] = [
    ("cwd", "working directory"),
    ("git", "git branch"),
    ("turns", "turns"),
    ("model", "model"),
    ("tokens", "tokens / context %"),
];

/// `/settings` control panel. Stats is a read-only live view of the session;
/// Statusline toggles which footer segments show (Space/Enter, persisted
/// immediately like `/skills`).
#[derive(Debug, Clone)]
pub(crate) struct SettingsPanel {
    pub(crate) tab: SettingsTab,
    /// Highlighted segment row on the Statusline tab.
    pub(crate) statusline_idx: usize,
}

impl SettingsPanel {
    pub(crate) fn open() -> Self {
        Self {
            tab: SettingsTab::Stats,
            statusline_idx: 0,
        }
    }

    /// Cycle the active tab (`←`/`→`/Tab).
    pub(crate) fn switch_tab(&mut self, direction: SelectionDirection) {
        let cur = SettingsTab::ORDER
            .iter()
            .position(|t| *t == self.tab)
            .unwrap_or(0);
        self.tab = SettingsTab::ORDER[next_selection(cur, SettingsTab::ORDER.len(), direction)];
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Idle,
    Thinking,    // LLM is generating
    ToolRunning, // tool execution in progress
}

pub(crate) struct App {
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    /// Active reasoning effort level (shown in the footer); `None` = not set.
    pub(crate) effort: Option<String>,
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,

    pub(crate) blocks: Vec<UIBlock>,
    /// The text composer: input buffer, cursor, paste chips, input history.
    pub(crate) composer: Composer,
    pub(crate) slash_selection: usize,
    pub(crate) session_picker: Option<SessionPicker>,
    /// Choices for the `/model` picker, flattened from config.
    pub(crate) model_options: Vec<crate::llm::ModelOption>,
    /// models.dev windows, keyed by model id — the fallback source for an active
    /// model that isn't listed in `model_options` (e.g. a config `model` naming
    /// a provider model ignis hasn't baked and the user hasn't declared). See
    /// [`App::context_window`].
    pub(crate) model_catalog: crate::llm::ModelCatalog,
    pub(crate) model_picker: Option<ModelPicker>,
    pub(crate) skill_picker: Option<SkillPicker>,
    pub(crate) mcp_picker: Option<McpPicker>,
    /// Tool-initiated picker (the `ask_user` tool). Set by the main loop when
    /// a `PickerRequest` arrives on `picker_rx`; cleared after the user picks
    /// or cancels. Unlike the slash pickers, this one is active *while the
    /// agent is running* (Thinking/ToolRunning), and keys go to it
    /// regardless of mode. After picker close the trace gets committed
    /// through the normal `UIBlock::Tool` flush path
    /// (`block_lines` → `ask_user_resume_trace`) — no separate live channel.
    pub(crate) inline_picker: Option<super::inline_picker::InlinePickerState>,
    /// `/connect` provider-setup wizard. Active while the multi-step picker is
    /// open (provider → API key → model). The picker-completion path in
    /// `keys.rs` checks `connect.is_active()` to route picker answers back into
    /// `advance_connect` instead of treating them as tool-initiated answers.
    pub(crate) connect: ConnectFlow,

    pub(crate) mode: Mode,
    pub(crate) tick: u64,
    pub(crate) stream_start: Option<Instant>,
    pub(crate) current_chunk_idx: Option<usize>,
    /// Output chars streamed in the current turn (for live token estimate).
    pub(crate) stream_chars: usize,
    /// Real token usage from the most recent completed turn (provider-reported).
    pub(crate) last_usage: Option<crate::Usage>,

    /// Number of leading blocks already pushed into the terminal's native
    /// scrollback (via `insert_before`). The rest are still mutating; the
    /// in-progress block streams its stable rows out incrementally and the
    /// block is fully committed once `block_done(idx)` is true.
    pub(crate) committed: usize,
    /// Visual rows of the in-progress block (`blocks[committed]`) already
    /// streamed into scrollback. Reset to 0 whenever `committed` is reset, so
    /// the runner's incremental commit can never desync after `/clear`.
    pub(crate) committed_rows: usize,
    /// Set on a session reset (`/clear`, `/resume`). The runner wipes the
    /// terminal screen + scrollback history and re-anchors the band before
    /// committing the new session's blocks, so old output doesn't linger in
    /// scroll-up history. Cleared by the runner once handled.
    pub(crate) pending_screen_clear: bool,

    /// Reasoning verbosity, toggled live by Ctrl+O. `false` (default) collapses
    /// chain-of-thought: while a thought streams it shows as a fixed 3-line
    /// rolling preview (not committed), and finalizes to a one-line
    /// `✻ Thinking … (N more lines, ctrl+o to expand)` breadcrumb. `true` streams
    /// the full thought into scrollback. Session-only; resets to collapsed each
    /// run. Flipping it re-commits the whole transcript in the new mode.
    pub(crate) reasoning_expanded: bool,

    pub(crate) should_quit: bool,
    pub(crate) error_flash: Option<(String, Instant)>,
    pub(crate) exit_pending: bool,

    /// Window to use when the active model declares no context (the compaction
    /// threshold). The active model's actual window is derived on demand from
    /// `model_options` — see [`App::context_window`] — so it can't drift out of
    /// sync with the selection.
    pub(crate) fallback_context_window: usize,

    /// Prompts typed while busy, waiting to fire after the current turn (FIFO).
    pub(crate) queue: Vec<String>,
    /// Inject messages sent to the live turn but not yet confirmed via
    /// `UserInjected`; reconciled on `TurnEnd` (leftovers → front of `queue`).
    pub(crate) pending_injects: Vec<String>,
    /// Set by `handle_event` on `TurnEnd`; the main loop drains one queued item
    /// per turn-end (edge-triggered, never level-triggered on `mode == Idle`).
    pub(crate) turn_just_ended: bool,
    /// True from the moment a prompt/compact is dispatched until its `TurnEnd`.
    /// Used to tell a real turn-end (drain the queue) from a stray/duplicate
    /// `TurnEnd` — `mode` can't, since an early failure ends a turn that never
    /// reached `TurnStart` (still `Idle`).
    pub(crate) turn_in_flight: bool,

    /// Clipboard function, injectable for testing.
    pub(crate) clipboard_fn: ClipFn,

    /// Skill registry for slash autocomplete; `None` when no skills are loaded.
    pub(crate) skills: Option<std::sync::Arc<crate::skills::SkillRegistry>>,
    /// MCP registry for `/mcp` picker; `None` when no MCP servers are configured.
    pub(crate) mcp: Option<std::sync::Arc<crate::mcp::McpRegistry>>,
    /// Shared permission state — read by `/permissions` and `/afk` pickers
    /// (chunk 3) and by the TUI status-badge renderer. `None` only in legacy
    /// tests that construct `App` directly.
    pub(crate) permissions: Option<std::sync::Arc<crate::permissions::runtime::PermissionState>>,
    /// Auto-update-check result. `None` until the background check resolves;
    /// once `Some`, the footer renders a "new version available" segment.
    pub(crate) update_notice: Option<crate::cli::upgrade::UpdateNotice>,
    /// Current git branch of `cwd` (oh-my-zsh-style footer segment), or `None`
    /// when cwd isn't in a work tree. Computed at startup and refreshed on each
    /// turn boundary so a mid-session `git checkout` is reflected.
    pub(crate) git_branch: Option<String>,
    /// Shared external-subprocess hook registry — used by `/hooks reload`
    /// and (clone-handed) by the assistant-render seam in `runner.rs`.
    pub(crate) hooks: Option<crate::hooks::HookRegistry>,
    /// Compact text to render for the next committed user prompt, in place of
    /// the prompt actually sent to the model. Set when a `/skill-name` command
    /// expands its full body into the prompt — the transcript shows the typed
    /// command, the model still receives the body. `take()`-consumed on commit;
    /// cleared at turn-end so a hook-blocked turn can't carry it into the next.
    pub(crate) pending_user_display: Option<String>,

    /// `/settings` control panel (Stats | Statusline); `None` when closed.
    pub(crate) settings_panel: Option<SettingsPanel>,
    /// Cumulative provider-reported token usage across the whole session
    /// (summed from every `AgentEvent::Usage`). `last_usage` keeps only the
    /// latest turn; this is what the Stats tab totals. Reset on `/clear`.
    pub(crate) cumulative_usage: crate::Usage,
    /// When the current session started — for the Stats tab uptime. Reset on
    /// `/clear` (`start_new_session`).
    pub(crate) session_start: Instant,
    /// Footer segments hidden via `/settings` → Statusline (segment ids).
    /// Loaded from `state.json` at startup; empty = every segment shown.
    pub(crate) statusline_hidden: Vec<String>,
}

impl App {
    pub(crate) fn new(
        provider: Option<String>,
        model: Option<String>,
        session_id: String,
        cwd: PathBuf,
    ) -> Self {
        let git_branch = super::git::branch(&cwd);
        Self {
            provider,
            model,
            effort: None,
            session_id,
            cwd,
            blocks: Vec::new(),
            composer: Composer::default(),
            slash_selection: 0,
            session_picker: None,
            model_options: Vec::new(),
            model_catalog: crate::llm::ModelCatalog::default(),
            model_picker: None,
            skill_picker: None,
            mcp_picker: None,
            inline_picker: None,
            connect: ConnectFlow::default(),
            mode: Mode::Idle,
            tick: 0,
            stream_start: None,
            current_chunk_idx: None,
            stream_chars: 0,
            last_usage: None,
            committed: 0,
            committed_rows: 0,
            pending_screen_clear: false,
            reasoning_expanded: false,
            should_quit: false,
            error_flash: None,
            exit_pending: false,
            fallback_context_window: 120_000,
            queue: Vec::new(),
            pending_injects: Vec::new(),
            turn_just_ended: false,
            turn_in_flight: false,
            clipboard_fn: super::clipboard::set_clipboard,
            skills: None,
            mcp: None,
            permissions: None,
            update_notice: None,
            git_branch,
            hooks: None,
            pending_user_display: None,
            settings_panel: None,
            cumulative_usage: crate::Usage::default(),
            session_start: Instant::now(),
            statusline_hidden: Vec::new(),
        }
    }

    /// Recompute the cached git branch from `cwd`. Called on each turn
    /// boundary so the footer tracks a mid-session `git checkout`.
    pub(crate) fn refresh_git_branch(&mut self) {
        self.git_branch = super::git::branch(&self.cwd);
    }

    /// The active model's context window — the token budget the usage % is
    /// measured against. Resolved on demand (config override → baked spec →
    /// models.dev) so it can't drift out of sync with the selection the way a
    /// cached copy each model switch had to refresh did:
    /// - `model_options` covers every UI-switchable model (the picker/connect
    ///   build it, folding config overrides, baked specs, and models.dev);
    /// - the models.dev `model_catalog` covers an active model that isn't listed
    ///   there — e.g. a config `model` naming an un-baked, undeclared provider
    ///   model that only models.dev knows;
    /// - the compaction threshold is the last-resort fallback.
    fn context_window(&self) -> usize {
        self.model_options
            .iter()
            .find(|o| {
                Some(o.provider.as_str()) == self.provider.as_deref()
                    && Some(o.model.as_str()) == self.model.as_deref()
            })
            .and_then(|o| o.context)
            .or_else(|| {
                self.model
                    .as_deref()
                    .and_then(|m| self.model_catalog.context_for(m))
            })
            .map(|c| c as usize)
            .unwrap_or(self.fallback_context_window)
    }

    /// Estimated tokens used by the whole transcript (chars/4). Estimate, not
    /// the provider's exact count.
    pub(crate) fn context_tokens(&self) -> usize {
        let chars: usize = self
            .blocks
            .iter()
            .map(|b| match b {
                UIBlock::User(t) | UIBlock::Assistant(t) | UIBlock::Reasoning(t) => t.len(),
                UIBlock::Tool(e) => {
                    e.arguments.len()
                        + match &e.status {
                            ToolStatus::Success(s) | ToolStatus::Error(s) => s.len(),
                            ToolStatus::Pending => 0,
                        }
                }
            })
            .sum();
        chars / 4
    }

    /// Context tokens and % for the footer. Prefers the provider's real
    /// last-turn input size; falls back to the chars/4 estimate.
    pub(crate) fn context_usage(&self) -> (u64, u8) {
        let tokens = match self.last_usage {
            Some(u) if u.input_tokens > 0 => u.input_tokens,
            _ => self.context_tokens() as u64,
        };
        let pct = (tokens as usize * 100)
            .checked_div(self.context_window())
            .map(|p| p.min(100))
            .unwrap_or(0) as u8;
        (tokens, pct)
    }

    /// Estimated output tokens streamed so far in the current turn (chars/4).
    pub(crate) fn stream_tokens(&self) -> usize {
        self.stream_chars / 4
    }

    /// Estimated output tokens/second for the current turn (0 until ~0.5s in).
    pub(crate) fn stream_rate(&self) -> usize {
        match self.stream_start {
            Some(t) => {
                let secs = t.elapsed().as_secs_f64();
                if secs >= 0.5 {
                    (self.stream_tokens() as f64 / secs).round() as usize
                } else {
                    0
                }
            }
            None => 0,
        }
    }

    /// Current "Thinking" verb, cycling every 3s while generating.
    pub(crate) fn thinking_label(&self) -> &'static str {
        let secs = self
            .stream_start
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        THINKING_VERBS[(secs as usize / 3) % THINKING_VERBS.len()]
    }

    pub(crate) fn spinner(&self) -> &str {
        SPINNERS[(self.tick as usize / 10) % SPINNERS.len()]
    }

    pub(crate) fn elapsed_str(&self) -> String {
        match self.stream_start {
            Some(t) => format_elapsed(t.elapsed().as_millis()),
            None => String::new(),
        }
    }

    pub(crate) fn handle_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TurnStart => {
                self.mode = Mode::Thinking;
                self.stream_start = Some(Instant::now());
                self.stream_chars = 0;
            }
            AgentEvent::RunStart => {}
            AgentEvent::MessageStart { message } => {
                // Contract with the agent loop: `reasoning_content: Some +
                // content: None` opens a Reasoning block; anything else
                // (including the degenerate `None`/`None` and `Some`/`Some`
                // shapes) falls back to a regular Assistant block. Picking a
                // default rather than rejecting unknown shapes lets future
                // protocol additions degrade gracefully into the assistant
                // lane instead of crashing.
                let is_reasoning = message.reasoning_content.is_some() && message.content.is_none();
                self.blocks.push(if is_reasoning {
                    UIBlock::Reasoning(String::new())
                } else {
                    UIBlock::Assistant(String::new())
                });
                self.current_chunk_idx = Some(self.blocks.len() - 1);
            }
            AgentEvent::MessageUpdate { delta } => {
                self.stream_chars += delta.len();
                if let Some(i) = self.current_chunk_idx {
                    match self.blocks.get_mut(i) {
                        Some(UIBlock::Assistant(ref mut s))
                        | Some(UIBlock::Reasoning(ref mut s)) => {
                            s.push_str(&delta);
                        }
                        // current_chunk_idx is set only by MessageStart,
                        // which always pushes Assistant or Reasoning — so a
                        // Tool or User block here means the agent loop
                        // forgot to close the message block before pushing
                        // something else. Log rather than silently swallow.
                        _ => log::debug!(
                            "MessageUpdate dropped: current_chunk_idx={i} doesn't point at \
                             an Assistant/Reasoning block (delta len={})",
                            delta.len()
                        ),
                    }
                }
            }
            AgentEvent::MessageEnd { .. } => {
                self.current_chunk_idx = None;
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                self.mode = Mode::ToolRunning;
                self.blocks.push(UIBlock::Tool(ToolCallEntry {
                    id: tool_call_id,
                    name: tool_name,
                    arguments,
                    status: ToolStatus::Pending,
                    started_at: Instant::now(),
                    elapsed_ms: 0,
                }));
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
            } => {
                for b in self.blocks.iter_mut().rev() {
                    if let UIBlock::Tool(ref mut t) = b {
                        if t.id == tool_call_id {
                            t.elapsed_ms = t.started_at.elapsed().as_millis();
                            t.status = if result.is_error {
                                ToolStatus::Error(result.content)
                            } else {
                                ToolStatus::Success(result.content)
                            };
                            break;
                        }
                    }
                }
                self.mode = Mode::Thinking;
            }
            AgentEvent::RunEnd => {}
            AgentEvent::Usage(usage) => {
                // Real provider-reported usage: `last_usage` tracks the latest
                // turn (drives the context gauge); `cumulative_usage` totals the
                // session (drives the Stats tab).
                self.last_usage = Some(usage);
                self.cumulative_usage.add(&usage);
            }
            AgentEvent::TurnEnd => {
                self.mode = Mode::Idle;
                self.current_chunk_idx = None;
                self.stream_start = None;
                // Only the TurnEnd of a turn we actually dispatched drains the
                // queue. This catches both a duplicate TurnEnd (e.g. a persist
                // error after the agent loop already emitted one) and a turn that
                // ended *before* TurnStart (provider-build / session-open error,
                // which still leaves `mode == Idle`).
                if self.turn_in_flight {
                    self.turn_in_flight = false;
                    self.turn_just_ended = true;
                    // Any inject that lost the end-of-turn race fires next turn.
                    if !self.pending_injects.is_empty() {
                        let mut stranded = std::mem::take(&mut self.pending_injects);
                        stranded.append(&mut self.queue);
                        self.queue = stranded;
                    }
                }
            }
            AgentEvent::Reconnecting {
                attempt,
                max,
                reason,
            } => {
                // The agent dropped a stream and is re-issuing the same request.
                // Render as a dim notice line in scrollback so the user sees
                // *why* there's a pause — borrowing the same `UIBlock::Assistant`
                // path the /copy notices use. The model's `history` doesn't
                // include UI blocks, so this never leaks into the prompt.
                self.blocks.push(UIBlock::Assistant(format!(
                    "↻ Reconnecting ({attempt}/{max}): {reason}"
                )));
            }
            AgentEvent::UserInjected { text } => {
                // Events arrive on one ordered channel and the agent emits
                // `UserInjected` in the same FIFO order injects were sent, so the
                // front of `pending_injects` is this confirmation. (Empty when an
                // inject was drained at run-start, not via Ctrl+S — still shown.)
                if !self.pending_injects.is_empty() {
                    self.pending_injects.remove(0);
                }
                self.push_user_prompt(text);
            }
            AgentEvent::UserPromptCommitted { text } => {
                // Fired by `Session::prompt` after the `UserPromptSubmit`
                // hook chain runs. This is the authoritative text that went
                // into history — render it so scrollback matches what the
                // model received. With no hooks installed, `text` equals
                // what the user typed; the user perceives no delay.
                //
                // A `/skill-name` command is the exception: it commits the full
                // skill body to history but sets `pending_user_display` to the
                // typed command, so the transcript shows the compact invocation
                // instead of the expansion (CC/Codex behavior).
                let shown = self.pending_user_display.take().unwrap_or(text);
                self.push_user_prompt(shown);
            }
            AgentEvent::Warning { source, message } => {
                // Surfaces hook-chain failures (and any other ignis-side
                // soft-failure path that opts into Warning). Rendered as a
                // dim assistant notice prefixed `[warn]` so the line is
                // visible without burying the conversation.
                self.add_assistant_notice(format!("[warn] {source}: {message}"));
            }
        }
    }

    /// Whether block `i` is finalized and safe to flush to scrollback:
    /// user prompts always are; assistant blocks once they stop streaming; tool
    /// calls once they leave the pending state.
    pub(crate) fn block_done(&self, i: usize) -> bool {
        match self.blocks.get(i) {
            Some(UIBlock::User(_)) => true,
            Some(UIBlock::Assistant(_)) | Some(UIBlock::Reasoning(_)) => {
                self.current_chunk_idx != Some(i)
            }
            Some(UIBlock::Tool(t)) => !matches!(t.status, ToolStatus::Pending),
            None => false,
        }
    }

    /// Route a bracketed paste into the composer (a ≥4-line block collapses to a
    /// `[ pasted-text#N ]` chip), wrapping it with the cross-cutting exit-hint
    /// clear and slash-autocomplete reset that the composer itself doesn't own.
    pub(crate) fn handle_paste(&mut self, pasted: String) {
        self.clear_exit_hint();
        self.composer.paste(pasted);
        self.reset_slash_selection();
    }

    /// Push a user prompt into the transcript + input history (shared by submit
    /// and the queue drain). Does not send anything.
    pub(crate) fn push_user_prompt(&mut self, text: String) {
        self.exit_pending = false;
        self.composer.push_history(text.clone());
        self.blocks.push(UIBlock::User(text));
    }

    /// Queue a prompt typed while busy (no transcript block until it fires).
    pub(crate) fn enqueue(&mut self, text: String) {
        self.exit_pending = false;
        self.queue.push(text);
    }

    /// Pop the next queued prompt (FIFO) for the drain.
    pub(crate) fn take_queued_front(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    /// Move the most recent queued prompt back into the input for editing.
    pub(crate) fn recall_last_queued(&mut self) -> bool {
        match self.queue.pop() {
            Some(text) => {
                self.composer.set_text(text);
                true
            }
            None => false,
        }
    }

    /// Read and clear the edge-trigger flag set on `TurnEnd`.
    pub(crate) fn take_turn_just_ended(&mut self) -> bool {
        std::mem::take(&mut self.turn_just_ended)
    }

    pub(crate) fn submit(&mut self) -> Option<String> {
        if self.mode != Mode::Idle {
            return None;
        }
        // `take_submit` trims, swaps paste chips back for their full content
        // (the agent must see what was pasted, not the placeholder), and clears
        // the buffer. We don't push the user block from the typed text —
        // `Session::prompt` emits `UserPromptCommitted` after the hook chain so
        // the visible block matches what hit history.
        self.composer.take_submit()
    }

    pub(crate) fn add_assistant_notice(&mut self, text: String) {
        self.exit_pending = false;
        self.session_picker = None;
        self.blocks.push(UIBlock::Assistant(text));
    }

    pub(crate) fn start_new_session(&mut self, session_id: String) {
        self.exit_pending = false;
        self.session_id = session_id;
        self.blocks.clear();
        self.committed = 0;
        self.reset_transcript_view();
        self.current_chunk_idx = None;
        self.composer.reset_history_browse();
        self.last_usage = None;
        self.cumulative_usage = crate::Usage::default();
        self.session_start = Instant::now();
        self.pending_user_display = None;
        self.session_picker = None;
        self.add_assistant_notice(format!("Started new session `{}`.", self.session_id));
    }

    /// Reset the incremental-commit cursor on session reset paths (`/clear`,
    /// `/resume`) so the new session's blocks stream out from row 0, and ask
    /// the runner to wipe the terminal screen + scrollback first.
    fn reset_transcript_view(&mut self) {
        self.committed_rows = 0;
        self.pending_screen_clear = true;
    }

    /// Flip collapsed↔expanded reasoning (Ctrl+O) and re-commit the whole
    /// transcript in the new mode. Reuses the `/resume` re-anchor: rewind the
    /// commit cursor to row 0 and ask the runner to wipe + repaint, so every
    /// past thought re-renders collapsed or full — the "(ctrl+o to expand)" hint
    /// is honest. `blocks` (with full reasoning text) is untouched.
    pub(crate) fn toggle_reasoning_expanded(&mut self) {
        self.reasoning_expanded = !self.reasoning_expanded;
        self.committed = 0;
        self.reset_transcript_view();
    }

    /// The in-progress reasoning text when a collapsed live preview should own
    /// its display — a `Reasoning` block is streaming and we're not expanded.
    /// `None` when expanded, or the current block isn't reasoning. Drives both
    /// the preview region's height (`reasoning_preview_height`) and its content.
    pub(crate) fn live_reasoning(&self) -> Option<&str> {
        if self.reasoning_expanded {
            return None;
        }
        match self.current_chunk_idx.and_then(|i| self.blocks.get(i)) {
            Some(UIBlock::Reasoning(t)) => Some(t.as_str()),
            _ => None,
        }
    }

    pub(crate) fn show_session_picker(
        &mut self,
        sessions: Vec<crate::cli::sessions::SessionRecord>,
        projects_dir: std::path::PathBuf,
    ) {
        self.exit_pending = false;
        self.session_picker = Some(SessionPicker::new(sessions, projects_dir));
    }

    pub(crate) fn select_session_picker(&mut self, direction: SelectionDirection) {
        self.exit_pending = false;
        if let Some(p) = &mut self.session_picker {
            p.select(direction);
        }
    }

    pub(crate) fn selected_session_id(&self) -> Option<String> {
        self.session_picker.as_ref().and_then(|p| p.selected_id())
    }

    /// Supply the `/model` picker choices and the active effort level.
    pub(crate) fn set_model_options(
        &mut self,
        options: Vec<crate::llm::ModelOption>,
        effort: Option<String>,
    ) {
        self.model_options = options;
        self.effort = effort;
    }

    /// `/connect` — start the multi-step provider-setup flow. Returns the first
    /// picker (provider selection); the caller (`keys.rs`) sends it over
    /// `picker_tx`. The runner installs it as `app.inline_picker`, the user
    /// picks, and the picker-completion path routes back here via
    /// [`Self::advance_connect`]. Thin coordinator over [`ConnectFlow::start`]:
    /// it supplies the App-side inputs (is a picker open? the current provider)
    /// and turns a "picker already open" refusal into a notice + `None`.
    pub(crate) fn start_connect(&mut self) -> Option<crate::console::picker::PickerRequest> {
        self.exit_pending = false;
        let current = self.provider.clone();
        match self.connect.start(self.inline_picker.is_some(), current) {
            Ok(req) => Some(req),
            Err(notice) => {
                self.add_assistant_notice(notice);
                None
            }
        }
    }

    /// Drive the `/connect` flow one step forward given the picker's answer.
    /// Coordinator over [`ConnectFlow::advance`]: it emits the flow's notices
    /// and, on either success, rebuilds the `/model` list so the connected
    /// provider's models appear in-session; a `Switched` result also adopts the
    /// new `provider`/`model` (clearing `effort`). The agent loop gets a separate
    /// `ReloadConfig` request from `keys.rs` on `Saved`.
    pub(crate) fn advance_connect(
        &mut self,
        answers: Vec<crate::console::picker::PickerAnswer>,
    ) -> ConnectAdvance {
        let current = self.provider.clone().zip(self.model.clone());
        let current_ref = current.as_ref().map(|(p, m)| (p.as_str(), m.as_str()));
        match self.connect.advance(answers, current_ref) {
            ConnectOutcome::NextPicker(req) => ConnectAdvance::NextPicker(req),
            ConnectOutcome::Done { notices, result } => {
                for notice in notices {
                    self.add_assistant_notice(notice);
                }
                match result {
                    ConnectResult::Switched(provider, model) => {
                        self.provider = Some(provider);
                        self.model = Some(model);
                        self.effort = None;
                        self.rebuild_model_options();
                        ConnectAdvance::Saved
                    }
                    ConnectResult::KeptCurrent => {
                        self.rebuild_model_options();
                        ConnectAdvance::Saved
                    }
                    ConnectResult::Failed => ConnectAdvance::Failed,
                }
            }
        }
    }

    /// Rebuild the `/model` list from the freshly-written config so a
    /// newly-connected provider's whole model catalog is selectable in-session,
    /// without a restart. Mirrors the startup path and the agent loop's
    /// `ReloadConfig`.
    fn rebuild_model_options(&mut self) {
        let catalog = crate::llm::catalog::load();
        match crate::config::load_config() {
            Ok(cfg) => self.model_options = cfg.model_options(&catalog),
            Err(e) => log::error!("/connect: model list not refreshed: {e}"),
        }
    }

    /// Discard the in-flight `/connect` draft and emit a one-line notice.
    /// Called from `keys.rs` when the user presses Esc/Ctrl-C while a connect
    /// picker is open. A cancel with no draft in flight is a silent no-op.
    pub(crate) fn cancel_connect(&mut self) {
        if let Some(notice) = self.connect.cancel() {
            self.add_assistant_notice(notice);
        }
    }

    pub(crate) fn show_model_picker(&mut self) {
        self.exit_pending = false;
        match ModelPicker::open(
            &self.model_options,
            self.provider.as_deref().unwrap_or(""),
            self.model.as_deref().unwrap_or(""),
            self.effort.as_deref(),
        ) {
            Some(p) => self.model_picker = Some(p),
            None => self.add_assistant_notice("No models configured.".to_string()),
        }
    }

    pub(crate) fn select_model_picker(&mut self, direction: SelectionDirection) {
        self.exit_pending = false;
        if let Some(p) = &mut self.model_picker {
            p.select(direction, &self.model_options);
        }
    }

    pub(crate) fn cycle_effort(&mut self, direction: SelectionDirection) {
        self.exit_pending = false;
        if let Some(p) = &mut self.model_picker {
            p.cycle_effort(direction, &self.model_options);
        }
    }

    /// Apply the highlighted selection: update the displayed provider/model/effort,
    /// close the picker, and return `(provider, model, effort)` to act on.
    pub(crate) fn apply_model_selection(&mut self) -> Option<(String, String, Option<String>)> {
        let picker = self.model_picker.take()?;
        let (provider, model, effort) = picker.resolve(&self.model_options)?;
        self.provider = Some(provider.clone());
        self.model = Some(model.clone());
        self.effort = effort.clone();
        Some((provider, model, effort))
    }

    pub(crate) fn show_skill_picker(&mut self) {
        self.exit_pending = false;
        match self.skills.as_deref().and_then(SkillPicker::open) {
            Some(p) => self.skill_picker = Some(p),
            None => self.add_assistant_notice(
                "No skills found. Add one at ~/.ignis/skills/<name>/SKILL.md".to_string(),
            ),
        }
    }

    /// Re-scan the skill roots from disk and swap in a fresh registry,
    /// preserving the user's enable/disable choices (re-read from `state.json`).
    /// Returns the new skill count. If the picker is open, clamps its selection
    /// to the new list and sets a one-line "reloaded" status. `home` is threaded
    /// in (rather than read here) so tests can point at a temp dir; the UI side
    /// updated here is paired with an `AgentRequest::ReloadSkills` to the runner
    /// — its registry clone is otherwise left stale.
    pub(crate) fn reload_skills(&mut self, home: Option<&std::path::Path>) -> usize {
        let disabled: std::collections::HashSet<String> = crate::state::load_state()
            .disabled_skills
            .into_iter()
            .collect();
        let registry = crate::skills::SkillRegistry::load(home, &self.cwd, disabled);
        let count = registry.all().len();
        self.skills = Some(std::sync::Arc::new(registry));
        if let Some(p) = &mut self.skill_picker {
            p.selected = p.selected.min(count.saturating_sub(1));
            p.status = Some(format!("↻ reloaded — {count} skills"));
        }
        count
    }

    pub(crate) fn select_skill_picker(&mut self, direction: SelectionDirection) {
        let total = self.skills.as_deref().map(|r| r.all().len()).unwrap_or(0);
        if let Some(p) = &mut self.skill_picker {
            p.select(direction, total);
        }
    }

    /// Toggle the highlighted skill. Returns `(name, now_enabled)` for a notice.
    pub(crate) fn toggle_selected_skill(&mut self) -> Option<(String, bool)> {
        self.skill_picker
            .as_ref()
            .zip(self.skills.as_deref())
            .and_then(|(p, reg)| p.toggle(reg))
    }

    pub(crate) fn show_mcp_picker(&mut self) {
        self.exit_pending = false;
        match self.mcp.as_deref().and_then(McpPicker::open) {
            Some(p) => self.mcp_picker = Some(p),
            None => self.add_assistant_notice(
                "No MCP servers configured. Add one with `ignis mcp add <name> -- <cmd> [args]`."
                    .to_string(),
            ),
        }
    }

    pub(crate) fn select_mcp_picker(&mut self, direction: SelectionDirection) {
        let total = self.mcp.as_deref().map(|r| r.len()).unwrap_or(0);
        if let Some(p) = &mut self.mcp_picker {
            p.select(direction, total);
        }
    }

    /// Toggle the highlighted MCP server. Returns `(name, now_enabled)`.
    pub(crate) fn toggle_selected_mcp_server(&mut self) -> Option<(String, bool)> {
        self.mcp_picker
            .as_ref()
            .zip(self.mcp.as_deref())
            .and_then(|(p, reg)| p.toggle(reg))
    }

    pub(crate) fn show_settings_panel(&mut self) {
        self.exit_pending = false;
        self.settings_panel = Some(SettingsPanel::open());
    }

    /// Switch the settings panel's active tab (Tab / Left / Right).
    pub(crate) fn settings_switch_tab(&mut self, direction: SelectionDirection) {
        if let Some(p) = &mut self.settings_panel {
            p.switch_tab(direction);
        }
    }

    /// Move the cursor on the active tab: segment row on Statusline; no-op on
    /// the read-only Stats tab.
    pub(crate) fn settings_move(&mut self, direction: SelectionDirection) {
        if let Some(p) = &mut self.settings_panel {
            if p.tab == SettingsTab::Statusline {
                p.statusline_idx =
                    next_selection(p.statusline_idx, STATUSLINE_SEGMENTS.len(), direction);
            }
        }
    }

    /// Toggle the highlighted footer segment on the Statusline tab and persist
    /// immediately (like `/skills`). No-op off the Statusline tab.
    pub(crate) fn settings_toggle_statusline(&mut self) {
        let idx = match &self.settings_panel {
            Some(p) if p.tab == SettingsTab::Statusline => p.statusline_idx,
            _ => return,
        };
        let Some((id, _)) = STATUSLINE_SEGMENTS.get(idx) else {
            return;
        };
        let id = (*id).to_string();
        if let Some(pos) = self.statusline_hidden.iter().position(|s| *s == id) {
            self.statusline_hidden.remove(pos);
        } else {
            self.statusline_hidden.push(id);
        }
        let _ = crate::state::persist_statusline_hidden(&self.statusline_hidden);
    }

    /// Whether a footer segment id is currently shown (not hidden). Read by the
    /// footer renderer.
    pub(crate) fn statusline_shows(&self, id: &str) -> bool {
        !self.statusline_hidden.iter().any(|s| s == id)
    }

    /// Close the settings panel (`Esc` / a typed char / Stats-tab `Enter`).
    pub(crate) fn close_settings(&mut self) {
        self.settings_panel = None;
    }

    /// User turns this session = committed user prompts.
    pub(crate) fn turn_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|b| matches!(b, UIBlock::User(_)))
            .count()
    }

    /// Total transcript blocks (user + assistant + reasoning + tool).
    pub(crate) fn message_count(&self) -> usize {
        self.blocks.len()
    }

    /// Per-tool call tally over the transcript, most-used first (ties by name).
    pub(crate) fn tool_tally(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for b in &self.blocks {
            if let UIBlock::Tool(e) = b {
                match counts.iter_mut().find(|(name, _)| *name == e.name) {
                    Some(slot) => slot.1 += 1,
                    None => counts.push((e.name.clone(), 1)),
                }
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        counts
    }

    /// Wall-clock time since the session started (for the Stats tab uptime).
    pub(crate) fn session_uptime(&self) -> std::time::Duration {
        self.session_start.elapsed()
    }

    pub(crate) fn render_session_history(
        &mut self,
        session_id: String,
        messages: Vec<crate::Message>,
    ) {
        self.exit_pending = false;
        self.session_id = session_id.clone();
        self.blocks.clear();
        self.committed = 0;
        self.reset_transcript_view();
        self.current_chunk_idx = None;
        self.session_picker = None;
        self.last_usage = None;
        // Stats track this live session instance: a resumed transcript's prior
        // token spend / uptime isn't replayed, so the Stats tab counts from the
        // resume forward (turns/tools are still rebuilt from `blocks`). Mirrors
        // `last_usage` resetting to None.
        self.cumulative_usage = crate::Usage::default();
        self.session_start = Instant::now();
        self.pending_user_display = None;

        if messages.is_empty() {
            self.add_assistant_notice(format!("Resumed empty session `{}`.", session_id));
            return;
        }

        for message in messages {
            match message.role.as_str() {
                "user" => {
                    if let Some(content) = message.content.filter(|c| !c.is_empty()) {
                        self.blocks.push(UIBlock::User(content));
                    }
                }
                "assistant" => {
                    // Push reasoning first, then the reply — matches the
                    // typical streaming order (reasoning chunks fully arrive
                    // before any text). Either may be missing or empty.
                    //
                    // Caveat for interleaved-thinking streams (Anthropic
                    // protocol via `interleaved-thinking-2025-05-14`): a
                    // live turn may render as reasoning₁ → text → reasoning₂
                    // as separate blocks, but the persisted Message
                    // collapses to one (content, reasoning_content) pair, so
                    // the resumed view shows one Reasoning + one Assistant.
                    // Deliberate trade-off — per-block storage records would
                    // cost every turn for an edge case OpenAI-compatible
                    // providers don't hit today.
                    if let Some(reasoning) = message.reasoning_content.filter(|r| !r.is_empty()) {
                        self.blocks.push(UIBlock::Reasoning(reasoning));
                    }
                    if let Some(content) = message.content.filter(|c| !c.is_empty()) {
                        self.blocks.push(UIBlock::Assistant(content));
                    }
                    // Reconstruct tool blocks from this turn's calls; the
                    // following `tool` messages fill in each result by id.
                    if let Some(tool_calls) = message.tool_calls {
                        for tc in tool_calls {
                            self.blocks.push(UIBlock::Tool(ToolCallEntry {
                                id: tc.id,
                                name: tc.function.name,
                                arguments: tc.function.arguments,
                                status: ToolStatus::Success(String::new()),
                                started_at: Instant::now(),
                                elapsed_ms: 0,
                            }));
                        }
                    }
                }
                "tool" => {
                    // Persisted tool content is {"result": <str>, "is_error": <bool>};
                    // parse it and attach to the matching pending tool block.
                    let (result, is_error) =
                        parse_tool_result(message.content.as_deref().unwrap_or(""));
                    let status = if is_error {
                        ToolStatus::Error(result)
                    } else {
                        ToolStatus::Success(result)
                    };
                    let idx = message.tool_call_id.as_deref().and_then(|id| {
                        self.blocks
                            .iter()
                            .rposition(|b| matches!(b, UIBlock::Tool(t) if t.id == id))
                    });
                    match idx {
                        Some(i) => {
                            if let UIBlock::Tool(t) = &mut self.blocks[i] {
                                t.status = status;
                            }
                        }
                        None => {
                            // Orphaned result (no matching call) — show it standalone.
                            self.blocks.push(UIBlock::Tool(ToolCallEntry {
                                id: String::new(),
                                name: message.name.unwrap_or_else(|| "tool".to_string()),
                                arguments: String::new(),
                                status,
                                started_at: Instant::now(),
                                elapsed_ms: 0,
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
        self.add_assistant_notice(format!("Resumed session `{}`.", session_id));
    }

    pub(crate) fn slash_suggestions(&self) -> Vec<SlashCommand> {
        slash_suggestions(&self.composer.input, self.skills.as_deref())
    }

    pub(crate) fn reset_slash_selection(&mut self) {
        let suggestions_len = self.slash_suggestions().len();
        if suggestions_len == 0 {
            self.slash_selection = 0;
        } else {
            self.slash_selection = self.slash_selection.min(suggestions_len - 1);
        }
    }

    pub(crate) fn select_slash_suggestion(&mut self, direction: SelectionDirection) -> bool {
        self.exit_pending = false;
        let suggestions = self.slash_suggestions();
        if suggestions.is_empty() {
            return false;
        }

        self.slash_selection = next_selection(self.slash_selection, suggestions.len(), direction);
        true
    }

    /// Returns the currently selected slash suggestion name, if any.
    pub(crate) fn selected_slash_command(&self) -> Option<String> {
        let suggestions = self.slash_suggestions();
        if suggestions.is_empty() {
            return None;
        }
        let idx = self.slash_selection.min(suggestions.len() - 1);
        Some(suggestions[idx].name.to_string())
    }

    pub(crate) fn history_prev(&mut self) {
        self.exit_pending = false;
        self.composer.history_prev();
    }

    pub(crate) fn history_next(&mut self) {
        self.exit_pending = false;
        self.composer.history_next();
    }

    /// Update pending tool elapsed times each tick
    pub(crate) fn tick_update(&mut self) {
        self.tick += 1;
        // Clear expired error flashes (3s)
        if let Some((_, t)) = &self.error_flash {
            if t.elapsed().as_secs() >= 3 {
                self.error_flash = None;
            }
        }
    }

    pub(crate) fn request_exit(&mut self) {
        if self.exit_pending {
            self.should_quit = true;
        } else {
            self.exit_pending = true;
        }
    }

    pub(crate) fn clear_exit_hint(&mut self) {
        self.exit_pending = false;
    }

    pub(crate) fn copy_last_assistant_message(&mut self) {
        let text = self.blocks.iter().rev().find_map(|b| match b {
            UIBlock::Assistant(text) => Some(text.clone()),
            _ => None,
        });

        match text {
            Some(content) => match (self.clipboard_fn)(&content) {
                Ok(()) => self.add_assistant_notice("Copied to clipboard.".to_string()),
                Err(e) => self.add_assistant_notice(format!("Copy failed: {e}")),
            },
            None => self.add_assistant_notice("Nothing to copy.".to_string()),
        }
    }
}

#[cfg(test)]
mod copy_tests {
    use super::{App, UIBlock};
    use std::path::PathBuf;

    /// A clipboard mock that always succeeds.
    fn mock_clipboard(_text: &str) -> Result<(), String> {
        Ok(())
    }

    fn test_app() -> App {
        let mut app = App::new(
            Some("p".to_string()),
            Some("m".to_string()),
            "s".to_string(),
            PathBuf::from("/tmp"),
        );
        app.clipboard_fn = mock_clipboard;
        app
    }

    #[test]
    fn copy_finds_last_assistant_message() {
        let mut app = test_app();
        app.blocks.push(UIBlock::User("user msg".to_string()));
        app.blocks
            .push(UIBlock::Assistant("first reply".to_string()));
        app.blocks.push(UIBlock::User("user msg 2".to_string()));
        app.blocks
            .push(UIBlock::Assistant("last reply".to_string()));

        app.copy_last_assistant_message();

        let last = app.blocks.last().unwrap();
        assert!(
            matches!(last, UIBlock::Assistant(text) if text == "Copied to clipboard."),
            "Expected success notice, got {:?}",
            last
        );
    }

    fn write_skill(skills_root: &std::path::Path, name: &str) {
        let dir = skills_root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill {name}\n---\nbody"),
        )
        .unwrap();
    }

    #[test]
    fn reload_skills_picks_up_new_skill_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_root = tmp.path().join(".ignis/skills");
        write_skill(&skills_root, "alpha");

        let mut app = test_app();
        app.cwd = tmp.path().to_path_buf();
        app.skills = Some(std::sync::Arc::new(crate::skills::SkillRegistry::load(
            None,
            &app.cwd,
            std::collections::HashSet::new(),
        )));
        app.show_skill_picker();
        assert_eq!(app.skills.as_deref().unwrap().all().len(), 1);

        // A new skill lands on disk after the picker is already open.
        write_skill(&skills_root, "beta");

        let count = app.reload_skills(None);

        assert_eq!(count, 2, "reload should re-scan disk and see the new skill");
        assert_eq!(app.skills.as_deref().unwrap().all().len(), 2);
        assert_eq!(
            app.skill_picker.as_ref().unwrap().status.as_deref(),
            Some("↻ reloaded — 2 skills"),
        );
    }

    #[test]
    fn reload_skills_clamps_selection_when_skills_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_root = tmp.path().join(".ignis/skills");
        for name in ["alpha", "beta", "gamma"] {
            write_skill(&skills_root, name);
        }

        let mut app = test_app();
        app.cwd = tmp.path().to_path_buf();
        app.skills = Some(std::sync::Arc::new(crate::skills::SkillRegistry::load(
            None,
            &app.cwd,
            std::collections::HashSet::new(),
        )));
        app.show_skill_picker();
        app.skill_picker.as_mut().unwrap().selected = 2; // last row

        std::fs::remove_dir_all(skills_root.join("beta")).unwrap();
        std::fs::remove_dir_all(skills_root.join("gamma")).unwrap();

        let count = app.reload_skills(None);

        assert_eq!(count, 1);
        assert_eq!(
            app.skill_picker.as_ref().unwrap().selected,
            0,
            "selection clamps to the new last row",
        );
    }

    #[test]
    fn copy_skips_tool_blocks() {
        let mut app = test_app();
        app.blocks.push(UIBlock::User("user msg".to_string()));
        app.blocks.push(UIBlock::Assistant("reply".to_string()));
        app.blocks.push(UIBlock::Tool(super::ToolCallEntry {
            id: "1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            status: super::ToolStatus::Success("ok".to_string()),
            started_at: std::time::Instant::now(),
            elapsed_ms: 0,
        }));

        app.copy_last_assistant_message();

        let last = app.blocks.last().unwrap();
        assert!(
            matches!(last, UIBlock::Assistant(text) if text == "Copied to clipboard."),
            "Expected copy of 'reply', got {:?}",
            last
        );
    }

    #[test]
    fn copy_when_nothing_to_copy() {
        let mut app = test_app();
        app.blocks.push(UIBlock::User("user only".to_string()));

        app.copy_last_assistant_message();

        let last = app.blocks.last().unwrap();
        assert!(
            matches!(last, UIBlock::Assistant(text) if text == "Nothing to copy."),
            "Expected 'Nothing to copy.' notice, got {:?}",
            last
        );
    }
}

#[cfg(test)]
mod connect_tests {
    //! State-machine tests for the `/connect` flow. The PERSIST step
    //! (`persist_connect`) touches `~/.ignis/config.toml` and `state.json`,
    //! so it's exercised separately by `config::tests::write_provider_key_*`
    //! and `state` tests — these tests cover everything up to that.
    use super::*;
    use crate::console::connect::ConnectStep;
    use crate::console::picker::PickerAnswer;
    use std::path::PathBuf;

    fn fresh_app() -> App {
        // No provider/model = no-provider mode, matching the typical
        // first-launch caller of /connect.
        App::new(None, None, "s".to_string(), PathBuf::from("/tmp"))
    }

    #[test]
    fn start_connect_creates_draft_and_returns_provider_picker() {
        let mut app = fresh_app();
        let req = app.start_connect().expect("should return a picker");
        // Draft is at step 1, all fields unset.
        let draft = app.connect.draft().expect("draft must exist");
        assert_eq!(draft.step, ConnectStep::PickProvider);
        assert!(draft.provider_id.is_none());
        assert!(draft.api_key.is_none());
        // Picker is the provider list — single-select, no text input, no
        // "Other" row (we don't want users free-texting provider names).
        let q = &req.questions[0];
        assert_eq!(q.kind, "connect");
        assert_eq!(q.header, "Provider");
        assert!(!q.multi_select);
        assert!(!q.text_input);
        assert!(!q.allow_other);
        // Every baked-in provider shows up — keeps the picker in lock-step
        // with `SPECS` (a new provider is discoverable without code here).
        assert_eq!(q.options.len(), crate::llm::providers::all().len());
    }

    #[test]
    fn start_connect_refuses_when_another_picker_is_open() {
        let mut app = fresh_app();
        // Fake an existing picker (simulate `/afk` being mid-flight).
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.inline_picker = Some(crate::console::inline_picker::InlinePickerState::new(
            crate::console::picker::PickerRequest {
                questions: vec![crate::console::picker::PickerQuestion {
                    question: "x".into(),
                    kind: "afk".into(),
                    header: "AFK".into(),
                    multi_select: false,
                    allow_other: false,
                    text_input: false,
                    mask: false,
                    options: vec![crate::console::picker::PickerOption {
                        label: "a".into(),
                        description: "b".into(),
                        preview: None,
                    }],
                }],
                reply: tx,
            },
        ));
        assert!(app.start_connect().is_none());
        assert!(app.connect.draft().is_none(), "no draft on refusal");
    }

    #[test]
    fn provider_step_advances_to_api_key_for_key_required_provider() {
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        // OpenAI requires an API key, so step 2 is the masked text input.
        let outcome = app.advance_connect(vec![PickerAnswer::Single("OpenAI".into())]);
        match outcome {
            ConnectAdvance::NextPicker(req) => {
                let q = &req.questions[0];
                assert!(q.text_input);
                assert!(q.mask);
                assert_eq!(q.header, "API Key");
            }
            other => panic!("expected NextPicker(api-key), got {other:?}"),
        }
        let draft = app.connect.draft().unwrap();
        assert_eq!(draft.step, ConnectStep::EnterApiKey);
        assert_eq!(draft.provider_id.as_deref(), Some("openai"));
    }

    #[test]
    fn provider_step_skips_api_key_for_keyless_provider() {
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        // Ollama has `api_key_required = false` — step 2 is the model picker.
        let outcome = app.advance_connect(vec![PickerAnswer::Single("Ollama (local)".into())]);
        match outcome {
            ConnectAdvance::NextPicker(req) => {
                assert_eq!(req.questions[0].header, "Model");
                assert!(!req.questions[0].text_input);
            }
            other => panic!("expected NextPicker(model), got {other:?}"),
        }
        assert_eq!(app.connect.draft().unwrap().step, ConnectStep::PickModel);
    }

    #[test]
    fn provider_step_bails_on_custom_with_pointer_to_config_file() {
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        let outcome = app.advance_connect(vec![PickerAnswer::Single(
            "Custom (OpenAI-compatible)".into(),
        )]);
        assert!(matches!(outcome, ConnectAdvance::Failed));
        // Draft cleared; user got a notice pointing at config.toml.
        assert!(app.connect.draft().is_none());
        let last = app
            .blocks
            .iter()
            .filter_map(|b| match b {
                UIBlock::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .next_back()
            .unwrap();
        assert!(last.contains("config.toml"));
        assert!(last.contains("config.example.toml"));
    }

    #[test]
    fn provider_step_failed_on_unknown_display_name() {
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        let outcome = app.advance_connect(vec![PickerAnswer::Single("Not A Real Provider".into())]);
        assert!(matches!(outcome, ConnectAdvance::Failed));
        assert!(app.connect.draft().is_none());
    }

    #[test]
    fn api_key_step_advances_to_model_picker_scoped_to_provider() {
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        let _ = app.advance_connect(vec![PickerAnswer::Single("DeepSeek".into())]);
        let outcome = app.advance_connect(vec![PickerAnswer::Single("sk-test".into())]);
        match outcome {
            ConnectAdvance::NextPicker(req) => {
                let q = &req.questions[0];
                assert_eq!(q.header, "Model");
                let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
                // Models come from DeepSeek's baked SPEC.
                assert!(labels.iter().any(|n| n.starts_with("deepseek-")));
            }
            other => panic!("expected NextPicker(model), got {other:?}"),
        }
        let draft = app.connect.draft().unwrap();
        assert_eq!(draft.step, ConnectStep::PickModel);
        assert_eq!(draft.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn model_picker_offers_keep_current_when_a_model_is_active() {
        // Re-connecting (a model is already active) — the model step leads with
        // a "keep current" row so the user can connect without switching away.
        let mut app = App::new(
            Some("deepseek".to_string()),
            Some("deepseek-v4-flash".to_string()),
            "s".to_string(),
            PathBuf::from("/tmp"),
        );
        let _ = app.start_connect().unwrap();
        let _ = app.advance_connect(vec![PickerAnswer::Single("OpenAI".into())]);
        let outcome = app.advance_connect(vec![PickerAnswer::Single("sk-test".into())]);
        match outcome {
            ConnectAdvance::NextPicker(req) => {
                let opts = &req.questions[0].options;
                assert_eq!(opts[0].label, "Keep current model");
                assert!(opts[0].description.contains("deepseek/deepseek-v4-flash"));
                // The provider's real models follow the keep row.
                assert!(opts.iter().skip(1).any(|o| o.label == "gpt-5.5"));
            }
            other => panic!("expected model picker, got {other:?}"),
        }
    }

    #[test]
    fn model_picker_has_no_keep_row_on_first_connect() {
        // No active model yet — nothing to keep, so only the provider's models.
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        let _ = app.advance_connect(vec![PickerAnswer::Single("OpenAI".into())]);
        let outcome = app.advance_connect(vec![PickerAnswer::Single("sk-test".into())]);
        match outcome {
            ConnectAdvance::NextPicker(req) => {
                let opts = &req.questions[0].options;
                assert!(opts.iter().all(|o| o.label != "Keep current model"));
                assert_eq!(opts[0].label, "gpt-5.5");
            }
            other => panic!("expected model picker, got {other:?}"),
        }
    }

    #[test]
    fn connect_draft_debug_redacts_api_key() {
        // The api_key must never appear in `Debug` output — `dbg!` or a
        // tracing span capturing `App` state would otherwise leak it.
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        let _ = app.advance_connect(vec![PickerAnswer::Single("DeepSeek".into())]);
        let _ = app.advance_connect(vec![PickerAnswer::Single("sk-supersecret".into())]);
        let draft = app.connect.draft().unwrap();
        let dbg = format!("{:?}", draft);
        assert!(
            !dbg.contains("sk-supersecret"),
            "Debug leaked api_key: {dbg}"
        );
        assert!(
            dbg.contains("***"),
            "Debug must show a redaction marker: {dbg}"
        );
    }

    #[test]
    fn cancel_connect_drops_draft_and_emits_notice() {
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        let _ = app.advance_connect(vec![PickerAnswer::Single("OpenAI".into())]);
        assert!(app.connect.is_active());
        app.cancel_connect();
        assert!(app.connect.draft().is_none());
        let last = app
            .blocks
            .iter()
            .filter_map(|b| match b {
                UIBlock::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .next_back()
            .unwrap();
        assert!(last.contains("cancelled"));
    }

    #[test]
    fn cancel_connect_with_no_draft_is_silent_noop() {
        let mut app = fresh_app();
        app.cancel_connect();
        assert!(app.connect.draft().is_none());
        // No notice was emitted (the assistant transcript stays empty).
        assert!(!app
            .blocks
            .iter()
            .any(|b| matches!(b, UIBlock::Assistant(_))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_reset_zeroes_the_streaming_commit_cursor() {
        // Regression: `/clear` while a reply was streaming must reset the
        // incremental-commit cursor in lockstep with `committed`, or the runner
        // skips the first rows of the new session's output.
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            std::path::PathBuf::from("/"),
        );
        app.committed = 7;
        app.committed_rows = 5;
        app.start_new_session("sess-2".into());
        assert_eq!(app.committed, 0);
        assert_eq!(
            app.committed_rows, 0,
            "streaming cursor must reset on /clear"
        );
    }

    #[test]
    fn slash_suggestions_show_all_commands_for_slash() {
        let suggestions = slash_suggestions("/", None);

        assert_eq!(
            suggestions
                .iter()
                .map(|command| command.name.as_ref())
                .collect::<Vec<_>>(),
            vec![
                "/sessions",
                "/clear",
                "/compact",
                "/copy",
                "/connect",
                "/model",
                "/skills",
                "/mcp",
                "/afk",
                "/telemetry",
                "/hooks",
                "/settings",
            ]
        );
    }

    #[test]
    fn slash_suggestions_filter_by_command_name_or_description() {
        // `/res` and `/list` both surface /sessions — name prefix and
        // description match, respectively.
        assert_eq!(
            slash_suggestions("/sess", None)[0].name.as_ref(),
            "/sessions"
        );
        assert_eq!(
            slash_suggestions("/list", None)[0].name.as_ref(),
            "/sessions"
        );
        // `/new` is merged into `/clear`: typing it still surfaces /clear via
        // its description ("Start a new session").
        assert_eq!(slash_suggestions("/new", None)[0].name.as_ref(), "/clear");
        assert_eq!(slash_suggestions("/clear", None)[0].name.as_ref(), "/clear");
    }

    #[test]
    fn slash_suggestions_stop_after_first_argument() {
        assert!(slash_suggestions("/sessions default", None).is_empty());
    }

    fn test_app() -> App {
        App::new(
            Some("p".to_string()),
            Some("m".to_string()),
            "s".to_string(),
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn context_window_tracks_active_model() {
        let mut app = test_app();
        app.fallback_context_window = 100_000;
        app.set_model_options(
            vec![
                crate::llm::ModelOption {
                    provider: "x".to_string(),
                    model: "big".to_string(),
                    effort_levels: vec![],
                    context: Some(1_000_000),
                },
                crate::llm::ModelOption {
                    provider: "x".to_string(),
                    model: "unknown".to_string(),
                    effort_levels: vec![],
                    context: None,
                },
            ],
            None,
        );
        // Switch to a known-context model → gauge uses its window.
        app.model_picker = Some(ModelPicker {
            selected: 0,
            effort_idx: 0,
        });
        app.apply_model_selection();
        assert_eq!(app.context_window(), 1_000_000);
        // Switch to an unknown-context model → gauge falls back, never sticking
        // to the previous model's window.
        app.model_picker = Some(ModelPicker {
            selected: 1,
            effort_idx: 0,
        });
        app.apply_model_selection();
        assert_eq!(app.context_window(), 100_000);
    }

    #[test]
    fn context_window_follows_connect_switch_without_explicit_refresh() {
        // Regression: the footer's gauge must follow a `/connect` model switch
        // (which sets `provider`/`model` + rebuilds the list but does NOT touch
        // any cached window) the same as the `/model` picker. Previously a cached
        // `context_window` left the % measured against the *prior* model's window
        // — the 120k compaction fallback — so a freshly-connected 1M model showed
        // `MiniMax-M3 · 107.5k tok (89%)` instead of ~11%. Deriving on demand
        // means simply adopting the selection is enough.
        let mut app = test_app();
        app.fallback_context_window = 120_000;
        app.set_model_options(
            vec![crate::llm::ModelOption {
                provider: "minimax-token-plan".to_string(),
                model: "MiniMax-M3".to_string(),
                effort_levels: vec![],
                context: Some(1_000_000),
            }],
            None,
        );
        // Exactly what the `Switched` arm does: adopt the new selection.
        app.provider = Some("minimax-token-plan".to_string());
        app.model = Some("MiniMax-M3".to_string());
        assert_eq!(app.context_window(), 1_000_000);
    }

    #[test]
    fn context_window_resolves_active_model_from_models_dev_when_unlisted() {
        // An active model that isn't in `model_options` (un-baked, undeclared)
        // but is known to models.dev must still use its real window, not the
        // compaction fallback — preserving the old `active_context` behavior.
        let mut app = test_app();
        app.fallback_context_window = 120_000;
        app.set_model_options(vec![], None);
        app.model_catalog = crate::llm::ModelCatalog::from_entries([("ad-hoc-model", 512_000)]);
        app.provider = Some("some-provider".to_string());
        app.model = Some("ad-hoc-model".to_string());
        assert_eq!(app.context_window(), 512_000);
        // Unknown to both → compaction fallback.
        app.model = Some("totally-unknown".to_string());
        assert_eq!(app.context_window(), 120_000);
    }

    fn picker_app() -> App {
        let mut app = App::new(
            Some("deepseek".to_string()),
            Some("deepseek-v4-flash".to_string()),
            "s".to_string(),
            PathBuf::from("/tmp"),
        );
        let opt = |provider: &str, model: &str, levels: &[&str]| crate::llm::ModelOption {
            provider: provider.to_string(),
            model: model.to_string(),
            effort_levels: levels.iter().map(|s| s.to_string()).collect(),
            context: None,
        };
        app.set_model_options(
            vec![
                opt("deepseek", "deepseek-v4-flash", &[]),
                opt("deepseek", "deepseek-v4-pro", &["high", "max"]),
                opt("kimi-code", "kimi-for-coding", &[]),
            ],
            None,
        );
        app
    }

    #[test]
    fn show_model_picker_highlights_active_model() {
        let mut app = picker_app();
        app.show_model_picker();
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn cycle_effort_wraps_and_is_inert_for_non_reasoning_models() {
        let mut app = picker_app();
        app.show_model_picker();
        // Move to deepseek-v4-pro (has levels high/max).
        app.select_model_picker(SelectionDirection::Next);
        assert_eq!(app.model_picker.as_ref().unwrap().selected, 1);
        app.cycle_effort(SelectionDirection::Next); // high -> max
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 1);
        app.cycle_effort(SelectionDirection::Next); // wraps max -> high
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 0);
        // Moving to kimi-code (no levels) clamps effort and cycling no-ops.
        app.cycle_effort(SelectionDirection::Next); // -> max (idx 1)
        app.select_model_picker(SelectionDirection::Next);
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 0);
        app.cycle_effort(SelectionDirection::Next);
        assert_eq!(app.model_picker.as_ref().unwrap().effort_idx, 0);
    }

    #[test]
    fn apply_model_selection_returns_choice_and_updates_display() {
        let mut app = picker_app();
        app.show_model_picker();
        app.select_model_picker(SelectionDirection::Next); // deepseek-v4-pro
        app.cycle_effort(SelectionDirection::Next); // max
        let (provider, model, effort) = app.apply_model_selection().unwrap();
        assert_eq!(
            (provider.as_str(), model.as_str(), effort.as_deref()),
            ("deepseek", "deepseek-v4-pro", Some("max"))
        );
        assert_eq!(app.provider, Some("deepseek".to_string()));
        assert_eq!(app.model, Some("deepseek-v4-pro".to_string()));
        assert_eq!(app.effort.as_deref(), Some("max"));
        assert!(app.model_picker.is_none());
    }

    #[test]
    fn apply_non_reasoning_model_clears_effort() {
        let mut app = picker_app();
        app.effort = Some("max".to_string()); // a stale prior effort
        app.show_model_picker(); // selected = deepseek-v4-flash (no levels)
        let (_, model, effort) = app.apply_model_selection().unwrap();
        assert_eq!(model, "deepseek-v4-flash");
        assert_eq!(effort, None);
        assert_eq!(app.effort, None);
    }

    #[test]
    fn editing_multibyte_chars_keeps_cursor_on_boundary() {
        // Regression: typing a CJK char (3 bytes) used to advance the cursor by
        // 1 byte, landing mid-character and panicking on the next slice/render.
        let mut app = test_app();
        for c in "中a文".chars() {
            app.composer.insert_char(c);
            assert!(
                app.composer.input.is_char_boundary(app.composer.cursor),
                "cursor must stay on a char boundary after insert"
            );
        }
        assert_eq!(app.composer.input, "中a文");
        assert_eq!(app.composer.cursor, app.composer.input.len());

        // Slicing at the cursor (what draw_input does) must not panic.
        let _ = &app.composer.input[..app.composer.cursor];

        // Walk left across every char, then delete them.
        for _ in 0..3 {
            app.composer.move_left();
            assert!(app.composer.input.is_char_boundary(app.composer.cursor));
        }
        assert_eq!(app.composer.cursor, 0);

        app.composer.cursor = app.composer.input.len();
        app.composer.backspace(); // removes "文"
        assert_eq!(app.composer.input, "中a");
        assert!(app.composer.input.is_char_boundary(app.composer.cursor));
        assert_eq!(app.composer.cursor, app.composer.input.len());
    }

    #[test]
    fn sanitize_expands_tabs_and_drops_control_chars() {
        use super::super::sanitize;
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("a\r\nb"), "ab"); // CR and LF dropped (lines split upstream)
        assert_eq!(sanitize("x\x1b[31my"), "x[31my"); // ESC dropped
        assert_eq!(sanitize("正常 text"), "正常 text"); // multibyte untouched
    }

    #[test]
    fn format_tokens_is_human_friendly() {
        use super::super::format_tokens;
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(120_000), "120.0k");
    }

    #[test]
    fn truncate_is_char_safe() {
        use super::super::truncate;
        // Must not panic slicing mid-codepoint, and counts chars not bytes.
        assert_eq!(truncate("中文字测试", 3), "中文字…");
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn next_selection_wraps_in_both_directions() {
        assert_eq!(next_selection(0, 2, SelectionDirection::Previous), 1);
        assert_eq!(next_selection(1, 2, SelectionDirection::Next), 0);
        assert_eq!(next_selection(0, 0, SelectionDirection::Next), 0);
    }

    #[test]
    fn enqueue_appends_without_touching_blocks() {
        let mut app = test_app();
        app.enqueue("first".to_string());
        app.enqueue("second".to_string());
        assert_eq!(app.queue, vec!["first", "second"]);
        // Queueing must NOT push a transcript block (ordering correctness).
        assert!(app.blocks.is_empty());
    }

    #[test]
    fn take_queued_front_is_fifo() {
        let mut app = test_app();
        app.enqueue("a".to_string());
        app.enqueue("b".to_string());
        assert_eq!(app.take_queued_front(), Some("a".to_string()));
        assert_eq!(app.take_queued_front(), Some("b".to_string()));
        assert_eq!(app.take_queued_front(), None);
    }

    #[test]
    fn recall_last_queued_moves_last_item_into_input() {
        let mut app = test_app();
        app.enqueue("older".to_string());
        app.enqueue("newest".to_string());
        assert!(app.recall_last_queued());
        assert_eq!(app.composer.input, "newest");
        assert_eq!(app.composer.cursor, app.composer.input.len());
        assert_eq!(app.queue, vec!["older"]);
        // Empty queue → no-op.
        app.composer.input.clear();
        app.queue.clear();
        assert!(!app.recall_last_queued());
        assert_eq!(app.composer.input, "");
    }

    #[test]
    fn agent_end_sets_flag_and_reconciles_pending_injects() {
        let mut app = test_app();
        app.turn_in_flight = true;
        app.mode = Mode::Thinking;
        app.pending_injects = vec!["stranded".to_string()];
        app.queue = vec!["queued".to_string()];
        app.handle_event(AgentEvent::TurnEnd);
        assert_eq!(app.mode, Mode::Idle);
        assert!(app.take_turn_just_ended());
        // Second take clears it.
        assert!(!app.take_turn_just_ended());
        // Stranded inject prepended ahead of existing queue.
        assert_eq!(app.queue, vec!["stranded", "queued"]);
        assert!(app.pending_injects.is_empty());
    }

    #[test]
    fn agent_end_drains_even_without_agent_start() {
        // A provider-build / session-open error ends the turn *before* TurnStart,
        // so `mode` is still Idle at TurnEnd. The drain must still fire (keyed on
        // turn_in_flight, not mode) or a queued prompt after a failed one stalls.
        let mut app = test_app();
        app.turn_in_flight = true; // dispatched, but no TurnStart arrived
        assert_eq!(app.mode, Mode::Idle);
        app.handle_event(AgentEvent::TurnEnd);
        assert!(
            app.take_turn_just_ended(),
            "a turn that failed before TurnStart still drains the queue"
        );
    }

    #[test]
    fn user_injected_pushes_block_records_history_pops_pending() {
        let mut app = test_app();
        app.pending_injects = vec!["steer".to_string()];
        app.handle_event(AgentEvent::UserInjected {
            text: "steer".to_string(),
        });
        assert!(matches!(app.blocks.last(), Some(UIBlock::User(t)) if t == "steer"));
        assert_eq!(
            app.composer.history.last().map(|s| s.as_str()),
            Some("steer")
        );
        assert!(app.pending_injects.is_empty());
    }

    #[test]
    fn duplicate_agent_end_does_not_double_drain() {
        // A persistence error after the agent loop already emitted TurnEnd can
        // produce a second TurnEnd. Only the first (dispatched turn) arms the
        // drain; the duplicate must not, or the queue would drain twice.
        let mut app = test_app();
        app.turn_in_flight = true;
        app.handle_event(AgentEvent::TurnEnd);
        assert!(app.take_turn_just_ended(), "real turn-end arms the drain");
        app.handle_event(AgentEvent::TurnEnd); // duplicate, no turn in flight
        assert!(
            !app.take_turn_just_ended(),
            "duplicate TurnEnd must not re-arm the drain"
        );
    }

    #[test]
    fn user_injected_with_empty_pending_still_pushes() {
        // An inject drained at run-start (not via Ctrl+S) has no pending entry,
        // but must still appear in the transcript + history without panicking.
        let mut app = test_app();
        assert!(app.pending_injects.is_empty());
        app.handle_event(AgentEvent::UserInjected {
            text: "from-start".to_string(),
        });
        assert!(matches!(app.blocks.last(), Some(UIBlock::User(t)) if t == "from-start"));
        assert_eq!(
            app.composer.history.last().map(|s| s.as_str()),
            Some("from-start")
        );
    }

    #[test]
    fn skill_commit_renders_typed_command_not_expanded_body() {
        // A /skill-name turn commits the full skill body to history but sets
        // `pending_user_display` to the typed command. The transcript must show
        // the compact invocation, not the expansion (CC/Codex behavior).
        let mut app = test_app();
        app.pending_user_display = Some("/brainstorming fix the bug".to_string());
        app.handle_event(AgentEvent::UserPromptCommitted {
            text: "The \"brainstorming\" skill is now active … (full body)".to_string(),
        });
        assert!(
            matches!(app.blocks.last(), Some(UIBlock::User(t)) if t == "/brainstorming fix the bug"),
            "transcript shows the typed command, not the skill body"
        );
        // Consumed, so the next normal prompt renders its own committed text.
        assert!(app.pending_user_display.is_none());
    }

    #[test]
    fn normal_commit_renders_committed_text_verbatim() {
        // Regression guard on the `unwrap_or`: with no override, the committed
        // (post-hook) text renders unchanged.
        let mut app = test_app();
        assert!(app.pending_user_display.is_none());
        app.handle_event(AgentEvent::UserPromptCommitted {
            text: "hello world".to_string(),
        });
        assert!(matches!(app.blocks.last(), Some(UIBlock::User(t)) if t == "hello world"));
    }

    #[test]
    fn message_start_with_reasoning_only_pushes_reasoning_block() {
        // A MessageStart whose Message carries reasoning_content but no
        // content is the agent loop's signal "this turn is opening a
        // thinking block" — the app must mint a Reasoning, not Assistant.
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        app.handle_event(AgentEvent::MessageStart {
            message: crate::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: Some(String::new()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        });
        assert!(matches!(app.blocks.last(), Some(UIBlock::Reasoning(s)) if s.is_empty()));
    }

    #[test]
    fn message_start_with_content_pushes_assistant_block() {
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        app.handle_event(AgentEvent::MessageStart {
            message: crate::Message {
                role: "assistant".to_string(),
                content: Some(String::new()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        });
        assert!(matches!(app.blocks.last(), Some(UIBlock::Assistant(s)) if s.is_empty()));
    }

    #[test]
    fn message_update_appends_to_reasoning_block() {
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        app.handle_event(AgentEvent::MessageStart {
            message: crate::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: Some(String::new()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        });
        app.handle_event(AgentEvent::MessageUpdate {
            delta: "hmm let me think".to_string(),
        });
        assert!(
            matches!(app.blocks.last(), Some(UIBlock::Reasoning(s)) if s == "hmm let me think")
        );
    }

    fn reasoning_msg() -> crate::Message {
        crate::Message {
            role: "assistant".to_string(),
            content: None,
            reasoning_content: Some(String::new()),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }
    }

    #[test]
    fn ctrl_o_toggles_reasoning_and_rewinds_commit() {
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        app.committed = 5;
        assert!(!app.reasoning_expanded);
        app.toggle_reasoning_expanded();
        assert!(app.reasoning_expanded);
        // Rewinds the commit cursor + asks the runner to wipe & repaint, so every
        // past thought re-renders in the new mode (the /resume re-anchor).
        assert_eq!(app.committed, 0);
        assert!(app.pending_screen_clear);
        app.toggle_reasoning_expanded();
        assert!(!app.reasoning_expanded, "flips back");
    }

    #[test]
    fn live_reasoning_tracks_streaming_collapsed_thought() {
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        assert_eq!(app.live_reasoning(), None, "no thought yet");

        app.handle_event(AgentEvent::MessageStart {
            message: reasoning_msg(),
        });
        app.handle_event(AgentEvent::MessageUpdate {
            delta: "weighing options".to_string(),
        });
        assert_eq!(app.live_reasoning(), Some("weighing options"));

        // Expanded mode suppresses the preview (the full thought streams instead).
        app.reasoning_expanded = true;
        assert_eq!(app.live_reasoning(), None);
        app.reasoning_expanded = false;

        // Once the thought finalizes there's no live preview to own.
        app.handle_event(AgentEvent::MessageEnd {
            message: reasoning_msg(),
        });
        assert_eq!(app.live_reasoning(), None);
    }

    #[test]
    fn interleaved_reasoning_text_reasoning_yields_three_blocks() {
        // Streaming order: reasoning₁ → text → reasoning₂. Each kind change
        // must close the previous block and open a new one — the user should
        // see three distinct UIBlocks in the order they streamed in, not
        // glued together.
        let mut app = App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        );
        let reasoning_start = || AgentEvent::MessageStart {
            message: crate::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: Some(String::new()),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        };
        let text_start = || AgentEvent::MessageStart {
            message: crate::Message {
                role: "assistant".to_string(),
                content: Some(String::new()),
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        };
        let end = || AgentEvent::MessageEnd {
            message: crate::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
                created_at_ms: None,
            },
        };

        app.handle_event(reasoning_start());
        app.handle_event(AgentEvent::MessageUpdate {
            delta: "first thought".to_string(),
        });
        app.handle_event(end());
        app.handle_event(text_start());
        app.handle_event(AgentEvent::MessageUpdate {
            delta: "the answer".to_string(),
        });
        app.handle_event(end());
        app.handle_event(reasoning_start());
        app.handle_event(AgentEvent::MessageUpdate {
            delta: "second thought".to_string(),
        });
        app.handle_event(end());

        assert_eq!(app.blocks.len(), 3);
        assert!(matches!(&app.blocks[0], UIBlock::Reasoning(s) if s == "first thought"));
        assert!(matches!(&app.blocks[1], UIBlock::Assistant(s) if s == "the answer"));
        assert!(matches!(&app.blocks[2], UIBlock::Reasoning(s) if s == "second thought"));
    }

    #[test]
    fn skill_picker_toggles_selected() {
        use std::sync::Arc;
        let _env = crate::util::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = crate::util::unique_temp_dir("ignis-app-skillpicker");
        let cwd = tmp.join("proj");
        for n in ["alpha", "beta"] {
            let dir = cwd.join(".ignis/skills").join(n);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\nname: {n}\n---\nbody")).unwrap();
        }
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = App::new(Some("p".into()), Some("m".into()), "s".into(), cwd.clone());
        app.skills = Some(Arc::new(crate::skills::SkillRegistry::load(
            None,
            &cwd,
            std::collections::HashSet::new(),
        )));
        app.show_skill_picker();
        assert!(app.skill_picker.is_some());
        let (name, now) = app.toggle_selected_skill().unwrap();
        assert_eq!(name, "alpha"); // row 0, sorted
        assert!(!now); // disabled
        assert!(!app.skills.as_deref().unwrap().is_enabled("alpha"));

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}

#[cfg(test)]
mod paste_tests {
    use super::App;
    use std::path::PathBuf;

    fn app() -> App {
        App::new(
            Some("p".into()),
            Some("m".into()),
            "s".into(),
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn block_paste_collapses_to_chip() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd\ne".into()); // 5 lines >= 4
        assert_eq!(app.composer.input, "[ pasted-text#1 5 lines ]");
        assert_eq!(app.composer.pending_pastes.len(), 1);
        assert_eq!(app.composer.pending_pastes[0].content, "a\nb\nc\nd\ne");
    }

    #[test]
    fn small_paste_inserts_inline() {
        let mut app = app();
        app.handle_paste("a\nb\nc".into()); // 3 lines < 4
        assert_eq!(app.composer.input, "a\nb\nc");
        assert!(app.composer.pending_pastes.is_empty());
    }

    #[test]
    fn crlf_is_normalized_in_count_and_content() {
        let mut app = app();
        app.handle_paste("a\r\nb\r\nc\r\nd".into()); // 4 lines after normalize
        assert_eq!(app.composer.input, "[ pasted-text#1 4 lines ]");
        assert_eq!(app.composer.pending_pastes[0].content, "a\nb\nc\nd");
    }

    #[test]
    fn submit_expands_chip_and_clears_pending() {
        let mut app = app();
        app.handle_paste("w\nx\ny\nz".into()); // 4 lines
        app.composer.insert_str(" review this");
        let out = app.submit().unwrap();
        assert_eq!(out, "w\nx\ny\nz review this");
        assert!(app.composer.input.is_empty());
        assert!(app.composer.pending_pastes.is_empty());
    }

    #[test]
    fn two_pastes_number_and_both_expand() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd".into());
        app.composer.insert_str(" ");
        app.handle_paste("e\nf\ng\nh\ni".into());
        assert!(app.composer.input.contains("#1"));
        assert!(app.composer.input.contains("#2"));
        let out = app.submit().unwrap();
        assert!(out.contains("a\nb\nc\nd"));
        assert!(out.contains("e\nf\ng\nh\ni"));
    }

    #[test]
    fn backspace_removes_whole_chip() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd".into());
        assert_eq!(app.composer.pending_pastes.len(), 1);
        app.composer.backspace(); // cursor at chip's right edge
        assert_eq!(app.composer.input, "");
        assert!(app.composer.pending_pastes.is_empty());
    }

    #[test]
    fn paste_counter_stays_monotonic_across_commits() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd".into());
        let _ = app.submit();
        app.handle_paste("e\nf\ng\nh".into());
        assert!(app.composer.input.contains("#2"));
    }

    // --- atomic-edit guards: a chip is never half-deleted into a fragment that
    // would leak to the agent and silently drop the pasted content. ---

    #[test]
    fn ctrl_w_removes_whole_chip_no_leak() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd".into()); // cursor at chip's right edge
        app.composer.delete_word_back();
        assert_eq!(app.composer.input, "");
        assert!(app.composer.pending_pastes.is_empty());
        assert_eq!(app.composer.expand_pastes(app.composer.input.clone()), "");
    }

    #[test]
    fn delete_forward_at_chip_start_removes_chip() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd".into());
        app.composer.cursor = 0;
        app.composer.delete_forward();
        assert_eq!(app.composer.input, "");
        assert!(app.composer.pending_pastes.is_empty());
    }

    #[test]
    fn backspace_inside_chip_removes_whole_chip() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd".into());
        app.composer.cursor = 5; // mid-chip
        app.composer.backspace();
        assert_eq!(app.composer.input, "");
        assert!(app.composer.pending_pastes.is_empty());
    }

    #[test]
    fn typing_inside_chip_snaps_and_keeps_chip_intact() {
        let mut app = app();
        app.handle_paste("a\nb\nc\nd".into());
        let chip = app.composer.input.clone();
        app.composer.cursor = 5; // mid-chip
        app.composer.insert_char('x');
        assert_eq!(app.composer.input, format!("{chip}x")); // x landed after the chip
        assert_eq!(app.composer.pending_pastes.len(), 1);
        assert_eq!(app.submit().unwrap(), "a\nb\nc\ndx");
    }
}

#[cfg(test)]
mod settings_tests {
    use super::{App, SettingsPanel, SettingsTab, ToolCallEntry, ToolStatus, UIBlock};
    use crate::console::SelectionDirection;
    use std::path::PathBuf;
    use std::time::Instant;

    fn test_app() -> App {
        App::new(
            Some("openai".to_string()),
            Some("gpt-5.5".to_string()),
            "s".to_string(),
            PathBuf::from("/tmp"),
        )
    }

    fn tool(name: &str) -> UIBlock {
        UIBlock::Tool(ToolCallEntry {
            id: "x".to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
            status: ToolStatus::Pending,
            started_at: Instant::now(),
            elapsed_ms: 0,
        })
    }

    #[test]
    fn tab_switch_cycles_both_tabs_both_ways() {
        let mut p = SettingsPanel::open();
        assert_eq!(p.tab, SettingsTab::Stats, "opens on Stats");
        p.switch_tab(SelectionDirection::Next);
        assert_eq!(p.tab, SettingsTab::Statusline);
        p.switch_tab(SelectionDirection::Next);
        assert_eq!(p.tab, SettingsTab::Stats, "wraps around");
        p.switch_tab(SelectionDirection::Previous);
        assert_eq!(p.tab, SettingsTab::Statusline, "← wraps the other way");
    }

    #[test]
    fn statusline_toggle_hides_and_persists_idempotently() {
        let _env = crate::util::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = crate::util::unique_temp_dir("ignis-app-statusline-toggle");
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let mut app = test_app();
        assert!(app.statusline_shows("git"), "shown by default");
        app.show_settings_panel();
        app.settings_switch_tab(SelectionDirection::Next); // Stats → Statusline
        assert_eq!(
            app.settings_panel.as_ref().unwrap().tab,
            SettingsTab::Statusline
        );
        // Move to the "git" row (index 1) and toggle it off.
        app.settings_move(SelectionDirection::Next);
        app.settings_toggle_statusline();
        assert!(!app.statusline_shows("git"), "git now hidden");
        // Toggling again shows it; the set stays clean (no duplicates).
        app.settings_toggle_statusline();
        assert!(app.statusline_shows("git"));
        assert!(app.statusline_hidden.is_empty());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn turn_message_and_tool_counts_from_blocks() {
        let mut app = test_app();
        app.blocks.clear();
        app.blocks.push(UIBlock::User("a".to_string()));
        app.blocks.push(UIBlock::Assistant("hi".to_string()));
        app.blocks.push(tool("bash"));
        app.blocks.push(tool("bash"));
        app.blocks.push(tool("edit_file"));
        app.blocks.push(UIBlock::User("b".to_string()));

        assert_eq!(app.turn_count(), 2, "two user prompts");
        assert_eq!(app.message_count(), 6);

        let tally = app.tool_tally();
        assert_eq!(tally[0], ("bash".to_string(), 2), "most-used first");
        assert_eq!(tally.iter().find(|(n, _)| n == "edit_file").unwrap().1, 1);
    }

    #[test]
    fn cumulative_usage_sums_while_last_usage_overwrites() {
        let mut app = test_app();
        let u = |i: u64, o: u64| crate::Usage {
            input_tokens: i,
            output_tokens: o,
            ..Default::default()
        };
        app.handle_event(crate::AgentEvent::Usage(u(100, 20)));
        app.handle_event(crate::AgentEvent::Usage(u(50, 10)));
        assert_eq!(app.cumulative_usage.input_tokens, 150);
        assert_eq!(app.cumulative_usage.output_tokens, 30);
        assert_eq!(
            app.last_usage.unwrap().input_tokens,
            50,
            "last_usage keeps only the latest turn"
        );
    }
}
