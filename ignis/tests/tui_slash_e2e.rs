use ignis::session::project_sessions_dir;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::json;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct TuiProcess {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
}

impl TuiProcess {
    fn spawn(home: &Path, project: &Path) -> Self {
        let ignis_home = home.join(".ignis");
        std::fs::create_dir_all(&ignis_home).unwrap();
        std::fs::write(
            ignis_home.join("config.toml"),
            "active_provider = \"ollama\"\n\n[providers.ollama]\napi_url = \"http://127.0.0.1:11434\"\nmodel = \"test-model\"\n",
        )
        .unwrap();

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();

        // Canonicalize so the cwd matches what the child's std::env::current_dir()
        // reports (macOS resolves /var -> /private/var), keeping the session slug
        // consistent with seed_session.
        let project = std::fs::canonicalize(project).unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ignis"));
        command.arg("--tui");
        command.cwd(project.as_os_str());
        command.env("HOME", home.as_os_str());
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");

        let child = pair.slave.spawn_command(command).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let output_for_thread = Arc::clone(&output);

        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        output_for_thread.lock().unwrap().push_str(&text);
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            child,
            writer,
            output,
        }
    }

    fn send(&mut self, input: &str) {
        self.writer.write_all(input.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.output.lock().unwrap().contains(needle) {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "TUI exited before rendering `{}` with status {:?}\n{}",
                    needle,
                    status,
                    self.snapshot()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for `{}`\n{}", needle, self.snapshot());
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "TUI exited unsuccessfully: {:?}\n{}",
                    status,
                    self.snapshot()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for TUI exit\n{}", self.snapshot());
    }

    fn snapshot(&self) -> String {
        let output = self.output.lock().unwrap();
        let len = output.len();
        output[len.saturating_sub(6000)..].to_string()
    }
}

impl Drop for TuiProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn seed_session(project: &Path, home: &Path, id: &str, user: &str, assistant: &str) {
    let project = std::fs::canonicalize(project).unwrap();
    let storage_dir = project_sessions_dir(&home.join(".ignis"), &project);
    std::fs::create_dir_all(&storage_dir).unwrap();
    let records = [
        json!({
            "type": "session_meta",
            "timestamp": 1,
            "payload": { "id": id }
        }),
        json!({
            "type": "message",
            "timestamp": 2,
            "payload": { "role": "user", "content": user }
        }),
        json!({
            "type": "message",
            "timestamp": 3,
            "payload": { "role": "assistant", "content": assistant }
        }),
    ];
    let mut content = String::new();
    for record in records {
        content.push_str(&serde_json::to_string(&record).unwrap());
        content.push('\n');
    }
    std::fs::write(storage_dir.join(format!("{}.jsonl", id)), content).unwrap();
}

#[test]
fn slash_autocomplete_can_run_resume_and_render_selected_history() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    seed_session(
        project.path(),
        home.path(),
        "beta",
        "beta user prompt",
        "beta assistant answer",
    );

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Type a prompt below");

    tui.send("/");
    tui.wait_for("/resume");
    tui.wait_for("/new");
    tui.wait_for("/clear");

    // First item (/resume) is already selected, just press Enter
    tui.send("\r");
    tui.wait_for("Sessions");
    tui.wait_for("beta");

    tui.send("\x1b[B\r");
    tui.wait_for("beta user prompt");
    tui.wait_for("beta assistant answer");
    tui.wait_for("Resumed session");
}

#[test]
fn resume_command_lists_sessions_and_enter_resumes_selection() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    seed_session(
        project.path(),
        home.path(),
        "alpha",
        "alpha user prompt",
        "alpha assistant answer",
    );

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Type a prompt below");

    tui.send("/resume\r");
    tui.wait_for("Sessions");
    tui.wait_for("alpha");
    tui.wait_for("Use Up/Down to choose, Enter to resume.");

    // Move down to select alpha (first item is the current in-memory session)
    tui.send("\x1b[B\r");
    tui.wait_for("alpha user prompt");
    tui.wait_for("alpha assistant answer");
    tui.wait_for("Resumed session");
}

#[test]
fn new_command_starts_a_fresh_session() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Type a prompt below");

    tui.send("/new\r");
    tui.wait_for("Started new session");
}

#[test]
fn clear_command_starts_a_fresh_session() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Type a prompt below");

    tui.send("/clear\r");
    tui.wait_for("Started new session");
}

#[test]
fn ctrl_d_must_be_pressed_twice_to_exit() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Type a prompt below");

    tui.send("\x04");
    tui.wait_for("Press Ctrl-D again to exit");
    assert!(
        tui.child.try_wait().unwrap().is_none(),
        "TUI exited after first Ctrl-D"
    );

    tui.send("\x04");
    tui.wait_for_exit();
}
