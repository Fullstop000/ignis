//! Adapter between the wire-level [`ClientCommand`] and the console's existing
//! internal signals.
//!
//! The frontend protocol intentionally keeps the upstream surface tiny:
//! `Submit` / `Inject` / `Cancel` / `Reply` / `Shutdown`. But the console's
//! current plumbing splits along different lines, and mapping between them
//! exposes *where the responsibility boundary actually sits* — which is the
//! whole point of this phase:
//!
//!   * `Cancel` / `Inject` / `Shutdown` are pure transport signals: they carry
//!     no session state and map mechanically onto the existing cancel channel,
//!     inject source, and quit flag. [`ControlSignal`] captures these.
//!   * `Reply` is answered by the [`super::RequestBroker`], not here.
//!   * `Submit` is NOT mechanical. Slash dispatch (`/clear`, `/model`,
//!     `/compact`, skills, …) mutates `App` and resolves the active
//!     `session_id` — both of which are *core* state the frontend can't supply.
//!     So a submit stays a raw line that the core-side dispatcher
//!     (`keys::submit_text`) interprets; it is deliberately absent from
//!     [`ControlSignal`] to make that ownership explicit.

use crate::console::frontend::protocol::ClientCommand;

/// A mechanically-mappable upstream command — one that needs no `App` or
/// session context to act on. `Submit` and `Reply` are intentionally excluded
/// (see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlSignal {
    /// Steer the in-flight turn with extra text (Ctrl+S today).
    Inject(String),
    /// Cancel the current turn (Ctrl+C / ESC).
    Cancel,
    /// The frontend is detaching; the session should wind down.
    Shutdown,
}

/// Classify a [`ClientCommand`]. `Submit`/`Reply` return `None` because they
/// are routed elsewhere (the slash dispatcher and the broker, respectively).
pub fn control_signal(cmd: &ClientCommand) -> Option<ControlSignal> {
    match cmd {
        ClientCommand::Inject { text } => Some(ControlSignal::Inject(text.clone())),
        ClientCommand::Cancel => Some(ControlSignal::Cancel),
        ClientCommand::Shutdown => Some(ControlSignal::Shutdown),
        // Submit / Reply / SetSession / NewSession carry session state and are
        // routed by the hub (dispatcher / broker / session ops), not as signals.
        ClientCommand::Submit { .. }
        | ClientCommand::Reply { .. }
        | ClientCommand::SetSession { .. }
        | ClientCommand::NewSession
        | ClientCommand::SetModel { .. }
        | ClientCommand::SetMode { .. }
        | ClientCommand::ToggleSkill { .. }
        | ClientCommand::ToggleMcp { .. }
        | ClientCommand::ListSessions
        | ClientCommand::ResumeSession { .. } => None,
    }
}

/// Pull the submit line out of a command, if it is one. Lets a runner route
/// submits to the core dispatcher without re-matching the whole enum.
pub fn submit_text(cmd: &ClientCommand) -> Option<&str> {
    match cmd {
        ClientCommand::Submit { text } => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::frontend::protocol::ReplyAnswer;

    #[test]
    fn control_signals_map_mechanically() {
        assert_eq!(
            control_signal(&ClientCommand::Cancel),
            Some(ControlSignal::Cancel)
        );
        assert_eq!(
            control_signal(&ClientCommand::Shutdown),
            Some(ControlSignal::Shutdown)
        );
        assert_eq!(
            control_signal(&ClientCommand::Inject {
                text: "hi".to_string()
            }),
            Some(ControlSignal::Inject("hi".to_string()))
        );
    }

    #[test]
    fn submit_and_reply_are_not_control_signals() {
        // These route through the dispatcher / broker, not the mechanical path.
        assert_eq!(
            control_signal(&ClientCommand::Submit {
                text: "/model".to_string()
            }),
            None
        );
        assert_eq!(
            control_signal(&ClientCommand::Reply {
                id: 0,
                answer: ReplyAnswer::Cancelled,
            }),
            None
        );
        // NewSession is a session op routed by the hub, not a mechanical signal.
        assert_eq!(control_signal(&ClientCommand::NewSession), None);
    }

    #[test]
    fn submit_text_extracts_only_submits() {
        assert_eq!(
            submit_text(&ClientCommand::Submit {
                text: "hello".to_string()
            }),
            Some("hello")
        );
        assert_eq!(submit_text(&ClientCommand::Cancel), None);
    }
}
