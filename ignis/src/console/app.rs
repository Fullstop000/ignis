use std::path::PathBuf;
use std::time::Instant;

use super::composer::Composer;
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

// ───────────────────────────────────────────────────────────────────────────
// `/connect` picker-request builders. Each returns a fully-formed
// `PickerRequest` for the runner's `picker_tx` mpsc; the reply oneshot is
// fire-and-forget because the flow's state lives in `App::connect_draft`,
// not in awaiting tasks. The picker-completion path in `keys.rs` reads the
// draft to know which step's answer it just received.
// ───────────────────────────────────────────────────────────────────────────

/// Step 1: pick a provider from the baked-in `SPECS` catalog. The currently-
/// active provider (if any) is mentioned in the question text so users who
/// re-run `/connect` to rotate a key know what they're about to overwrite.
fn build_provider_picker(current_provider: Option<&str>) -> crate::console::picker::PickerRequest {
    use crate::console::picker::{PickerOption, PickerQuestion, PickerRequest};
    let options: Vec<PickerOption> = crate::llm::providers::all()
        .iter()
        .map(|spec| PickerOption {
            label: spec.display_name.to_string(),
            description: provider_description(spec),
            preview: None,
        })
        .collect();
    let question = match current_provider {
        Some(id) => format!("Connect a provider (current: {id})"),
        None => "Connect a provider — pick one to configure".to_string(),
    };
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PickerRequest {
        questions: vec![PickerQuestion {
            question,
            kind: "connect".to_string(),
            header: "Provider".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: false,
            mask: false,
            options,
        }],
        reply: tx,
    }
}

/// Step 2: API-key entry. Text-input mode, masked (no shoulder-surfing).
fn build_api_key_picker(provider_display: &str) -> crate::console::picker::PickerRequest {
    use crate::console::picker::{PickerQuestion, PickerRequest};
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PickerRequest {
        questions: vec![PickerQuestion {
            question: format!("Paste your API key for {provider_display}"),
            kind: "connect".to_string(),
            header: "API Key".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: true,
            mask: true,
            options: vec![],
        }],
        reply: tx,
    }
}

/// Step 3: default-model picker, scoped to the chosen provider's `&[ModelSpec]`.
fn build_model_picker(
    spec: &crate::llm::providers::ProviderSpec,
) -> crate::console::picker::PickerRequest {
    use crate::console::picker::{PickerOption, PickerQuestion, PickerRequest};
    let options: Vec<PickerOption> = spec
        .models
        .iter()
        .map(|m| PickerOption {
            label: m.name.to_string(),
            description: model_description(m),
            preview: None,
        })
        .collect();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PickerRequest {
        questions: vec![PickerQuestion {
            question: format!("Pick a default model for {}", spec.display_name),
            kind: "connect".to_string(),
            header: "Model".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: false,
            mask: false,
            options,
        }],
        reply: tx,
    }
}

/// One-line endpoint hint for the provider row, synthesized from the first
/// endpoint's `base_url`. Strips the protocol so the URL doesn't dominate
/// the line.
fn provider_description(spec: &crate::llm::providers::ProviderSpec) -> String {
    if spec.id == "custom" {
        return "Edit ~/.ignis/config.toml after selecting (api_url + models required)."
            .to_string();
    }
    let host = spec
        .endpoints
        .first()
        .map(|e| {
            e.base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string()
        })
        .unwrap_or_default();
    if spec.api_key_required {
        host
    } else {
        // Local-only providers don't take a key — call that out so users
        // don't expect to be prompted for one.
        format!("{host}  (no key required)")
    }
}

/// One-line model-row hint: context window if known, else empty. Keeps the
/// row short — full effort/reasoning details live in `/model`.
fn model_description(m: &crate::llm::providers::ModelSpec) -> String {
    match m.context {
        Some(ctx) => format!("context {}", super::format::format_context(ctx)),
        None => String::new(),
    }
}

/// `/connect` multi-step flow state. Created when the user types `/connect`,
/// cleared on completion or cancel. The picker-completion path in `keys.rs`
/// drives advancement: each step's answer feeds the next, and the final
/// step persists to `~/.ignis/config.toml` + `~/.ignis/state.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectStep {
    PickProvider,
    EnterApiKey,
    PickModel,
}

#[derive(Clone)]
pub(crate) struct ConnectDraft {
    pub(crate) step: ConnectStep,
    /// Provider id (the `SPECS` key, e.g. "openai"). Set after step 1.
    pub(crate) provider_id: Option<String>,
    /// Provider display name (e.g. "OpenAI"). Used for the API-key prompt.
    pub(crate) provider_display: Option<String>,
    /// Raw API key as typed. Stays in memory until the persist step writes
    /// `[providers.<id>] api_key = "…"` to `config.toml`. None for Ollama
    /// and similar providers with `api_key_required = false`.
    pub(crate) api_key: Option<String>,
    /// Selected model name (e.g. "gpt-5.5"). Set after step 3.
    pub(crate) model: Option<String>,
}

// Manual `Debug` that redacts `api_key` — a derived impl would print the
// plaintext key the moment something `dbg!(&draft)`s or a tracing span captures
// `App` state. Keep the redaction; never derive Debug on this struct.
impl std::fmt::Debug for ConnectDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectDraft")
            .field("step", &self.step)
            .field("provider_id", &self.provider_id)
            .field("provider_display", &self.provider_display)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("model", &self.model)
            .finish()
    }
}

/// What `advance_connect` tells the caller (`keys.rs`) to do next.
#[derive(Debug)]
pub(crate) enum ConnectAdvance {
    /// Send this picker over `picker_tx` — it's the next step in the flow.
    NextPicker(crate::console::picker::PickerRequest),
    /// Connect succeeded. The agent loop needs a fresh config from disk so
    /// its in-memory `agent_config` reflects the new `api_key` (the existing
    /// `SetModel` variant doesn't carry providers, hence the dedicated
    /// `ReloadConfig` request).
    Saved,
    /// Connect aborted (user picked Custom, persist failed, etc). A user-
    /// facing notice has already been added; the caller does nothing else.
    Failed,
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
    pub(crate) current_session_id: String,
    pub(crate) projects_dir: std::path::PathBuf,
}

impl SessionPicker {
    pub(crate) fn new(
        sessions: Vec<crate::cli::sessions::SessionRecord>,
        current_session_id: String,
        projects_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            sessions,
            selected: 0,
            mode: SessionPickerMode::List,
            current_session_id,
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
}

impl SkillPicker {
    /// Open over a non-empty registry; returns `None` (so the caller can show
    /// a notice) when no skills are configured.
    pub(crate) fn open(registry: &crate::skills::SkillRegistry) -> Option<Self> {
        (!registry.is_empty()).then_some(Self { selected: 0 })
    }

    pub(crate) fn select(&mut self, direction: SelectionDirection, total: usize) {
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

    /// Resolve the picker's selection into `(provider, model, effort, context_window)`
    /// for the caller to apply. Effort is `None` when the model declares no
    /// levels; context_window is `None` when the option doesn't declare one.
    pub(crate) fn resolve(
        &self,
        options: &[crate::llm::ModelOption],
    ) -> Option<(String, String, Option<String>, Option<u64>)> {
        let opt = options.get(self.selected)?;
        let effort = if opt.effort_levels.is_empty() {
            None
        } else {
            opt.effort_levels.get(self.effort_idx).cloned()
        };
        Some((opt.provider.clone(), opt.model.clone(), effort, opt.context))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Idle,
    Thinking,    // LLM is generating
    ToolRunning, // tool execution in progress
}

pub(crate) struct App {
    pub(crate) provider: String,
    pub(crate) model: String,
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
    /// `/connect` flow state. `Some` while the multi-step picker is open
    /// (provider → API key → model), `None` otherwise. The picker-completion
    /// path in `keys.rs` checks this to route picker answers back into
    /// `advance_connect` instead of treating them as tool-initiated answers.
    pub(crate) connect_draft: Option<ConnectDraft>,

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

    pub(crate) should_quit: bool,
    pub(crate) error_flash: Option<(String, Instant)>,
    pub(crate) exit_pending: bool,

    /// Token budget the context-usage % is measured against (the active model's
    /// window, or the fallback below). Estimated, not exact.
    pub(crate) context_window: usize,
    /// Window to use when the active model's context is unknown (the compaction
    /// threshold) — so the gauge never sticks to a previous model's window.
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
}

impl App {
    pub(crate) fn new(provider: String, model: String, session_id: String, cwd: PathBuf) -> Self {
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
            model_picker: None,
            skill_picker: None,
            mcp_picker: None,
            inline_picker: None,
            connect_draft: None,
            mode: Mode::Idle,
            tick: 0,
            stream_start: None,
            current_chunk_idx: None,
            stream_chars: 0,
            last_usage: None,
            committed: 0,
            committed_rows: 0,
            pending_screen_clear: false,
            should_quit: false,
            error_flash: None,
            exit_pending: false,
            context_window: 120_000,
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
        }
    }

    /// Recompute the cached git branch from `cwd`. Called on each turn
    /// boundary so the footer tracks a mid-session `git checkout`.
    pub(crate) fn refresh_git_branch(&mut self) {
        self.git_branch = super::git::branch(&self.cwd);
    }

    pub(crate) fn set_context_window(&mut self, window: usize) {
        self.context_window = window;
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
            .checked_div(self.context_window)
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
                // Real provider-reported usage for the latest turn.
                self.last_usage = Some(usage);
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

    pub(crate) fn show_session_picker(
        &mut self,
        sessions: Vec<crate::cli::sessions::SessionRecord>,
        projects_dir: std::path::PathBuf,
    ) {
        self.exit_pending = false;
        self.session_picker = Some(SessionPicker::new(
            sessions,
            self.session_id.clone(),
            projects_dir,
        ));
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

    /// `/connect` — start the multi-step provider-setup flow. Returns the
    /// first picker (provider selection); the caller (`keys.rs`) sends it
    /// over `picker_tx`. The runner installs it as `app.inline_picker`, the
    /// user picks, and the picker-completion path routes back here via
    /// [`Self::advance_connect`].
    ///
    /// Returns `None` only if a picker is already open — the caller emits
    /// "another picker is open" instead of stomping the existing one.
    pub(crate) fn start_connect(&mut self) -> Option<crate::console::picker::PickerRequest> {
        self.exit_pending = false;
        if self.inline_picker.is_some() {
            self.add_assistant_notice(
                "/connect: another picker is open; close it first.".to_string(),
            );
            return None;
        }
        let current_provider = (!self.provider.is_empty()).then(|| self.provider.clone());
        self.connect_draft = Some(ConnectDraft {
            step: ConnectStep::PickProvider,
            provider_id: None,
            provider_display: None,
            api_key: None,
            model: None,
        });
        Some(build_provider_picker(current_provider.as_deref()))
    }

    /// Drive the `/connect` flow one step forward given the picker's answer.
    /// Called from `keys.rs` when an inline picker completes AND a draft is
    /// active. Mutates `connect_draft`, may emit notices, and on the final
    /// step writes `config.toml` + `state.json` and refreshes the UI's
    /// `provider`/`model` fields so the banner reflects the new pick.
    pub(crate) fn advance_connect(
        &mut self,
        answers: Vec<crate::console::picker::PickerAnswer>,
    ) -> ConnectAdvance {
        use crate::console::picker::PickerAnswer;
        // The draft must be set by `start_connect` before this is called; a
        // missing draft is a programming error, not a user-facing situation.
        let Some(draft) = self.connect_draft.as_mut() else {
            return ConnectAdvance::Failed;
        };
        let answer = match answers.into_iter().next() {
            Some(PickerAnswer::Single(s)) => s,
            // Connect pickers are all single-select; a Multi answer means the
            // picker shape got out of sync somewhere — treat as cancel.
            _ => {
                self.connect_draft = None;
                return ConnectAdvance::Failed;
            }
        };
        match draft.step {
            ConnectStep::PickProvider => {
                let Some(spec) = crate::llm::providers::all()
                    .iter()
                    .find(|s| s.display_name == answer)
                else {
                    self.connect_draft = None;
                    self.add_assistant_notice(format!("Unknown provider: {answer}"));
                    return ConnectAdvance::Failed;
                };
                // The `custom` brand requires `api_url` + `models` fields that
                // need a multi-field form; we don't build that wizard in v1.
                // Bail out with a pointer to the example config so the user
                // knows where to go.
                if spec.id == "custom" {
                    self.connect_draft = None;
                    self.add_assistant_notice(
                        "For custom providers, edit ~/.ignis/config.toml — see config.example.toml."
                            .to_string(),
                    );
                    return ConnectAdvance::Failed;
                }
                draft.provider_id = Some(spec.id.to_string());
                draft.provider_display = Some(spec.display_name.to_string());
                // Ollama-class providers skip the key step entirely.
                if spec.api_key_required {
                    draft.step = ConnectStep::EnterApiKey;
                    ConnectAdvance::NextPicker(build_api_key_picker(spec.display_name))
                } else {
                    draft.step = ConnectStep::PickModel;
                    ConnectAdvance::NextPicker(build_model_picker(spec))
                }
            }
            ConnectStep::EnterApiKey => {
                draft.api_key = Some(answer);
                let provider_id = draft.provider_id.clone().unwrap_or_default();
                let Some(spec) = crate::llm::providers::lookup(&provider_id) else {
                    self.connect_draft = None;
                    self.add_assistant_notice(format!("Unknown provider id: {provider_id}"));
                    return ConnectAdvance::Failed;
                };
                draft.step = ConnectStep::PickModel;
                ConnectAdvance::NextPicker(build_model_picker(spec))
            }
            ConnectStep::PickModel => {
                draft.model = Some(answer);
                self.persist_connect()
            }
        }
    }

    /// Write the draft to disk and update in-memory `App` fields. Called from
    /// `advance_connect` after the model picker resolves; split out so the
    /// flow stays readable.
    fn persist_connect(&mut self) -> ConnectAdvance {
        let Some(draft) = self.connect_draft.take() else {
            return ConnectAdvance::Failed;
        };
        let (provider_id, model) = match (draft.provider_id, draft.model) {
            (Some(p), Some(m)) => (p, m),
            _ => return ConnectAdvance::Failed,
        };
        // Key may be empty for providers that don't require one (Ollama). The
        // config writer is only called when there's actually a key to store —
        // otherwise we'd write `api_key = ""` which is meaningless.
        if let Some(api_key) = draft
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Err(e) = crate::config::write_provider_key(&provider_id, api_key) {
                self.add_assistant_notice(format!(
                    "Failed to write ~/.ignis/config.toml: {e}. Nothing saved."
                ));
                return ConnectAdvance::Failed;
            }
        }
        // Default-model write into state.json. Failure here is recoverable —
        // the api_key is the expensive thing the user typed; preserve it and
        // tell the user to /model manually.
        if let Err(e) = crate::state::persist_model_selection(&provider_id, &model, None) {
            self.add_assistant_notice(format!(
                "Provider saved but default model not set: {e}. Run /model to set it."
            ));
        }
        // Refresh the in-memory UI so the banner/footer show the new pick on
        // next draw. The agent loop gets a separate ReloadConfig request.
        self.provider = provider_id.clone();
        self.model = model.clone();
        self.effort = None;
        self.add_assistant_notice(format!(
            "✓ Connected to {provider_id}. Default model: {model}.\n  \
             Wrote ~/.ignis/config.toml and ~/.ignis/state.json."
        ));
        ConnectAdvance::Saved
    }

    /// Discard the in-flight `/connect` draft and emit a one-line notice.
    /// Called from `keys.rs` when the user presses Esc/Ctrl-C while a connect
    /// picker is open.
    pub(crate) fn cancel_connect(&mut self) {
        if self.connect_draft.take().is_some() {
            self.add_assistant_notice("/connect cancelled.".to_string());
        }
    }

    pub(crate) fn show_model_picker(&mut self) {
        self.exit_pending = false;
        match ModelPicker::open(
            &self.model_options,
            &self.provider,
            &self.model,
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
        let (provider, model, effort, context) = picker.resolve(&self.model_options)?;
        self.provider = provider.clone();
        self.model = model.clone();
        self.effort = effort.clone();
        // Retarget the footer's context gauge to the new model's window, falling
        // back when it's unknown so the % isn't measured against the old model.
        self.context_window = context
            .map(|c| c as usize)
            .unwrap_or(self.fallback_context_window);
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
            "p".to_string(),
            "m".to_string(),
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
    use crate::console::picker::PickerAnswer;
    use std::path::PathBuf;

    fn fresh_app() -> App {
        // Empty provider/model = no-provider mode, matching the typical
        // first-launch caller of /connect.
        App::new(
            String::new(),
            String::new(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn start_connect_creates_draft_and_returns_provider_picker() {
        let mut app = fresh_app();
        let req = app.start_connect().expect("should return a picker");
        // Draft is at step 1, all fields unset.
        let draft = app.connect_draft.as_ref().expect("draft must exist");
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
        assert!(app.connect_draft.is_none(), "no draft on refusal");
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
        let draft = app.connect_draft.as_ref().unwrap();
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
        assert_eq!(
            app.connect_draft.as_ref().unwrap().step,
            ConnectStep::PickModel
        );
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
        assert!(app.connect_draft.is_none());
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
        assert!(app.connect_draft.is_none());
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
        let draft = app.connect_draft.as_ref().unwrap();
        assert_eq!(draft.step, ConnectStep::PickModel);
        assert_eq!(draft.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn connect_draft_debug_redacts_api_key() {
        // The api_key must never appear in `Debug` output — `dbg!` or a
        // tracing span capturing `App` state would otherwise leak it.
        let mut app = fresh_app();
        let _ = app.start_connect().unwrap();
        let _ = app.advance_connect(vec![PickerAnswer::Single("DeepSeek".into())]);
        let _ = app.advance_connect(vec![PickerAnswer::Single("sk-supersecret".into())]);
        let draft = app.connect_draft.as_ref().unwrap();
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
        assert!(app.connect_draft.is_some());
        app.cancel_connect();
        assert!(app.connect_draft.is_none());
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
        assert!(app.connect_draft.is_none());
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
            "p".into(),
            "m".into(),
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
            "p".to_string(),
            "m".to_string(),
            "s".to_string(),
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn switching_models_retargets_context_window() {
        let mut app = test_app();
        app.fallback_context_window = 100_000;
        app.context_window = 500_000; // pretend a prior known window
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
        assert_eq!(app.context_window, 1_000_000);
        // Switch to an unknown-context model → gauge resets to the fallback,
        // not the previous model's window.
        app.model_picker = Some(ModelPicker {
            selected: 1,
            effort_idx: 0,
        });
        app.apply_model_selection();
        assert_eq!(app.context_window, 100_000);
    }

    fn picker_app() -> App {
        let mut app = App::new(
            "deepseek".to_string(),
            "deepseek-v4-flash".to_string(),
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
        assert_eq!(app.provider, "deepseek");
        assert_eq!(app.model, "deepseek-v4-pro");
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
        let mut app = App::new("p".into(), "m".into(), "s".into(), PathBuf::from("/tmp"));
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
        let mut app = App::new("p".into(), "m".into(), "s".into(), PathBuf::from("/tmp"));
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
        let mut app = App::new("p".into(), "m".into(), "s".into(), PathBuf::from("/tmp"));
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

    #[test]
    fn interleaved_reasoning_text_reasoning_yields_three_blocks() {
        // Streaming order: reasoning₁ → text → reasoning₂. Each kind change
        // must close the previous block and open a new one — the user should
        // see three distinct UIBlocks in the order they streamed in, not
        // glued together.
        let mut app = App::new("p".into(), "m".into(), "s".into(), PathBuf::from("/tmp"));
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

        let mut app = App::new("p".into(), "m".into(), "s".into(), cwd.clone());
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
        App::new("p".into(), "m".into(), "s".into(), PathBuf::from("/tmp"))
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
