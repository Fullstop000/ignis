//! stdio NDJSON [`FrontendPort`] — the transport for an out-of-process frontend
//! (the Ink `ignis-tui`, or any program that speaks the protocol over a pipe).
//!
//! One self-describing JSON value per line, both directions:
//!   * core → frontend: each [`Outbound`] frame is `serde_json`-encoded and
//!     written as a single `\n`-terminated line to the child's stdin.
//!   * frontend → core: each line read from the child's stdout is decoded as a
//!     [`ClientCommand`].
//!
//! Line framing (no length prefix) keeps the stream `cat`-friendly: a captured
//! session is a plain NDJSON file you can replay into a frontend offline, and a
//! frontend's output can be eyeballed. EOF on the read side (the child closed
//! its stdout / exited) is a clean disconnect; the acceptor then hands over or
//! the core winds down, exactly as for the local port.
//!
//! [`StdioPort`] is generic over any [`AsyncWrite`]/[`AsyncRead`] so the framing
//! logic is unit-testable over an in-memory pipe — no child process required.
//! [`spawn_stdio_port`] is the thin glue that turns a real child's piped
//! stdin/stdout into one; it is the only process-aware part.

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::console::frontend::port::{FrontendPort, PortError};
use crate::console::frontend::protocol::{ClientCommand, Outbound};

/// A [`FrontendPort`] that speaks NDJSON over a byte stream pair. `W` is the
/// frontend's input (we write `Outbound` lines); `R` is its output (we read
/// `ClientCommand` lines).
pub struct StdioPort<W, R> {
    writer: W,
    reader: BufReader<R>,
    /// Reused read buffer so a long session doesn't reallocate per command.
    line: String,
}

impl<W, R> StdioPort<W, R>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    pub fn new(writer: W, reader: R) -> Self {
        Self {
            writer,
            reader: BufReader::new(reader),
            line: String::new(),
        }
    }
}

#[async_trait]
impl<W, R> FrontendPort for StdioPort<W, R>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    async fn emit(&mut self, frame: Outbound) -> Result<(), PortError> {
        // Serialization of an owned `Outbound` is infallible in practice; treat
        // the impossible failure as a dead port rather than panicking a render.
        let mut line = serde_json::to_string(&frame).map_err(|e| {
            log::error!("stdio: failed to encode outbound frame: {e}");
            PortError::Disconnected
        })?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|_| PortError::Disconnected)?;
        // Flush per frame: a frontend rendering at ~30fps must see events as
        // they happen, not when an OS pipe buffer happens to fill.
        self.writer
            .flush()
            .await
            .map_err(|_| PortError::Disconnected)
    }

    async fn next_command(&mut self) -> Option<ClientCommand> {
        loop {
            self.line.clear();
            match self.reader.read_line(&mut self.line).await {
                // EOF: the frontend closed its output / exited.
                Ok(0) => return None,
                Ok(_) => {
                    let trimmed = self.line.trim();
                    // Tolerate blank/keepalive lines without churning the core.
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<ClientCommand>(trimmed) {
                        Ok(cmd) => return Some(cmd),
                        // A malformed line is a protocol violation: the peer is
                        // out of sync, so disconnect rather than guess.
                        Err(e) => {
                            log::warn!("stdio: malformed command, disconnecting: {e}");
                            return None;
                        }
                    }
                }
                // A read error means the pipe is broken — same as a disconnect.
                Err(_) => return None,
            }
        }
    }
}

/// Spawn `command` as a child process speaking the NDJSON protocol on its
/// stdin/stdout and wrap them in a [`StdioPort`]. Returns the port plus the
/// [`Child`](tokio::process::Child) handle — the caller must keep the handle
/// alive for the child's lifetime (dropping it would orphan the process); the
/// child is killed if the handle is dropped (`kill_on_drop`), so a torn-down
/// session never leaks a frontend process.
///
/// The shipped topology has the Ink host spawn the engine (`ignis --engine`),
/// not the reverse, so this constructor — the one process-aware seam, where
/// ignis would itself spawn and own a child frontend — is currently unused.
/// All framing lives in [`StdioPort`] above.
pub fn spawn_stdio_port(
    mut command: tokio::process::Command,
) -> std::io::Result<(
    StdioPort<tokio::process::ChildStdin, tokio::process::ChildStdout>,
    tokio::process::Child,
)> {
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("child stdin not piped"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child stdout not piped"))?;
    Ok((StdioPort::new(stdin, stdout), child))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::frontend::protocol::ReplyAnswer;

    /// emit() writes exactly one `\n`-terminated JSON line per frame, tagged
    /// like the wire protocol, and a reader on the other end sees it verbatim.
    #[tokio::test]
    async fn emit_writes_one_ndjson_line_per_frame() {
        // The port writes to `to_frontend`; the test reads the other duplex end.
        let (to_frontend, frontend_reads) = tokio::io::duplex(4096);
        // Command read side is unused here; keep its peer (`_cmd_feed`) alive so
        // it doesn't EOF.
        let (cmd_read, _cmd_feed) = tokio::io::duplex(64);
        let mut port = StdioPort::new(to_frontend, cmd_read);
        let mut reader = BufReader::new(frontend_reads).lines();

        port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnStart)))
            .await
            .unwrap();
        port.emit(Outbound::Event(Box::new(crate::AgentEvent::TurnEnd)))
            .await
            .unwrap();

        let l1 = reader.next_line().await.unwrap().expect("first line");
        let v1: serde_json::Value = serde_json::from_str(&l1).unwrap();
        assert_eq!(v1["kind"], "event");
        assert_eq!(v1["data"]["type"], "turn_start");
        let l2 = reader.next_line().await.unwrap().expect("second line");
        assert!(l2.contains("turn_end"), "second line is the turn_end event");
    }

    /// The `Todos` event serializes to the exact wire shape the Ink frontend's
    /// reducer reads: `{kind:"event", data:{type:"todos", payload:{items:[…]}}}`
    /// with snake_case status and a camelCase `activeForm`. Pins the engine→Ink
    /// contract that the e2e harness assumes (it hand-builds these frames).
    #[tokio::test]
    async fn emit_serializes_todos_event_for_ink() {
        use crate::tools::{Todo, TodoStatus};
        let (to_frontend, frontend_reads) = tokio::io::duplex(4096);
        let (cmd_read, _cmd_feed) = tokio::io::duplex(64);
        let mut port = StdioPort::new(to_frontend, cmd_read);
        let mut reader = BufReader::new(frontend_reads).lines();

        port.emit(Outbound::Event(Box::new(crate::AgentEvent::Todos {
            items: vec![
                Todo {
                    content: "build it".into(),
                    status: TodoStatus::InProgress,
                    active_form: Some("Building it".into()),
                },
                Todo {
                    content: "test it".into(),
                    status: TodoStatus::Pending,
                    active_form: None,
                },
            ],
        })))
        .await
        .unwrap();

        let line = reader.next_line().await.unwrap().expect("todos line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "event");
        assert_eq!(v["data"]["type"], "todos");
        let items = &v["data"]["payload"]["items"];
        assert_eq!(items[0]["content"], "build it");
        assert_eq!(items[0]["status"], "in_progress");
        assert_eq!(items[0]["activeForm"], "Building it");
        assert_eq!(items[1]["status"], "pending");
        // Absent activeForm is omitted (skip_serializing_if), not null.
        assert!(items[1].get("activeForm").is_none());
    }

    /// The `BackgroundShells` event serializes to the wire shape the Ink footer
    /// reducer reads: `{kind:"event", data:{type:"background_shells",
    /// payload:{running:N}}}`. Pins the engine→Ink contract.
    #[tokio::test]
    async fn emit_serializes_background_shells_event_for_ink() {
        let (to_frontend, frontend_reads) = tokio::io::duplex(4096);
        let (cmd_read, _cmd_feed) = tokio::io::duplex(64);
        let mut port = StdioPort::new(to_frontend, cmd_read);
        let mut reader = BufReader::new(frontend_reads).lines();

        port.emit(Outbound::Event(Box::new(
            crate::AgentEvent::BackgroundShells { running: 3 },
        )))
        .await
        .unwrap();

        let line = reader.next_line().await.unwrap().expect("bg line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "event");
        assert_eq!(v["data"]["type"], "background_shells");
        assert_eq!(v["data"]["payload"]["running"], 3);
    }

    /// The `FollowUps` event serializes to the wire shape the Ink reducer reads:
    /// `{kind:"event", data:{type:"follow_ups", payload:{items:[…]}}}`.
    #[tokio::test]
    async fn emit_serializes_follow_ups_event_for_ink() {
        let (to_frontend, frontend_reads) = tokio::io::duplex(4096);
        let (cmd_read, _cmd_feed) = tokio::io::duplex(64);
        let mut port = StdioPort::new(to_frontend, cmd_read);
        let mut reader = BufReader::new(frontend_reads).lines();

        port.emit(Outbound::Event(Box::new(crate::AgentEvent::FollowUps {
            items: vec![
                "Run the tests".to_string(),
                "Add error handling".to_string(),
            ],
        })))
        .await
        .unwrap();

        let line = reader.next_line().await.unwrap().expect("follow_ups line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "event");
        assert_eq!(v["data"]["type"], "follow_ups");
        assert_eq!(v["data"]["payload"]["items"][0], "Run the tests");
        assert_eq!(v["data"]["payload"]["items"][1], "Add error handling");
    }

    /// next_command() decodes one ClientCommand per line and skips blanks.
    #[tokio::test]
    async fn next_command_decodes_lines_and_skips_blanks() {
        // writer end unused; reader end fed by `feed`.
        let (sink, _sink_other) = tokio::io::duplex(64);
        let (cmd_read, mut feed) = tokio::io::duplex(4096);
        let mut port = StdioPort::new(sink, cmd_read);

        // A blank line, then a Submit, then a Cancel.
        feed.write_all(b"\n").await.unwrap();
        feed.write_all(b"{\"kind\":\"submit\",\"data\":{\"text\":\"hi\"}}\n")
            .await
            .unwrap();
        feed.write_all(b"{\"kind\":\"cancel\"}\n").await.unwrap();
        feed.flush().await.unwrap();

        assert!(matches!(
            port.next_command().await,
            Some(ClientCommand::Submit { text }) if text == "hi"
        ));
        assert!(matches!(
            port.next_command().await,
            Some(ClientCommand::Cancel)
        ));
    }

    /// A Reply round-trips so the broker bridge works over the wire too.
    #[tokio::test]
    async fn next_command_decodes_reply() {
        let (sink, _o) = tokio::io::duplex(64);
        let (cmd_read, mut feed) = tokio::io::duplex(4096);
        let mut port = StdioPort::new(sink, cmd_read);
        feed.write_all(b"{\"kind\":\"reply\",\"data\":{\"id\":7,\"answer\":\"Cancelled\"}}\n")
            .await
            .unwrap();
        feed.flush().await.unwrap();
        match port.next_command().await {
            Some(ClientCommand::Reply { id, answer }) => {
                assert_eq!(id, 7);
                assert!(matches!(answer, ReplyAnswer::Cancelled));
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    /// EOF on the read side is a clean disconnect (`None`).
    #[tokio::test]
    async fn eof_is_disconnect() {
        let (sink, _o) = tokio::io::duplex(64);
        let (cmd_read, feed) = tokio::io::duplex(64);
        let mut port = StdioPort::new(sink, cmd_read);
        drop(feed); // frontend closed its output
        assert!(port.next_command().await.is_none());
    }

    /// A malformed (non-JSON) line disconnects rather than guessing.
    #[tokio::test]
    async fn malformed_line_disconnects() {
        let (sink, _o) = tokio::io::duplex(64);
        let (cmd_read, mut feed) = tokio::io::duplex(4096);
        let mut port = StdioPort::new(sink, cmd_read);
        feed.write_all(b"not json at all\n").await.unwrap();
        feed.flush().await.unwrap();
        assert!(port.next_command().await.is_none());
    }
}
