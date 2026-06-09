//! Regression test for the inline-viewport DSR crash.
//!
//! Re-anchoring the inline viewport (on a band rebuild or a terminal resize)
//! queries the cursor row with a DSR (`ESC[6n`) and crossterm waits ~2s for the
//! reply. Under output backpressure on a slow pty (WSL2, tmux) that reply can
//! land late; the timeout used to `?`-bubble out of `run_console` and tear down
//! the whole TUI mid-session. This test withholds the DSR reply across a resize
//! and asserts the TUI rides it out instead of exiting.
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Tui {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output: Arc<Mutex<String>>,
    answer_dsr: Arc<AtomicBool>,
}

impl Tui {
    fn spawn(home: &Path, project: &Path) -> Self {
        let ignis_home = home.join(".ignis");
        std::fs::create_dir_all(&ignis_home).unwrap();
        std::fs::write(
            ignis_home.join("config.toml"),
            "active_provider = \"ollama\"\n\n[providers.ollama]\napi_url = \"http://127.0.0.1:11434\"\nmodel = \"test-model\"\n",
        )
        .unwrap();

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        let project = std::fs::canonicalize(project).unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ignis"));
        command.cwd(project.as_os_str());
        command.env("HOME", home.as_os_str());
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");

        let child = pair.slave.spawn_command(command).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let output = Arc::new(Mutex::new(String::new()));
        let answer_dsr = Arc::new(AtomicBool::new(true));

        let output_t = Arc::clone(&output);
        let writer_t = Arc::clone(&writer);
        let answer_t = Arc::clone(&answer_dsr);
        // Reader thread: answer DSR queries with a fixed cursor report — but
        // only while `answer_dsr` is set. Flipping it off simulates a terminal
        // that has stopped replying (the WSL2/tmux backpressure case).
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        let dsr = text.matches("\x1b[6n").count();
                        if dsr > 0 && answer_t.load(Ordering::SeqCst) {
                            let mut w = writer_t.lock().unwrap();
                            for _ in 0..dsr {
                                let _ = w.write_all(b"\x1b[1;1R");
                            }
                            let _ = w.flush();
                        }
                        output_t.lock().unwrap().push_str(&text);
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            master: pair.master,
            writer,
            output,
            answer_dsr,
        }
    }

    fn send(&mut self, input: &str) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(input.as_bytes()).unwrap();
        w.flush().unwrap();
    }

    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.output.lock().unwrap().contains(needle) {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "TUI exited before `{needle}` with status {status:?}\n{}",
                    self.snapshot()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for `{needle}`\n{}", self.snapshot());
    }

    fn snapshot(&self) -> String {
        let o = self.output.lock().unwrap();
        o[o.len().saturating_sub(4000)..].to_string()
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn inline_tui_survives_cursor_read_timeout_on_resize() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = Tui::spawn(home.path(), project.path());
    // Startup DSR is answered, so the TUI boots and paints its banner.
    tui.wait_for("Your AI coding agent");

    // Stop answering DSR, then resize: ignis detects the new size, rebuilds the
    // inline viewport, and the rebuild's `ESC[6n` goes unanswered. crossterm
    // returns its "cursor position could not be read within a normal duration"
    // error after ~2s. The fix classifies that as transient and skips the frame;
    // before the fix it `?`-bubbled and the process exited.
    tui.answer_dsr.store(false, Ordering::SeqCst);
    tui.master
        .resize(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    // Hold the timeout window open well past crossterm's 2s deadline, asserting
    // the child stays alive throughout. Without the fix it exits ~2s in.
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if let Some(status) = tui.child.try_wait().unwrap() {
            panic!(
                "TUI crashed on a cursor-read timeout (status {status:?}) — the inline rebuild \
                 should skip the frame, not tear down the session\n{}",
                tui.snapshot()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Resume answering DSR; the next rebuild succeeds and the TUI is fully
    // interactive again. Two Ctrl-Ds exit cleanly, proving recovery.
    tui.answer_dsr.store(true, Ordering::SeqCst);
    tui.send("\x04");
    tui.wait_for("Press Ctrl-D again to exit");
    tui.send("\x04");

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = tui.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "TUI exited unsuccessfully: {status:?}\n{}",
                tui.snapshot()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TUI did not exit after recovery\n{}",
            tui.snapshot()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Regression guard for the "blank after input, full content on resume" wedge.
///
/// In inline mode the live band never draws conversation content — every block
/// (user prompt, assistant text, tool calls, notices) reaches the screen ONLY
/// through the commit loop's `insert_before` into native scrollback. That loop
/// is gated by `!app.pending_screen_clear`. A reset path (`/clear`, `/resume`)
/// sets `pending_screen_clear`, and it is cleared only when the re-anchor's
/// `try_rebuild` succeeds — which issues a DSR (`ESC[6n`) via ratatui's inline
/// `compute_inline_size`. On WSL2/conpty that DSR can lag indefinitely, so the
/// flag would stay set and the commit loop stay suppressed: nothing the agent
/// produces paints, even though it keeps running and persisting.
///
/// The fix bounds the gate: after `MAX_REANCHOR_ATTEMPTS` failed re-anchors the
/// runner forces a DSR-free re-anchor (`terminal.clear()`) and clears the flag,
/// so content paints even if the DSR never answers. This test holds the DSR
/// unanswered FOREVER across a `/clear` and asserts the queued "Started new
/// session" notice still reaches the screen within a bounded window. Before the
/// fix it never appears (wedged blank indefinitely); after it, the backstop
/// flushes it.
#[test]
fn inline_reset_renders_even_if_reanchor_dsr_never_answers() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = Tui::spawn(home.path(), project.path());
    // Startup DSR answered → the banner paints (the commit loop is healthy).
    tui.wait_for("Your AI coding agent");

    // Stop answering DSR for good, then trigger a reset. `/clear` sets
    // `pending_screen_clear` and queues a "Started new session" notice block
    // that must flow to scrollback via the (gated) commit loop.
    tui.answer_dsr.store(false, Ordering::SeqCst);
    tui.send("/clear\r");

    // The re-anchor's DSR is never answered. Each `try_rebuild` blocks ~2s on
    // crossterm's DSR timeout; after MAX_REANCHOR_ATTEMPTS the backstop forces a
    // DSR-free re-anchor and the gated notice flushes. Allow generous slack for
    // the serialized timeouts. Pre-fix this `wait_for` would time out (blank
    // forever); post-fix the notice appears.
    tui.wait_for("Started new session");

    // And the session stays interactive afterward — two Ctrl-Ds exit cleanly.
    tui.send("\x04");
    tui.wait_for("Press Ctrl-D again to exit");
    tui.send("\x04");
}
