//! Local [`FrontendPort`] — the seam for the bundled ratatui TUI.
//!
//! "Local" because this frontend lives in the same OS process as the agent
//! core (it's the TUI compiled into the single `ignis` binary), in contrast to
//! a remote frontend reached over a pipe or socket (an Ink subprocess, a web
//! client). Being local, frames never serialize: [`Outbound`] and
//! [`ClientCommand`] values cross a pair of tokio channels as plain Rust.
//!
//! [`local_tui`] hands back the two ends of one pipe: the core holds a
//! [`LocalTuiPort`] (which implements [`FrontendPort`]); the render loop holds
//! the matching [`TuiHandle`]. The existing [`crate::console::runner`] will
//! adopt this seam.
//!
//! Direction recap (matches the trait): the core *emits* downstream frames and
//! *takes* upstream commands; the handle mirrors that — it receives `Outbound`
//! and sends `ClientCommand`.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::console::frontend::port::{FrontendPort, PortError};
use crate::console::frontend::protocol::{ClientCommand, Outbound};

/// The core-held end: implements [`FrontendPort`] over bounded channels.
pub struct LocalTuiPort {
    outbound: mpsc::Sender<Outbound>,
    commands: mpsc::Receiver<ClientCommand>,
}

/// The frontend-held end: the render loop receives downstream frames here and
/// sends user commands back. Returned alongside [`LocalTuiPort`] from
/// [`local_tui`] so the two ends share one channel pair.
pub struct TuiHandle {
    pub outbound: mpsc::Receiver<Outbound>,
    pub commands: mpsc::Sender<ClientCommand>,
}

/// Connect a core/frontend pair for the bundled TUI. `buffer` bounds each
/// direction; the existing runner uses 256 for events, so a similar order of
/// magnitude keeps backpressure behavior familiar.
pub fn local_tui(buffer: usize) -> (LocalTuiPort, TuiHandle) {
    let (out_tx, out_rx) = mpsc::channel(buffer);
    let (cmd_tx, cmd_rx) = mpsc::channel(buffer);
    (
        LocalTuiPort {
            outbound: out_tx,
            commands: cmd_rx,
        },
        TuiHandle {
            outbound: out_rx,
            commands: cmd_tx,
        },
    )
}

#[async_trait]
impl FrontendPort for LocalTuiPort {
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
        let (mut port, mut handle) = local_tui(8);
        port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnStart)))
            .await
            .unwrap();
        port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnEnd)))
            .await
            .unwrap();
        assert!(matches!(
            handle.outbound.recv().await,
            Some(Outbound::Event(e)) if matches!(*e, crate::AgentEvent::TurnStart)
        ));
        assert!(matches!(
            handle.outbound.recv().await,
            Some(Outbound::Event(e)) if matches!(*e, crate::AgentEvent::TurnEnd)
        ));
    }

    #[tokio::test]
    async fn frontend_commands_reach_core() {
        let (mut port, handle) = local_tui(8);
        handle
            .commands
            .send(ClientCommand::Submit {
                text: "hi".to_string(),
            })
            .await
            .unwrap();
        handle
            .commands
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
        let (mut port, handle) = local_tui(8);
        drop(handle);
        assert!(matches!(
            port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnEnd)))
                .await,
            Err(PortError::Disconnected)
        ));
        // And the command side reports end-of-stream.
        assert!(port.next_command().await.is_none());
    }
}
