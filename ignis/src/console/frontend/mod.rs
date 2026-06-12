//! Transport-agnostic frontend layer.
//!
//! This is the seam that lets the agent core drive any frontend — the
//! in-process ratatui TUI today, an out-of-process Ink `ignis-tui`, a web
//! client, or a plugin — through one contract:
//!
//!   * [`protocol`] — the `serde` wire types ([`Outbound`], [`ClientCommand`], …).
//!   * [`broker`] — bridges tools' blocking `oneshot` picker requests across
//!     the frontend boundary via correlation ids.
//!   * [`port`] — the [`FrontendPort`] trait and the single-slot FIFO
//!     [`Acceptor`] that guarantees exactly one live frontend.
//!   * [`command`] — maps wire [`ClientCommand`]s onto the console's existing
//!     internal signals, making the responsibility boundary explicit.

pub mod broker;
pub mod command;
pub mod port;
pub mod protocol;

pub use broker::RequestBroker;
pub use command::{control_signal, ControlSignal};
pub use port::{Acceptor, FrontendPort, PortError};
pub use protocol::{ClientCommand, ClientRequest, Outbound, ReplyAnswer, RequestId, Snapshot};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::picker::{PickerAnswer, PickerQuestion, PickerRequest, PickerResponse};
    use async_trait::async_trait;
    use tokio::sync::{mpsc, oneshot};

    fn question(text: &str) -> PickerQuestion {
        PickerQuestion {
            question: text.to_string(),
            kind: "ask_user".to_string(),
            header: "Q".to_string(),
            multi_select: false,
            options: vec![],
            allow_other: true,
            text_input: false,
            mask: false,
        }
    }

    fn picker_request(text: &str) -> (PickerRequest, oneshot::Receiver<PickerResponse>) {
        let (tx, rx) = oneshot::channel();
        (
            PickerRequest {
                questions: vec![question(text)],
                reply: tx,
            },
            rx,
        )
    }

    #[test]
    fn ui_command_reply_round_trips_through_json() {
        let cmd = ClientCommand::Reply {
            id: 7,
            answer: ReplyAnswer::Answered(vec![PickerAnswer::Single("yes".to_string())]),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: ClientCommand = serde_json::from_str(&json).unwrap();
        match back {
            ClientCommand::Reply { id, answer } => {
                assert_eq!(id, 7);
                assert!(matches!(answer, ReplyAnswer::Answered(v) if v.len() == 1));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn submit_command_round_trips() {
        let cmd = ClientCommand::Submit {
            text: "/compact".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: ClientCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ClientCommand::Submit { text } if text == "/compact"));
    }

    #[test]
    fn outbound_event_serializes_with_kind_tag() {
        let frame = Outbound::Event(Box::new(crate::AgentEvent::TurnStart));
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["kind"], "event");
        // The inner AgentEvent keeps its own type/payload tagging.
        assert_eq!(v["data"]["type"], "turn_start");
    }

    #[tokio::test]
    async fn broker_resolves_pending_request_and_wakes_tool() {
        let mut broker = RequestBroker::new();
        let (req, rx) = picker_request("proceed?");
        let ui = broker.register(req);
        assert!(broker.is_pending(ui.id));

        let woke = broker.resolve(
            ui.id,
            PickerResponse::Answered(vec![PickerAnswer::Single("ok".to_string())]),
        );
        assert!(woke);
        assert!(!broker.is_pending(ui.id));
        // The blocked tool receives exactly what the frontend answered.
        let got = rx.await.unwrap();
        assert!(
            matches!(got, PickerResponse::Answered(v) if v == vec![PickerAnswer::Single("ok".to_string())])
        );
    }

    #[tokio::test]
    async fn broker_unknown_id_is_ignored() {
        let mut broker = RequestBroker::new();
        let (req, _rx) = picker_request("x");
        let ui = broker.register(req);
        // A stale/foreign id does nothing and leaves the real one pending.
        assert!(!broker.resolve(ui.id + 999, PickerResponse::Cancelled));
        assert!(broker.is_pending(ui.id));
    }

    #[tokio::test]
    async fn broker_cancel_all_unblocks_every_tool() {
        let mut broker = RequestBroker::new();
        let (r1, rx1) = picker_request("a");
        let (r2, rx2) = picker_request("b");
        broker.register(r1);
        broker.register(r2);
        assert_eq!(broker.pending_count(), 2);

        broker.cancel_all();
        assert_eq!(broker.pending_count(), 0);
        assert!(matches!(rx1.await.unwrap(), PickerResponse::Cancelled));
        assert!(matches!(rx2.await.unwrap(), PickerResponse::Cancelled));
    }

    #[tokio::test]
    async fn broker_drop_cancels_outstanding() {
        let (req, rx) = picker_request("c");
        {
            let mut broker = RequestBroker::new();
            broker.register(req);
        } // dropped here
        assert!(matches!(rx.await.unwrap(), PickerResponse::Cancelled));
    }

    /// Minimal port backed by channels — stands in for a real transport in
    /// acceptor tests.
    struct ChannelPort {
        out: mpsc::UnboundedSender<Outbound>,
        cmd: mpsc::UnboundedReceiver<ClientCommand>,
    }

    #[async_trait]
    impl FrontendPort for ChannelPort {
        async fn emit(&mut self, frame: Outbound) -> Result<(), PortError> {
            self.out.send(frame).map_err(|_| PortError::Disconnected)
        }
        async fn next_command(&mut self) -> Option<ClientCommand> {
            self.cmd.recv().await
        }
    }

    fn channel_port() -> (
        ChannelPort,
        mpsc::UnboundedReceiver<Outbound>,
        mpsc::UnboundedSender<ClientCommand>,
    ) {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (
            ChannelPort {
                out: out_tx,
                cmd: cmd_rx,
            },
            out_rx,
            cmd_tx,
        )
    }

    #[test]
    fn acceptor_first_attach_is_active_rest_queue_fifo() {
        let mut acc = Acceptor::new();
        let (p1, _o1, _c1) = channel_port();
        let (p2, _o2, _c2) = channel_port();
        let (p3, _o3, _c3) = channel_port();

        assert!(acc.attach(Box::new(p1)), "first frontend activates");
        assert!(!acc.attach(Box::new(p2)), "second queues");
        assert!(!acc.attach(Box::new(p3)), "third queues");
        assert!(acc.has_active());
        assert_eq!(acc.waiting_count(), 2);
    }

    #[test]
    fn acceptor_handover_promotes_in_arrival_order() {
        let mut acc = Acceptor::new();
        let (p1, _o1, _c1) = channel_port();
        let (p2, mut o2, _c2) = channel_port();
        let (p3, _o3, _c3) = channel_port();
        acc.attach(Box::new(p1));
        acc.attach(Box::new(p2));
        acc.attach(Box::new(p3));

        // p1 leaves -> p2 (first queued) takes over.
        assert!(acc.handover());
        assert_eq!(acc.waiting_count(), 1);
        // The now-active port is the one whose receiver is o2: emitting reaches it.
        futures_executor_block(async {
            acc.active_mut()
                .unwrap()
                .emit(Outbound::Event(Box::new(crate::AgentEvent::TurnEnd)))
                .await
                .unwrap();
        });
        assert!(o2.try_recv().is_ok(), "p2 is the active frontend now");

        // p2 leaves -> p3.
        assert!(acc.handover());
        assert_eq!(acc.waiting_count(), 0);
        // p3 leaves -> no successor.
        assert!(!acc.handover());
        assert!(!acc.has_active());
    }

    /// Tiny block-on so the acceptor test can exercise the async `emit` without
    /// pulling a full runtime into a sync test.
    fn futures_executor_block<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
