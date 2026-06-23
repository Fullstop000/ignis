//! State for the slash-command pickers (`/sessions`, `/skills`, `/mcp`,
//! `/model`, `/settings`). Pure UI state: each type owns its cursor and its
//! own data and is driven by `next_selection`; none reference the `App`
//! god-object. Extracted from `app.rs` to keep that module focused on `App`
//! itself, and re-exported from there so `crate::console::app::*` paths resolve.

use crate::console::format::{next_selection, SelectionDirection};

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
    Sandbox,
}

impl SettingsTab {
    /// Tab order, used to cycle with `←`/`→`/Tab.
    const ORDER: [SettingsTab; 3] = [
        SettingsTab::Stats,
        SettingsTab::Statusline,
        SettingsTab::Sandbox,
    ];
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
