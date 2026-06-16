//! Frontend wire protocol — the transport-agnostic contract between the agent
//! core and whatever renders it (the in-process ratatui TUI today; an Ink
//! `ignis-tui` subprocess, a web client, or a plugin tomorrow).
//!
//! Two directions, both `serde`-friendly so the same definitions ride NDJSON
//! over a pipe, JSON over a WebSocket, or move in-process as plain Rust values:
//!
//!   * [`Outbound`] — core → frontend. Either a streaming [`AgentEvent`] or a
//!     [`ClientRequest`] that blocks a tool until the frontend replies.
//!   * [`ClientCommand`] — frontend → core. User submits, lifecycle control,
//!     and the [`ClientCommand::Reply`] that answers a `ClientRequest`.
//!
//! Atomicity invariant: every frame is self-describing. A frontend that
//! attaches mid-session must be able to interpret each frame without replaying
//! earlier ones — which is why a fresh/handover frontend is first sent a
//! [`Snapshot`] (see the FIFO acceptor) and `ClientRequest`s carry the full
//! question set rather than a delta.

use serde::{Deserialize, Serialize};

use crate::console::picker::{PickerQuestion, PickerResponse};

/// Correlates a [`ClientRequest`] with the [`ClientCommand::Reply`] that
/// answers it. Monotonic per process; allocated by the request broker.
pub type RequestId = u64;

/// Core → frontend. The single downstream frame type.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "data")]
pub enum Outbound {
    /// A streaming turn event (token delta, tool start/end, usage, …). Render
    /// only — no reply expected.
    #[serde(rename = "event")]
    Event(Box<crate::AgentEvent>),
    /// A blocking ask: a tool (or the permission gate) needs the user to pick.
    /// The frontend MUST eventually answer with a [`ClientCommand::Reply`]
    /// carrying the same `id`, or the request resolves to `Cancelled` on
    /// disconnect.
    #[serde(rename = "request")]
    Request(ClientRequest),
    /// Full session state handed to a frontend at activation (fresh connect or
    /// FIFO handover) so it can render without having seen prior frames. Also
    /// carries any request that was in flight at handover time, so an open
    /// `ask_user` survives a frontend swap.
    // Boxed: `Snapshot` is by far the largest variant; boxing keeps `Outbound`
    // (and the `Wake`/event channels that carry it) small. Serde serializes
    // `Box<T>` as `T`, so the wire shape is unchanged.
    #[serde(rename = "snapshot")]
    Snapshot(Box<Snapshot>),
    /// The project's past sessions, in reply to [`ClientCommand::ListSessions`]
    /// — for the `/sessions` picker. Most-recent-first, current session
    /// excluded; the engine owns the listing (it reads the JSONL off disk).
    #[serde(rename = "sessions")]
    Sessions(Vec<SessionInfo>),
    /// A resumed session's transcript, replayed as render-ready blocks so the
    /// frontend rebuilds its scrollback without parsing the stored JSONL. Sent
    /// in reply to [`ClientCommand::ResumeSession`]; the frontend replaces its
    /// transcript with these blocks, and the following [`Snapshot`] carries the
    /// retargeted session id.
    #[serde(rename = "transcript")]
    Transcript {
        session_id: String,
        blocks: Vec<TranscriptBlock>,
    },
}

/// One past session, for the `/sessions` picker. The engine reads this off disk
/// (mirrors `SessionMeta`); the frontend formats the relative age itself.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    /// First user message, trimmed — the picker's title line.
    pub preview: String,
    pub message_count: usize,
    /// Unix seconds of last modification.
    pub last_modified: u64,
}

/// A render-ready transcript entry. The wire shape mirrors the frontend's block
/// model so a resumed session drops straight into scrollback.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptBlock {
    User {
        text: String,
    },
    /// Chain-of-thought, rendered as a collapsible `✻ Thinking` block. Pushed
    /// before its assistant reply, mirroring the streaming order.
    Reasoning {
        text: String,
    },
    Assistant {
        text: String,
    },
    Tool {
        name: String,
        args: String,
        result: crate::ToolResult,
    },
}

/// A blocking interaction request surfaced to the active frontend.
#[derive(Debug, Clone, Serialize)]
pub struct ClientRequest {
    pub id: RequestId,
    /// One or more questions, rendered one at a time. Self-contained: the whole
    /// set travels in the frame so a mid-session frontend needs no prior state.
    pub questions: Vec<PickerQuestion>,
}

/// Session state for a newly-activated frontend. Intentionally minimal at this
/// layer — the rich transcript model lives in `App`; this is the seam a
/// transport serializes. Carries the in-flight request (if any) so a handover
/// never strands a blocked tool.
/// A selectable provider/model pair, for the frontend's `/model` picker.
#[derive(Debug, Clone, Serialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
    /// Context window in tokens (`None` = unknown), so the frontend can render a
    /// context-fill % for the active model without re-deriving it from a catalog.
    pub context: Option<u64>,
    /// Reasoning-effort levels this model accepts, in display order (empty = no
    /// effort control), so the `/model` picker can cycle effort like the native
    /// picker does — without re-deriving them from a catalog.
    pub effort_levels: Vec<String>,
}

/// A named, toggleable feature (a skill or an MCP server) with its enabled
/// state — for the `/skills` and `/mcp` toggle pickers.
#[derive(Debug, Clone, Serialize)]
pub struct Toggle {
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub session_id: String,
    /// Engine binary version (`CARGO_PKG_VERSION`), so the frontend's banner can
    /// show the authoritative ignis version rather than its own package version.
    pub version: String,
    /// Active provider/model and working directory, so an out-of-process
    /// frontend can render a statusline without re-deriving the core's config.
    pub provider: String,
    pub model: String,
    pub cwd: String,
    /// Active reasoning effort (`None` = the model has no effort control or none
    /// is set), so the `/model` picker can preselect it and the footer can show it.
    pub effort: Option<String>,
    /// Active permission mode (`off` / `hands_free` / `fully_unattended`), for
    /// the statusline badge and the `/afk` picker.
    pub mode: String,
    /// The configured models, for the `/model` picker (the engine owns the list).
    pub models: Vec<ModelRef>,
    /// Skills + MCP servers with their enabled state, for `/skills` and `/mcp`.
    pub skills: Vec<Toggle>,
    pub mcp: Vec<Toggle>,
    /// Request awaiting an answer at activation time, if a tool was blocked
    /// when this frontend took over.
    pub pending_request: Option<ClientRequest>,
}

/// Frontend → core. The single upstream frame type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum ClientCommand {
    /// Submit a line for the agent. Slash commands (`/compact`, `/model`, …)
    /// arrive here verbatim; the core dispatcher interprets them — the frontend
    /// only recognizes and forwards.
    #[serde(rename = "submit")]
    Submit { text: String },
    /// Tell the core which session id subsequent [`ClientCommand::Submit`]s
    /// belong to, after the frontend switched it (`/clear`, `/resume`, startup).
    ///
    /// This is a LOCAL-frontend concept: the in-process ratatui TUI owns session
    /// creation today (it mints ids and writes the JSONL). A remote frontend
    /// can't create the core's session files, so the transport-agnostic endgame
    /// is for the CORE to own session lifecycle (creation + a core→frontend id
    /// signal). That is a deferred refinement (plan "approach B"); until then a
    /// local frontend syncs the id it created via this command.
    #[serde(rename = "set_session")]
    SetSession { session_id: String },
    /// Inject text into the *in-flight* turn (the running prompt's inject
    /// source) rather than queuing a new turn.
    #[serde(rename = "inject")]
    Inject { text: String },
    /// Cancel the current turn (Ctrl-C / ESC).
    #[serde(rename = "cancel")]
    Cancel,
    /// Start a fresh session (`/clear`). The core mints a new session id,
    /// retargets subsequent submits at it, and re-snapshots the frontend with
    /// the new id — the engine owns session creation (unlike the local-frontend
    /// [`ClientCommand::SetSession`]).
    #[serde(rename = "new_session")]
    NewSession,
    /// Switch the active provider/model (`/model`). The core applies it to
    /// subsequent prompts and re-snapshots so the statusline updates. `effort`
    /// is the picked reasoning level (`None` = the model has no effort control);
    /// the core applies + persists it exactly like the native picker.
    #[serde(rename = "set_model")]
    SetModel {
        provider: String,
        model: String,
        #[serde(default)]
        effort: Option<String>,
    },
    /// Switch the permission mode (`/afk`): `off` / `hands_free` /
    /// `fully_unattended`. The core applies + persists it and re-snapshots.
    #[serde(rename = "set_mode")]
    SetMode { mode: String },
    /// Toggle a skill (`/skills`) or MCP server (`/mcp`) on/off. The core flips
    /// + persists it and re-snapshots with the new enabled state.
    #[serde(rename = "toggle_skill")]
    ToggleSkill { name: String },
    #[serde(rename = "toggle_mcp")]
    ToggleMcp { name: String },
    /// Request the project's past sessions (`/sessions`). The core replies with
    /// an [`Outbound::Sessions`] frame (current session excluded).
    #[serde(rename = "list_sessions")]
    ListSessions,
    /// Resume a past session (`/sessions` → pick, or `/resume <id>`). The core
    /// retargets subsequent submits at it (like [`ClientCommand::SetSession`]),
    /// replays its transcript as an [`Outbound::Transcript`], and re-snapshots.
    #[serde(rename = "resume_session")]
    ResumeSession { session_id: String },
    /// Copy text to the system clipboard (`/copy`). The frontend holds the
    /// transcript, so it extracts the last assistant message and sends the text;
    /// the core reuses its platform clipboard helper (no Ink-stdout/OSC52 risk)
    /// and surfaces a `Warning` only if the copy fails.
    #[serde(rename = "copy")]
    Copy { text: String },
    /// Answer a [`ClientRequest`]. `id` must match the outstanding request; a
    /// stale or unknown id is dropped by the broker.
    #[serde(rename = "reply")]
    Reply { id: RequestId, answer: ReplyAnswer },
    /// Frontend is detaching cleanly. The acceptor promotes the next queued
    /// frontend (FIFO), if any.
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// The payload of a [`ClientCommand::Reply`]. Mirrors [`PickerResponse`] but is
/// the wire-facing name so the protocol layer doesn't leak the picker module's
/// internal type as its public reply shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplyAnswer {
    /// The user answered every question, in `questions` order.
    Answered(Vec<crate::console::picker::PickerAnswer>),
    /// The user dismissed the request (ESC).
    Cancelled,
}

impl From<ReplyAnswer> for PickerResponse {
    fn from(a: ReplyAnswer) -> Self {
        match a {
            ReplyAnswer::Answered(v) => PickerResponse::Answered(v),
            ReplyAnswer::Cancelled => PickerResponse::Cancelled,
        }
    }
}

impl From<PickerResponse> for ReplyAnswer {
    fn from(r: PickerResponse) -> Self {
        match r {
            PickerResponse::Answered(v) => ReplyAnswer::Answered(v),
            PickerResponse::Cancelled => ReplyAnswer::Cancelled,
        }
    }
}
