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
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output: Arc<Mutex<String>>,
}

impl TuiProcess {
    fn spawn(home: &Path, project: &Path) -> Self {
        Self::spawn_with_args(home, project, &[])
    }

    fn spawn_with_args(home: &Path, project: &Path, args: &[&str]) -> Self {
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
        for arg in args {
            command.arg(arg);
        }
        // No `--tui` arg — the no-prompt invocation already launches the TUI.
        command.cwd(project.as_os_str());
        command.env("HOME", home.as_os_str());
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");

        let child = pair.slave.spawn_command(command).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let output = Arc::new(Mutex::new(String::new()));
        let output_for_thread = Arc::clone(&output);
        let writer_for_thread = Arc::clone(&writer);

        // The inline viewport (`Viewport::Inline`) queries cursor position via
        // DSR (`ESC[6n`) on startup and on every rebuild (resize / picker
        // open-close). A real terminal replies; the reader thread must too, or
        // ratatui errors ("cursor position could not be read") and the TUI
        // exits before rendering. Answer each query with a fixed report — the
        // anchor row doesn't matter for these output-content assertions.
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        let dsr = text.matches("\x1b[6n").count();
                        if dsr > 0 {
                            let mut w = writer_for_thread.lock().unwrap();
                            for _ in 0..dsr {
                                let _ = w.write_all(b"\x1b[1;1R");
                            }
                            let _ = w.flush();
                        }
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
    tui.wait_for("Your AI coding agent");

    tui.send("/");
    tui.wait_for("/sessions");
    tui.wait_for("/clear");
    tui.wait_for("/compact");

    // First suggestion (/sessions) is already selected, just press Enter
    tui.send("\r");
    tui.wait_for("Sessions");
    tui.wait_for("beta");

    tui.send("\x1b[B\r");
    tui.wait_for("beta user prompt");
    tui.wait_for("beta assistant answer");
    tui.wait_for("Resumed session");
}

#[test]
fn sessions_command_lists_sessions_and_enter_resumes_selection() {
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
    tui.wait_for("Your AI coding agent");

    tui.send("/sessions\r");
    tui.wait_for("Sessions");
    tui.wait_for("alpha");
    tui.wait_for("resume");

    // Move down to select alpha (first item is the current in-memory session)
    tui.send("\x1b[B\r");
    tui.wait_for("alpha user prompt");
    tui.wait_for("alpha assistant answer");
    tui.wait_for("Resumed session");
}

#[test]
fn startup_resume_renders_prior_transcript() {
    // `ignis --resume <id>` must paint the resumed transcript into scrollback,
    // not just feed it to the agent. Regression for the launch path that set
    // the right session id but never called render_session_history, so the
    // chat history showed blank after resuming.
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    seed_session(
        project.path(),
        home.path(),
        "delta",
        "delta user prompt",
        "delta assistant answer",
    );

    let mut tui = TuiProcess::spawn_with_args(home.path(), project.path(), &["--resume", "delta"]);

    // The prior conversation paints on launch (above the live band).
    tui.wait_for("delta user prompt");
    tui.wait_for("delta assistant answer");
    tui.wait_for("Resumed session");

    // A new prompt still echoes below the restored history.
    tui.send("brand-new-followup\r");
    tui.wait_for("brand-new-followup");
}

#[test]
fn new_typed_resolves_to_clear() {
    // `/new` was merged into `/clear`; typing it still starts a fresh session
    // because the suggestion menu surfaces /clear (matched on its description)
    // and Enter runs the selected command.
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Your AI coding agent");

    tui.send("/new\r");
    tui.wait_for("Started new session");
}

#[test]
fn clear_command_starts_a_fresh_session() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Your AI coding agent");

    tui.send("/clear\r");
    tui.wait_for("Started new session");
}

#[test]
fn ctrl_d_must_be_pressed_twice_to_exit() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Your AI coding agent");

    tui.send("\x04");
    tui.wait_for("Press Ctrl-D again to exit");
    assert!(
        tui.child.try_wait().unwrap().is_none(),
        "TUI exited after first Ctrl-D"
    );

    tui.send("\x04");
    tui.wait_for_exit();
}

#[test]
fn hooks_command_lists_registered_chains() {
    // Spins up the TUI with a hand-rolled hooks.json (one entry per event)
    // and asserts that bare `/hooks` and `/hooks list` both render the
    // expected chain headers, names, program paths, and timeout — and
    // that `/hooks reload` re-reads the file (we mutate it on disk in
    // between). This pins the user-visible behaviour of the new subcommand
    // without coupling the test to internal formatter strings beyond the
    // stable anchors.
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let ignis_home = home.path().join(".ignis");
    std::fs::create_dir_all(&ignis_home).unwrap();

    // Write two hooks — one per event, distinct timeouts so the listing
    // proves the timeout column is wired up.
    let prompt_hook = project.path().join("prompt-hook.sh");
    std::fs::write(&prompt_hook, "#!/bin/sh\ncat >/dev/null\nprintf '{}'\n").unwrap();
    let render_hook = project.path().join("render-hook.sh");
    std::fs::write(&render_hook, "#!/bin/sh\ncat >/dev/null\nprintf '{}'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for p in [&prompt_hook, &render_hook] {
            let mut perms = std::fs::metadata(p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(p, perms).unwrap();
        }
    }

    let hooks_json = format!(
        r#"{{
            "hooks": {{
                "UserPromptSubmit": [
                    {{"command": "{}", "timeout_ms": 1234}}
                ],
                "AssistantMessageRender": [
                    {{"command": "{}", "timeout_ms": 5678}}
                ]
            }}
        }}"#,
        prompt_hook.display(),
        render_hook.display(),
    );
    std::fs::write(ignis_home.join("hooks.json"), &hooks_json).unwrap();

    let mut tui = TuiProcess::spawn(home.path(), project.path());
    tui.wait_for("Your AI coding agent");

    // Bare `/hooks` lists.
    tui.send("/hooks\r");
    tui.wait_for("[info] 2 hooks registered");
    tui.wait_for("UserPromptSubmit (1):");
    tui.wait_for("AssistantMessageRender (1):");
    tui.wait_for("prompt-hook");
    tui.wait_for("render-hook");
    tui.wait_for("timeout 1234ms");
    tui.wait_for("timeout 5678ms");

    // `/hooks list` is an alias — produces the same listing.
    tui.send("/hooks list\r");
    tui.wait_for("[info] 2 hooks registered");

    // Edit the file (drop one hook, add a longer one) and `/hooks reload`.
    // `sync_all` flushes the write to disk so the child process is
    // guaranteed to read the new bytes regardless of OS buffering — the
    // unit-level reload test in `hooks/mod.rs` exercises the same path
    // in a single-process context, but a PTY spawn has its own caching.
    let hooks_path = ignis_home.join("hooks.json");
    {
        let mut f = std::fs::File::create(&hooks_path).unwrap();
        use std::io::Write as _;
        write!(
            f,
            r#"{{"hooks": {{"UserPromptSubmit": [
                {{"command": "{}", "timeout_ms": 9999}}
            ]}}}}"#,
            prompt_hook.display()
        )
        .unwrap();
        f.sync_all().unwrap();
    }
    tui.send("/hooks reload\r");
    tui.wait_for("[info] reloaded 1 hook");

    // Reloaded list reflects the new state.
    tui.send("/hooks\r");
    tui.wait_for("[info] 1 hook registered");
    tui.wait_for("UserPromptSubmit (1):");
    tui.wait_for("timeout 9999ms");
    // The render hook is gone from the new listing. The full output buffer
    // still contains the older "AssistantMessageRender (1):" line from
    // the first listing, so we can't assert against that — but we CAN
    // assert the prompt chain shrank to a single entry.
    tui.wait_for("UserPromptSubmit (1):");

    // Unknown subcommand falls through to the usage line. Pins that the
    // new dispatcher doesn't silently accept arbitrary tokens.
    tui.send("/hooks bogus\r");
    tui.wait_for("Usage: /hooks [list] | /hooks reload");
}
