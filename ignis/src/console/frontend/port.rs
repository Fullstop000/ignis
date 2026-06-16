//! The frontend seam: a [`FrontendPort`] trait the agent core talks to without
//! knowing what's on the other side, plus a single-slot FIFO [`Acceptor`] that
//! enforces "exactly one live frontend at a time".
//!
//! Why a trait: the core's event loop should depend only on "emit a frame /
//! take the next command", never on stdio, sockets, or React. The in-process
//! ratatui TUI is the first implementor; an Ink subprocess (NDJSON over a
//! pipe), a web client (JSON over a WebSocket), and an in-process plugin are
//! later implementors of the *same* trait.
//!
//! Why single-slot + FIFO: only one frontend may drive the session at once (so
//! a blocking `ask_user` has an unambiguous answerer). Additional frontends
//! that attach are not rejected and not made read-only — they wait in a queue
//! and, in arrival order, take over with full capability when the active one
//! disconnects. The successor is brought current via an [`Outbound::Snapshot`]
//! rather than a replay of missed frames.

use std::collections::VecDeque;

use async_trait::async_trait;

use crate::console::frontend::protocol::{ClientCommand, Outbound};

/// One concrete frontend attachment (the live ratatui loop, a connected Ink
/// subprocess, one web socket, …). The core drives it purely through this
/// interface.
#[async_trait]
pub trait FrontendPort: Send {
    /// Push a frame to the frontend. Point-to-point — there is only ever one
    /// active frontend, so there is no broadcast. An error means the frontend
    /// is gone; the acceptor will promote the next queued one.
    async fn emit(&mut self, frame: Outbound) -> Result<(), PortError>;

    /// Await the next command from the frontend. `None` means the frontend
    /// disconnected (clean shutdown or dropped transport).
    async fn next_command(&mut self) -> Option<ClientCommand>;
}

/// Why a port operation failed. Kept minimal: the core only needs to know the
/// frontend is unreachable so it can fail over.
#[derive(Debug)]
pub enum PortError {
    /// The transport is closed/broken; this port is dead.
    Disconnected,
}

/// Holds the one active frontend and a FIFO queue of frontends waiting to take
/// over. Transport-agnostic: it manipulates `Box<dyn FrontendPort>` only.
#[derive(Default)]
pub struct Acceptor {
    active: Option<Box<dyn FrontendPort>>,
    waiting: VecDeque<Box<dyn FrontendPort>>,
}

impl Acceptor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a frontend. If no frontend is active it becomes active
    /// immediately and `true` is returned (the caller should then send it a
    /// snapshot). Otherwise it joins the back of the FIFO queue and `false` is
    /// returned.
    pub fn attach(&mut self, port: Box<dyn FrontendPort>) -> bool {
        if self.active.is_none() {
            self.active = Some(port);
            true
        } else {
            self.waiting.push_back(port);
            false
        }
    }

    pub fn active_mut(&mut self) -> Option<&mut (dyn FrontendPort + 'static)> {
        self.active.as_deref_mut()
    }

    pub fn has_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn waiting_count(&self) -> usize {
        self.waiting.len()
    }

    /// The active frontend went away. Drop it and promote the next queued one
    /// (FIFO). Returns `true` if a successor was promoted — the caller then
    /// hands it a snapshot (including any in-flight request) so it starts with
    /// full session context. `false` means no frontend remains, and the caller
    /// should cancel outstanding requests.
    pub fn handover(&mut self) -> bool {
        self.active = self.waiting.pop_front();
        self.active.is_some()
    }
}
