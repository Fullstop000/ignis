//! In-process [`FrontendPort`] — the seam for the bundled ratatui TUI.
//!
//! Unlike an out-of-process transport (Ink subprocess, web socket), the TUI
//! lives in the same process as the agent core, so frames never serialize:
//! [`Outbound`] and [`ClientCommand`] values move across a pair of tokio
//! channels as plain Rust. This is the first concrete `FrontendPort` and the
//! seam the existing [`crate::console::runner`] will adopt — the core holds the
//! [`InProcessTuiPort`], the render loop holds the matching [`FrontendChannels`].
//!
//! Direction recap (matches the trait): the core *emits* downstream frames and
//! *takes* upstream commands; the frontend side mirrors that — it receives
//! `Outbound` and sends `ClientCommand`.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::console::frontend::port::{FrontendPort, PortError};
use crate::console::frontend::protocol::{ClientCommand, Outbound};

/// The core-held half: implements [`FrontendPort`] over bounded channels.
pub struct InProcessTuiPort {
    outbound: mpsc::Sender<Outbound>,
    commands: mpsc::Receiver<ClientCommand>,
}

/// The frontend-held half: the render loop receives downstream frames here and
/// sends user commands back. Returned alongside [`InProcessTuiPort`] from
/// [`in_process`] so the two ends share one channel pair.
pub struct FrontendChannels {
    pub outbound: mpsc::Receiver<Outbound>,
    pub commands: mpsc::Sender<ClientCommand>,
}

/// Create a connected core/frontend channel pair. `buffer` bounds each
/// direction; the existing runner uses 256 for events, so a similar order of
/// magnitude keeps backpressure behavior familiar.
pub fn in_process(buffer: usize) -> (InProcessTuiPort, FrontendChannels) {
    let (out_tx, out_rx) = mpsc::channel(buffer);
    let (cmd_tx, cmd_rx) = mpsc::channel(buffer);
    (
        InProcessTuiPort {
            outbound: out_tx,
            commands: cmd_rx,
        },
        FrontendChannels {
            outbound: out_rx,
            commands: cmd_tx,
        },
    )
}

#[async_trait]
impl FrontendPort for InProcessTuiPort {
    async fn emit(&mut self, frame: Outbound) -> Result<(), PortError> {
        // A closed receiver means the render loop is gone — surface it as a
        // disconnect so the acceptor can hand over (or the core can wind down).
        self.outbound
            .send(frame)
            .await
            .map_err(|_| PortError::Disconnected)
    }

    async fn next_command(&mut self) -> Option<ClientCommand> {
        self.commands.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::frontend::protocol::ReplyAnswer;

    #[tokio::test]
    async fn emit_reaches_frontend_in_order() {
        let (mut port, mut fe) = in_process(8);
        port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnStart)))
            .await
            .unwrap();
        port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnEnd)))
            .await
            .unwrap();
        assert!(matches!(
            fe.outbound.recv().await,
            Some(Outbound::Event(e)) if matches!(*e, crate::AgentEvent::TurnStart)
        ));
        assert!(matches!(
            fe.outbound.recv().await,
            Some(Outbound::Event(e)) if matches!(*e, crate::AgentEvent::TurnEnd)
        ));
    }

    #[tokio::test]
    async fn frontend_commands_reach_core() {
        let (mut port, fe) = in_process(8);
        fe.commands
            .send(ClientCommand::Submit {
                text: "hi".to_string(),
            })
            .await
            .unwrap();
        fe.commands
            .send(ClientCommand::Reply {
                id: 3,
                answer: ReplyAnswer::Cancelled,
            })
            .await
            .unwrap();
        assert!(matches!(
            port.next_command().await,
            Some(ClientCommand::Submit { text }) if text == "hi"
        ));
        assert!(matches!(
            port.next_command().await,
            Some(ClientCommand::Reply { id: 3, .. })
        ));
    }

    #[tokio::test]
    async fn dropped_frontend_surfaces_disconnect() {
        let (mut port, fe) = in_process(8);
        drop(fe);
        assert!(matches!(
            port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnEnd)))
                .await,
            Err(PortError::Disconnected)
        ));
        // And the command side reports end-of-stream.
        assert!(port.next_command().await.is_none());
    }
}
