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
use crate::console::frontend::protocol::{ClientCommand, ClientRequest, Outbound, Snapshot};
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
    /// A mechanical control signal (cancel / inject / shutdown).
    Control(ControlSignal),
    /// A `Reply` the hub already resolved against the broker — nothing left for
    /// the runner to do.
    Handled,
}

pub struct FrontendHub {
    session_id: String,
    acceptor: Acceptor,
    broker: RequestBroker,
    /// The single picker awaiting an answer, if any. The console opens pickers
    /// one at a time, so one slot is enough; it rides a [`Snapshot`] on
    /// handover so a successor frontend can finish answering it.
    pending: Option<ClientRequest>,
}

impl FrontendHub {
    pub fn new(session_id: String, acceptor: Acceptor, broker: RequestBroker) -> Self {
        Self {
            session_id,
            acceptor,
            broker,
            pending: None,
        }
    }

    pub fn has_active(&self) -> bool {
        self.acceptor.has_active()
    }

    /// Push a streaming agent event to the active frontend, recovering from a
    /// disconnect by handing over to the next queued frontend.
    pub async fn emit_event(&mut self, ev: crate::AgentEvent) {
        self.send(Outbound::Event(Box::new(ev))).await;
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
        if let ClientCommand::Reply { id, answer } = cmd {
            let response: PickerResponse = answer.into();
            self.broker.resolve(id, response);
            // Clear the in-flight slot once its matching reply lands.
            if self.pending.as_ref().is_some_and(|p| p.id == id) {
                self.pending = None;
            }
            return CommandOutcome::Handled;
        }
        if let ClientCommand::Submit { text } = &cmd {
            return CommandOutcome::Submit(text.clone());
        }
        if let ClientCommand::SetSession { session_id } = &cmd {
            return CommandOutcome::SetSession(session_id.clone());
        }
        // Remaining variants are mechanical control signals.
        match control_signal(&cmd) {
            Some(sig) => CommandOutcome::Control(sig),
            // Unreachable: Submit/Reply handled above, so control_signal covers
            // the rest. Treat any gap defensively as already-handled.
            None => CommandOutcome::Handled,
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
            let snapshot = Snapshot {
                session_id: self.session_id.clone(),
                pending_request: self.pending.clone(),
            };
            // Best-effort: if the successor is *also* already gone, the next
            // emit/next_command will drive another handover.
            if let Some(port) = self.acceptor.active_mut() {
                let _ = port.emit(Outbound::Snapshot(snapshot)).await;
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
        FrontendHub::new("s1".to_string(), acc, RequestBroker::new())
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
        let mut hub = FrontendHub::new("s1".to_string(), acc, RequestBroker::new());

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
        let mut hub = FrontendHub::new("s1".to_string(), acc, RequestBroker::new());

        let (req, rx) = picker_request();
        hub.open_request(req).await; // emit fails, no successor -> cancel_all
        assert!(!hub.has_active());
        assert!(matches!(rx.await.unwrap(), PickerResponse::Cancelled));
    }
}
