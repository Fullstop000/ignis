//! Request broker — bridges the tool side's blocking `oneshot` picker
//! mechanism across the (possibly out-of-process) frontend boundary.
//!
//! A tool that needs user input builds a [`PickerRequest`] carrying a
//! `oneshot::Sender<PickerResponse>` and blocks on the receiver. The broker
//! take that request, peels off the sender into an id-keyed table, and hands
//! back a [`ClientRequest`] for the active frontend to render. When the
//! frontend answers with [`ClientCommand::Reply`], the broker looks up the id
//! and fires the oneshot — so the tool wakes up exactly as it did with the
//! in-process channel, oblivious to whether the answer came from ratatui, an
//! Ink subprocess, or a web client.
//!
//! Invariant: every registered request resolves exactly once — via a matching
//! reply, or as [`PickerResponse::Cancelled`] when the frontend disconnects or
//! the broker is dropped. A tool must never block forever on a dead frontend.

use std::collections::HashMap;

use tokio::sync::oneshot;

use crate::console::frontend::protocol::{ClientRequest, RequestId};
use crate::console::picker::{PickerRequest, PickerResponse};

/// Owns the pending-request table and allocates correlation ids.
#[derive(Default)]
pub struct RequestBroker {
    next_id: RequestId,
    pending: HashMap<RequestId, oneshot::Sender<PickerResponse>>,
}

impl RequestBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool's picker request: store its reply channel under a fresh
    /// id and return the [`ClientRequest`] to surface to the active frontend.
    pub fn register(&mut self, req: PickerRequest) -> ClientRequest {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, req.reply);
        ClientRequest {
            id,
            questions: req.questions,
        }
    }

    /// Resolve an outstanding request with the frontend's answer. A stale or
    /// unknown id is ignored (the request may have already been cancelled on a
    /// handover); returns whether a waiting tool was actually woken.
    pub fn resolve(&mut self, id: RequestId, response: PickerResponse) -> bool {
        match self.pending.remove(&id) {
            // The receiver is gone if the tool itself was cancelled meanwhile;
            // a failed send is benign — nothing is waiting.
            Some(tx) => tx.send(response).is_ok(),
            None => false,
        }
    }

    /// Is a request with this id still awaiting an answer?
    pub fn is_pending(&self, id: RequestId) -> bool {
        self.pending.contains_key(&id)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Cancel every outstanding request — used when the active frontend
    /// disconnects with no successor, so blocked tools fail fast instead of
    /// hanging. (On a FIFO handover the in-flight request travels to the
    /// successor via the snapshot instead; this is the no-successor path.)
    pub fn cancel_all(&mut self) {
        for (_, tx) in self.pending.drain() {
            let _ = tx.send(PickerResponse::Cancelled);
        }
    }
}

impl Drop for RequestBroker {
    fn drop(&mut self) {
        self.cancel_all();
    }
}
