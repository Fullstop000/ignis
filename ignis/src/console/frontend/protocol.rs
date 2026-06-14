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
    #[serde(rename = "snapshot")]
    Snapshot(Snapshot),
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
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub session_id: String,
    /// Active provider/model and working directory, so an out-of-process
    /// frontend can render a statusline without re-deriving the core's config.
    pub provider: String,
    pub model: String,
    pub cwd: String,
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
