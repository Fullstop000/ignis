//! Frontend hub — the one object the runner drives to talk to whatever
//! frontend is attached.
//!
//! It composes the three lower-level pieces so the runner doesn't have to
//! orchestrate them by hand:
//!
//!   * [`Acceptor`] — which frontend is live (single-slot, FIFO successors).
//!   * [`RequestBroker`] — the id table bridging tools' blocking picker
//!     `oneshot`s to wire [`ClientRequest`]s and back.
//!   * [`command`] classification — turning an upstream [`ClientCommand`] into
//!     either a broker reply (handled here) or work the runner must do.
//!
//! The hub also owns the failure-recovery policy in one place: when emitting to
//! the active frontend fails (it disconnected), promote the next queued
//! frontend and re-establish it with a [`Snapshot`] — carrying any in-flight
//! request so a blocked tool survives the swap — or, if none remain, cancel all
//! outstanding requests so no tool hangs.

use crate::console::frontend::command::{control_signal, ControlSignal};
use crate::console::frontend::port::Acceptor;
use crate::console::frontend::protocol::{
    ClientCommand, ClientRequest, ModelRef, Outbound, SessionInfo, Setting, Snapshot, Toggle,
    TranscriptBlock,
};
use crate::console::frontend::RequestBroker;
use crate::console::picker::{PickerRequest, PickerResponse};

/// What the runner should do with an upstream command after the hub has handled
/// everything frontend-internal (i.e. replies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// A user line to route through the core slash dispatcher.
    Submit(String),
    /// The active session id changed on the frontend; the core should retarget
    /// subsequent submits at it (local-frontend session sync — see
    /// [`ClientCommand::SetSession`]).
    SetSession(String),
    /// Start a fresh session (`/clear`): the core mints a new id, retargets, and
    /// re-snapshots the frontend with it.
    NewSession,
    /// Switch the active provider/model (`/model`): the core applies it to the
    /// next prompt and re-snapshots so the statusline updates.
    SetModel {
        provider: String,
        model: String,
        /// Picked reasoning effort (`None` = the model has no effort control).
        effort: Option<String>,
    },
    /// Switch the permission mode (`/afk`): the core applies + persists it and
    /// re-snapshots.
    SetMode(String),
    /// Flip a `/settings` config knob: the core calls `settings::apply_setting`
    /// (effect + persist) and re-snapshots with the rebuilt settings list.
    SetSetting {
        id: String,
        value: bool,
    },
    /// Toggle a skill / MCP server (`/skills`, `/mcp`): the core flips + persists
    /// it and re-snapshots.
    ToggleSkill(String),
    ToggleMcp(String),
    /// List the project's past sessions (`/sessions`): the core reads them off
    /// disk and replies with an [`Outbound::Sessions`] frame.
    ListSessions,
    /// Resume a past session (`/sessions` pick / `/resume`): the core retargets
    /// subsequent submits at it, replays its transcript, and re-snapshots.
    ResumeSession(String),
    /// Copy text to the system clipboard (`/copy`): the core writes it via its
    /// platform clipboard helper and warns only on failure.
    Copy(String),
    /// A mechanical control signal (cancel / inject / shutdown).
    Control(ControlSignal),
    /// A `Reply` the hub already resolved against the broker — nothing left for
    /// the runner to do.
    Handled,
}

pub struct FrontendHub {
    session_id: String,
    /// Static session meta surfaced to frontends in every [`Snapshot`] so an
    /// out-of-process renderer can draw a statusline (provider/model/cwd).
    provider: String,
    model: String,
    cwd: String,
    /// Active permission mode (set after construction + on `/afk`).
    mode: String,
    /// Active reasoning effort (set after construction + on `/model`).
    effort: Option<String>,
    /// The configured models, surfaced to the frontend's `/model` picker.
    models: Vec<ModelRef>,
    /// Skills + MCP servers with enabled state (set after construction + on toggle).
    skills: Vec<Toggle>,
    mcp: Vec<Toggle>,
    /// Generic `/settings` config knobs (set at startup + rebuilt on each toggle).
    settings: Vec<Setting>,
    acceptor: Acceptor,
    broker: RequestBroker,
    /// The single picker awaiting an answer, if any. The console opens pickers
    /// one at a time, so one slot is enough; it rides a [`Snapshot`] on
    /// handover so a successor frontend can finish answering it.
    pending: Option<ClientRequest>,
}

impl FrontendHub {
    pub fn new(
        session_id: String,
        provider: String,
        model: String,
        cwd: String,
        models: Vec<ModelRef>,
        acceptor: Acceptor,
        broker: RequestBroker,
    ) -> Self {
        Self {
            session_id,
            provider,
            model,
            cwd,
            mode: String::new(),
            effort: None,
            models,
            skills: Vec::new(),
            mcp: Vec::new(),
            settings: Vec::new(),
            acceptor,
            broker,
            pending: None,
        }
    }

    pub fn has_active(&self) -> bool {
        self.acceptor.has_active()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            session_id: self.session_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            effort: self.effort.clone(),
            mode: self.mode.clone(),
            models: self.models.clone(),
            skills: self.skills.clone(),
            mcp: self.mcp.clone(),
            settings: self.settings.clone(),
            pending_request: self.pending.clone(),
        }
    }

    /// Update the permission mode the hub reports (startup + after `/afk`).
    pub fn set_mode(&mut self, mode: String) {
        self.mode = mode;
    }

    /// Update the active reasoning effort the hub reports (startup; `/model`
    /// updates it via [`Self::set_active_model`]).
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort;
    }

    /// Update the skill / MCP enabled lists the hub reports (startup + on toggle).
    pub fn set_skills(&mut self, skills: Vec<Toggle>) {
        self.skills = skills;
    }
    pub fn set_mcp(&mut self, mcp: Vec<Toggle>) {
        self.mcp = mcp;
    }

    /// Update the `/settings` config knobs the hub reports (startup + rebuilt
    /// after each `SetSetting`).
    pub fn set_settings(&mut self, settings: Vec<Setting>) {
        self.settings = settings;
    }

    /// Retarget the session this hub reports in snapshots (after `/clear`).
    pub fn set_session_id(&mut self, session_id: String) {
        self.session_id = session_id;
    }

    /// Update the active provider/model + effort the hub reports (after `/model`).
    pub fn set_active_model(&mut self, provider: String, model: String, effort: Option<String>) {
        self.provider = provider;
        self.model = model;
        self.effort = effort;
    }

    /// The active provider/model the hub reports — read by the `/connect` flow to
    /// offer a "keep current model" row and skip re-listing the active pair.
    pub fn provider(&self) -> &str {
        &self.provider
    }
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Replace the `/model` picker's model list (after `/connect` imports a new
    /// provider's models into config).
    pub fn set_models(&mut self, models: Vec<ModelRef>) {
        self.models = models;
    }

    /// Whether a picker request is currently awaiting an answer — `/connect`
    /// refuses to start while another picker is open.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Clear the in-flight picker slot. The `/connect` driver routes replies
    /// itself (not through the broker), so it clears `pending` directly.
    pub fn clear_pending(&mut self) {
        self.pending = None;
    }

    /// Emit the current session snapshot to the active frontend. Sent once when
    /// the core driver starts so a fresh frontend can render its statusline.
    pub async fn send_snapshot(&mut self) {
        let snap = self.snapshot();
        self.send(Outbound::Snapshot(Box::new(snap))).await;
    }

    /// Push a streaming agent event to the active frontend, recovering from a
    /// disconnect by handing over to the next queued frontend.
    pub async fn emit_event(&mut self, ev: crate::AgentEvent) {
        self.send(Outbound::Event(Box::new(ev))).await;
    }

    /// Send the project's session list to the active frontend (answers
    /// `/sessions`).
    pub async fn send_sessions(&mut self, sessions: Vec<SessionInfo>) {
        self.send(Outbound::Sessions(sessions)).await;
    }

    /// Replay a resumed session's transcript to the active frontend so it can
    /// rebuild its scrollback (answers `/sessions` pick / `/resume`).
    pub async fn send_transcript(&mut self, session_id: String, blocks: Vec<TranscriptBlock>) {
        self.send(Outbound::Transcript { session_id, blocks }).await;
    }

    /// Surface a tool's blocking picker request: register it with the broker,
    /// remember it as the in-flight request (for handover), and emit it.
    pub async fn open_request(&mut self, req: PickerRequest) {
        let client_req = self.broker.register(req);
        self.pending = Some(client_req.clone());
        self.send(Outbound::Request(client_req)).await;
    }

    /// Handle one upstream command. `Reply`s are resolved against the broker
    /// here; everything else is classified for the runner to act on.
    pub fn handle_command(&mut self, cmd: ClientCommand) -> CommandOutcome {
        // One exhaustive match: a new `ClientCommand` variant fails to compile
        // until it's handled here, instead of silently falling through to
        // `Handled` (the previous if-chain + `None => Handled` could swallow a
        // mis-wired command).
        match cmd {
            ClientCommand::Reply { id, answer } => {
                let response: PickerResponse = answer.into();
                self.broker.resolve(id, response);
                // Clear the in-flight slot once its matching reply lands.
                if self.pending.as_ref().is_some_and(|p| p.id == id) {
                    self.pending = None;
                }
                CommandOutcome::Handled
            }
            ClientCommand::Submit { text } => CommandOutcome::Submit(text),
            ClientCommand::SetSession { session_id } => CommandOutcome::SetSession(session_id),
            ClientCommand::NewSession => CommandOutcome::NewSession,
            ClientCommand::SetModel {
                provider,
                model,
                effort,
            } => CommandOutcome::SetModel {
                provider,
                model,
                effort,
            },
            ClientCommand::SetMode { mode } => CommandOutcome::SetMode(mode),
            ClientCommand::SetSetting { id, value } => CommandOutcome::SetSetting { id, value },
            ClientCommand::ToggleSkill { name } => CommandOutcome::ToggleSkill(name),
            ClientCommand::ToggleMcp { name } => CommandOutcome::ToggleMcp(name),
            ClientCommand::ListSessions => CommandOutcome::ListSessions,
            ClientCommand::ResumeSession { session_id } => {
                CommandOutcome::ResumeSession(session_id)
            }
            ClientCommand::Copy { text } => CommandOutcome::Copy(text),
            // Mechanical control signals — classified by `control_signal` (also
            // unit-tested standalone); always `Some` for these three.
            cmd @ (ClientCommand::Cancel
            | ClientCommand::Inject { .. }
            | ClientCommand::Shutdown) => {
                CommandOutcome::Control(control_signal(&cmd).expect("control variant classified"))
            }
        }
    }

    /// Pull the next command from the active frontend. `None` means the active
    /// frontend disconnected with no successor.
    pub async fn next_command(&mut self) -> Option<ClientCommand> {
        loop {
            match self.acceptor.active_mut() {
                Some(port) => match port.next_command().await {
                    Some(cmd) => return Some(cmd),
                    // Active frontend closed its command stream: hand over and
                    // retry against the successor (if any).
                    None => {
                        if !self.handover().await {
                            return None;
                        }
                    }
                },
                None => return None,
            }
        }
    }

    /// Emit a frame, transparently recovering from a disconnected active
    /// frontend by promoting the next queued one.
    async fn send(&mut self, frame: Outbound) {
        loop {
            let Some(port) = self.acceptor.active_mut() else {
                return;
            };
            match port.emit(frame.clone()).await {
                Ok(()) => return,
                Err(_) => {
                    if !self.handover().await {
                        return;
                    }
                    // Successor promoted; retry the same frame against it.
                }
            }
        }
    }

    /// Promote the next queued frontend after the active one died. On success,
    /// re-establish it with a snapshot (carrying any in-flight request). On
    /// failure (no successor), cancel every outstanding request so blocked
    /// tools fail fast. Returns whether a successor took over.
    async fn handover(&mut self) -> bool {
        if self.acceptor.handover() {
            let snapshot = self.snapshot();
            // Best-effort: if the successor is *also* already gone, the next
            // emit/next_command will drive another handover.
            if let Some(port) = self.acceptor.active_mut() {
                let _ = port.emit(Outbound::Snapshot(Box::new(snapshot))).await;
            }
            true
        } else {
            self.broker.cancel_all();
            self.pending = None;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::frontend::port::{FrontendPort, PortError};
    use crate::console::frontend::protocol::ReplyAnswer;
    use crate::console::picker::{PickerAnswer, PickerQuestion};
    use async_trait::async_trait;
    use tokio::sync::{mpsc, oneshot};

    fn question() -> PickerQuestion {
        PickerQuestion {
            question: "proceed?".to_string(),
            kind: "ask_user".to_string(),
            header: "Q".to_string(),
            multi_select: false,
            options: vec![],
            allow_other: true,
            text_input: false,
            mask: false,
        }
    }

    fn picker_request() -> (PickerRequest, oneshot::Receiver<PickerResponse>) {
        let (tx, rx) = oneshot::channel();
        (
            PickerRequest {
                questions: vec![question()],
                reply: tx,
            },
            rx,
        )
    }

    struct VecPort {
        sent: mpsc::UnboundedSender<Outbound>,
        cmds: mpsc::UnboundedReceiver<ClientCommand>,
        /// Force emit to fail, simulating a dead transport.
        dead: bool,
    }

    #[async_trait]
    impl FrontendPort for VecPort {
        async fn emit(&mut self, frame: Outbound) -> Result<(), PortError> {
            if self.dead {
                return Err(PortError::Disconnected);
            }
            self.sent.send(frame).map_err(|_| PortError::Disconnected)
        }
        async fn next_command(&mut self) -> Option<ClientCommand> {
            self.cmds.recv().await
        }
    }

    fn port() -> (
        VecPort,
        mpsc::UnboundedReceiver<Outbound>,
        mpsc::UnboundedSender<ClientCommand>,
    ) {
        let (s_tx, s_rx) = mpsc::unbounded_channel();
        let (c_tx, c_rx) = mpsc::unbounded_channel();
        (
            VecPort {
                sent: s_tx,
                cmds: c_rx,
                dead: false,
            },
            s_rx,
            c_tx,
        )
    }

    fn hub_with(port: Box<dyn FrontendPort>) -> FrontendHub {
        let mut acc = Acceptor::new();
        acc.attach(port);
        FrontendHub::new(
            "s1".to_string(),
            String::new(),
            String::new(),
            String::new(),
            Vec::new(),
            acc,
            RequestBroker::new(),
        )
    }

    #[tokio::test]
    async fn reply_resolves_broker_and_clears_pending() {
        let (p, _sent, _cmds) = port();
        let mut hub = hub_with(Box::new(p));
        let (req, rx) = picker_request();
        hub.open_request(req).await;
        assert!(hub.pending.is_some());
        let id = hub.pending.as_ref().unwrap().id;

        let outcome = hub.handle_command(ClientCommand::Reply {
            id,
            answer: ReplyAnswer::Answered(vec![PickerAnswer::Single("ok".to_string())]),
        });
        assert_eq!(outcome, CommandOutcome::Handled);
        assert!(hub.pending.is_none());
        assert!(matches!(
            rx.await.unwrap(),
            PickerResponse::Answered(v) if v == vec![PickerAnswer::Single("ok".to_string())]
        ));
    }

    #[tokio::test]
    async fn submit_and_control_are_classified() {
        let (p, _sent, _cmds) = port();
        let mut hub = hub_with(Box::new(p));
        assert_eq!(
            hub.handle_command(ClientCommand::Submit {
                text: "/model".to_string()
            }),
            CommandOutcome::Submit("/model".to_string())
        );
        assert_eq!(
            hub.handle_command(ClientCommand::Cancel),
            CommandOutcome::Control(ControlSignal::Cancel)
        );
        assert_eq!(
            hub.handle_command(ClientCommand::Copy {
                text: "hi".to_string()
            }),
            CommandOutcome::Copy("hi".to_string())
        );
    }

    #[tokio::test]
    async fn disconnect_hands_over_and_snapshots_pending_request() {
        // Active port is dead; a healthy successor is queued. An open request
        // must survive the swap via the snapshot.
        let (mut dead, _dead_sent, _dc) = port();
        dead.dead = true;
        let (live, mut live_sent, _lc) = port();
        let mut acc = Acceptor::new();
        acc.attach(Box::new(dead));
        acc.attach(Box::new(live));
        let mut hub = FrontendHub::new(
            "s1".to_string(),
            String::new(),
            String::new(),
            String::new(),
            Vec::new(),
            acc,
            RequestBroker::new(),
        );

        let (req, _rx) = picker_request();
        // open_request emits to the dead active port -> triggers handover ->
        // the live successor receives a Snapshot carrying the pending request.
        hub.open_request(req).await;

        let frame = live_sent.recv().await.expect("successor got a frame");
        match frame {
            Outbound::Snapshot(s) => {
                assert_eq!(s.session_id, "s1");
                assert!(s.pending_request.is_some(), "pending request carried over");
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
        assert!(hub.has_active());
    }

    #[tokio::test]
    async fn disconnect_with_no_successor_cancels_requests() {
        let (mut dead, _sent, _c) = port();
        dead.dead = true;
        let mut acc = Acceptor::new();
        acc.attach(Box::new(dead));
        let mut hub = FrontendHub::new(
            "s1".to_string(),
            String::new(),
            String::new(),
            String::new(),
            Vec::new(),
            acc,
            RequestBroker::new(),
        );

        let (req, rx) = picker_request();
        hub.open_request(req).await; // emit fails, no successor -> cancel_all
        assert!(!hub.has_active());
        assert!(matches!(rx.await.unwrap(), PickerResponse::Cancelled));
    }
}
