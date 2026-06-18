//! Background (non-blocking) shell execution.
//!
//! `bash` with `run_in_background: true` spawns a command, registers it in the
//! process-shared [`BackgroundShells`] registry, starts a reader task that
//! drains its output into a capped ring buffer, and returns immediately with a
//! short id. The model then polls with `bash_output` and stops it with
//! `kill_shell`. Poll-only: there is no agent-loop re-invocation.
//!
//! The registry is created once per run (in the console runner / engine driver),
//! shared (`Arc`) into the background-aware tools, and SIGKILLs every live shell
//! on shutdown so nothing is orphaned.

use crate::tools::tool::{ExecutionMode, StaticTool, ToolOutcome, ToolParam};
use crate::AgentEvent;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// Max bytes retained per background shell (matches foreground `bash`).
const BUFFER_CAP: usize = 50 * 1024;
/// Max simultaneously-live background shells. Over this, spawning errors.
const MAX_SHELLS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStatus {
    Running,
    Exited(i32),
    Killed,
}

impl ShellStatus {
    fn label(&self) -> String {
        match self {
            ShellStatus::Running => "status: running".to_string(),
            ShellStatus::Exited(code) => format!("exited: {code}"),
            ShellStatus::Killed => "killed".to_string(),
        }
    }
}

/// A capped output buffer with monotonic absolute positions, so a read cursor
/// stays valid even after the oldest bytes are dropped on overflow.
struct RingBuffer {
    data: VecDeque<u8>,
    /// Total bytes ever appended (monotonic). The bytes currently held are the
    /// absolute range `[total - data.len(), total)`.
    total: usize,
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            data: VecDeque::new(),
            total: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        self.data.extend(bytes.iter().copied());
        while self.data.len() > BUFFER_CAP {
            self.data.pop_front();
        }
    }

    /// Absolute position of the oldest retained byte.
    fn start(&self) -> usize {
        self.total - self.data.len()
    }

    /// Read everything after absolute `cursor`. Returns the text, the new cursor
    /// (= `total`), and whether any bytes below the cursor were dropped on
    /// overflow (so the caller can surface an "earlier output dropped" marker).
    fn read_from(&self, cursor: usize) -> (String, usize, bool) {
        let start = self.start();
        let dropped = cursor < start;
        let from = cursor.max(start);
        let bytes: Vec<u8> = self.data.iter().skip(from - start).copied().collect();
        (
            String::from_utf8_lossy(&bytes).into_owned(),
            self.total,
            dropped,
        )
    }
}

struct Shell {
    id: String,
    /// Process-group id (unix) for a group SIGKILL; `None` off-unix.
    pgid: Option<i32>,
    buffer: RingBuffer,
    /// Absolute position the model has already read up to.
    cursor: usize,
    status: ShellStatus,
    #[allow(dead_code)]
    command: String,
}

#[derive(Default)]
struct Inner {
    shells: Vec<Shell>,
    next_id: u64,
}

/// Process-shared registry of background shells.
pub struct BackgroundShells {
    inner: Mutex<Inner>,
}

impl Default for BackgroundShells {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundShells {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    fn running_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .shells
            .iter()
            .filter(|s| s.status == ShellStatus::Running)
            .count()
    }

    /// Emit the live-shell count to the frontend (footer indicator). No-op when
    /// no event channel is wired (e.g. tests).
    async fn emit_running(&self, events: &Option<mpsc::Sender<AgentEvent>>) {
        if let Some(tx) = events {
            let running = self.running_count();
            let _ = tx.send(AgentEvent::BackgroundShells { running }).await;
        }
    }

    /// Reserve a new shell id, or `Err` when at the concurrency cap. Holds a slot
    /// with `Running` status; the caller fills in the pgid once spawned.
    fn register(&self, command: String) -> Result<String, String> {
        let mut inner = self.inner.lock().unwrap();
        let live = inner
            .shells
            .iter()
            .filter(|s| s.status == ShellStatus::Running)
            .count();
        if live >= MAX_SHELLS {
            return Err(format!(
                "Too many background shells ({live}/{MAX_SHELLS} running). Read their output \
                 with bash_output or stop one with kill_shell before starting another."
            ));
        }
        inner.next_id += 1;
        let id = format!("bash_{}", inner.next_id);
        inner.shells.push(Shell {
            id: id.clone(),
            pgid: None,
            buffer: RingBuffer::new(),
            cursor: 0,
            status: ShellStatus::Running,
            command,
        });
        Ok(id)
    }

    fn set_pgid(&self, id: &str, pgid: Option<i32>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.shells.iter_mut().find(|s| s.id == id) {
            s.pgid = pgid;
        }
    }

    fn append_output(&self, id: &str, bytes: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.shells.iter_mut().find(|s| s.id == id) {
            s.buffer.append(bytes);
        }
    }

    /// Record a process exit, but only if the shell is still `Running` — a prior
    /// `kill_shell` set `Killed` and must not be clobbered by the reaped code.
    fn mark_exited(&self, id: &str, code: i32) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.shells.iter_mut().find(|s| s.id == id) {
            if s.status == ShellStatus::Running {
                s.status = ShellStatus::Exited(code);
            }
        }
    }

    /// Drain new output for `id`, advancing the cursor. Returns the rendered
    /// block (with a trailing status line) or `Err` for an unknown id.
    fn read(&self, id: &str, filter: Option<&regex::Regex>) -> Result<String, String> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.shells.iter().any(|s| s.id == id) {
            let known: Vec<&str> = inner.shells.iter().map(|s| s.id.as_str()).collect();
            return Err(format!(
                "Unknown shell id '{id}'. Known shells: {}.",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
        let s = inner.shells.iter_mut().find(|s| s.id == id).unwrap();
        let (text, new_cursor, dropped) = s.buffer.read_from(s.cursor);
        s.cursor = new_cursor;
        let status = s.status;

        let mut out = String::new();
        if dropped {
            out.push_str("[earlier output dropped]\n");
        }
        let body = match filter {
            Some(re) => text
                .lines()
                .filter(|l| re.is_match(l))
                .collect::<Vec<_>>()
                .join("\n"),
            None => text.trim_end_matches('\n').to_string(),
        };
        if body.is_empty() && !dropped {
            out.push_str("(no new output)");
        } else {
            out.push_str(&body);
        }
        out.push_str(&format!("\n[{}]", status.label()));
        Ok(out)
    }

    /// SIGKILL a shell's process group and mark it `Killed`. Already-exited
    /// shells report their code and are a no-op kill. Unknown id → `Err`.
    fn kill(&self, id: &str) -> Result<String, String> {
        let mut inner = self.inner.lock().unwrap();
        let Some(s) = inner.shells.iter_mut().find(|s| s.id == id) else {
            return Err(format!("Unknown shell id '{id}'."));
        };
        match s.status {
            ShellStatus::Exited(code) => Ok(format!("Shell {id} already exited (code {code}).")),
            ShellStatus::Killed => Ok(format!("Shell {id} was already killed.")),
            ShellStatus::Running => {
                #[cfg(unix)]
                if let Some(pgid) = s.pgid {
                    // SAFETY: async-signal-safe; negative pid targets the group.
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
                s.status = ShellStatus::Killed;
                Ok(format!("Killed shell {id}."))
            }
        }
    }

    /// SIGKILL every still-running shell. Called on run shutdown so no
    /// background process is orphaned.
    pub fn kill_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        for s in inner.shells.iter_mut() {
            if s.status == ShellStatus::Running {
                #[cfg(unix)]
                if let Some(pgid) = s.pgid {
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
                s.status = ShellStatus::Killed;
            }
        }
    }
}

/// What `BashTool` needs to run a command in the background: the shared registry
/// plus the (optional) event channel for the footer indicator. Absent on
/// sub-agents — they get plain blocking bash only.
#[derive(Clone)]
pub struct BackgroundCtx {
    pub shells: std::sync::Arc<BackgroundShells>,
    pub events: Option<mpsc::Sender<AgentEvent>>,
}

/// Spawn a command in the background and register it. Returns the ack string.
/// Called from `BashTool::run` when `run_in_background` is set.
pub async fn spawn_background(
    ctx: &BackgroundCtx,
    cwd: &std::path::Path,
    command: &str,
) -> ToolOutcome {
    let id = ctx.shells.register(command.to_string())?;

    let mut builder = tokio::process::Command::new("bash");
    builder
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Backstop: if the reader task is dropped (runtime teardown) before
        // `kill_all` runs, this SIGKILLs the `bash -c` wrapper. The process-group
        // kill in `kill`/`kill_all` is what reaps a compound command's forked
        // descendants; this just ensures the wrapper itself never lingers.
        .kill_on_drop(true);
    #[cfg(unix)]
    builder.process_group(0);

    let mut child = match builder.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Roll back the reserved slot so the cap isn't permanently consumed.
            ctx.shells.mark_exited(&id, -1);
            return Err(format!("Failed to spawn background command: {e}"));
        }
    };
    #[cfg(unix)]
    ctx.shells.set_pgid(&id, child.id().map(|p| p as i32));

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let shells = ctx.shells.clone();
    let events = ctx.events.clone();
    let reader_id = id.clone();
    tokio::spawn(async move {
        read_until_exit(child, stdout, stderr, &shells, &reader_id).await;
        shells.emit_running(&events).await;
    });

    ctx.shells.emit_running(&ctx.events).await;
    Ok(format!(
        "Started background shell `{id}`. Read its output with bash_output(\"{id}\"); \
         stop it with kill_shell(\"{id}\")."
    ))
}

/// Drain both pipes (chronological merge) into the shell's buffer until EOF,
/// then reap the child and record its exit code.
async fn read_until_exit(
    mut child: tokio::process::Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    shells: &BackgroundShells,
    id: &str,
) {
    let mut out = stdout;
    let mut err = stderr;
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    loop {
        // `if let` guards keep a closed pipe from busy-looping on Ok(0).
        tokio::select! {
            r = async { out.as_mut().unwrap().read(&mut out_buf).await }, if out.is_some() => {
                match r {
                    Ok(0) | Err(_) => out = None,
                    Ok(n) => shells.append_output(id, &out_buf[..n]),
                }
            }
            r = async { err.as_mut().unwrap().read(&mut err_buf).await }, if err.is_some() => {
                match r {
                    Ok(0) | Err(_) => err = None,
                    Ok(n) => shells.append_output(id, &err_buf[..n]),
                }
            }
            else => break,
        }
    }
    let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
    shells.mark_exited(id, code);
}

pub struct BashOutputTool {
    shells: std::sync::Arc<BackgroundShells>,
}

impl BashOutputTool {
    pub fn new(shells: std::sync::Arc<BackgroundShells>) -> Self {
        Self { shells }
    }
}

#[async_trait]
impl StaticTool for BashOutputTool {
    const NAME: &'static str = "bash_output";
    const DESCRIPTION: &'static str =
        "Read new output (since your last read) from a background shell started with \
         bash(run_in_background=true). Returns the new output plus a status line \
         (running / exited: <code> / killed). Poll this until the shell exits.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "shell_id",
            ty: "string",
            description: "The background shell id returned by bash (e.g. \"bash_1\").",
        },
        ToolParam {
            name: "filter",
            ty: "string",
            description: "Optional regex; only matching lines of the new output are returned.",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["shell_id"];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let id = args
            .get("shell_id")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: shell_id")?;
        let filter = match args.get("filter").and_then(|v| v.as_str()) {
            Some(pat) if !pat.is_empty() => {
                Some(regex::Regex::new(pat).map_err(|e| format!("Invalid filter regex: {e}"))?)
            }
            _ => None,
        };
        self.shells.read(id, filter.as_ref())
    }
}

pub struct KillShellTool {
    shells: std::sync::Arc<BackgroundShells>,
    events: Option<mpsc::Sender<AgentEvent>>,
}

impl KillShellTool {
    pub fn new(
        shells: std::sync::Arc<BackgroundShells>,
        events: Option<mpsc::Sender<AgentEvent>>,
    ) -> Self {
        Self { shells, events }
    }
}

#[async_trait]
impl StaticTool for KillShellTool {
    const NAME: &'static str = "kill_shell";
    const DESCRIPTION: &'static str =
        "Stop a background shell started with bash(run_in_background=true), by id.";
    const PARAMETERS: &'static [ToolParam] = &[ToolParam {
        name: "shell_id",
        ty: "string",
        description: "The background shell id to kill (e.g. \"bash_1\").",
    }];
    const REQUIRED: &'static [&'static str] = &["shell_id"];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let id = args
            .get("shell_id")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: shell_id")?;
        let result = self.shells.kill(id);
        self.shells.emit_running(&self.events).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_reads_incrementally_and_advances_cursor() {
        let mut b = RingBuffer::new();
        b.append(b"hello ");
        let (t1, c1, dropped1) = b.read_from(0);
        assert_eq!(t1, "hello ");
        assert!(!dropped1);
        b.append(b"world");
        let (t2, c2, dropped2) = b.read_from(c1);
        assert_eq!(t2, "world");
        assert!(!dropped2);
        assert_eq!(c2, 11);
        // Reading again from the end yields nothing.
        let (t3, _, _) = b.read_from(c2);
        assert_eq!(t3, "");
    }

    #[test]
    fn ring_buffer_overflow_drops_oldest_and_flags_it() {
        let mut b = RingBuffer::new();
        b.append(&vec![b'a'; BUFFER_CAP]);
        b.append(b"NEW"); // pushes 3 bytes out the front
        assert_eq!(b.total, BUFFER_CAP + 3);
        // A reader still at cursor 0 lost the dropped prefix.
        let (text, cursor, dropped) = b.read_from(0);
        assert!(dropped, "overflow below the cursor must be flagged");
        assert!(text.ends_with("NEW"));
        assert_eq!(text.len(), BUFFER_CAP);
        assert_eq!(cursor, BUFFER_CAP + 3);
    }

    #[tokio::test]
    async fn register_enforces_the_concurrency_cap() {
        let r = BackgroundShells::new();
        for _ in 0..MAX_SHELLS {
            r.register("x".into()).unwrap();
        }
        let err = r.register("over".into()).unwrap_err();
        assert!(err.contains("Too many background shells"));
        assert_eq!(r.running_count(), MAX_SHELLS);
    }

    #[tokio::test]
    async fn read_unknown_id_errors() {
        let r = BackgroundShells::new();
        assert!(r
            .read("bash_99", None)
            .unwrap_err()
            .contains("Unknown shell"));
        assert!(r.kill("bash_99").unwrap_err().contains("Unknown shell"));
    }

    #[tokio::test]
    async fn kill_does_not_clobber_a_recorded_exit() {
        let r = BackgroundShells::new();
        let id = r.register("x".into()).unwrap();
        r.mark_exited(&id, 0);
        let msg = r.kill(&id).unwrap();
        assert!(msg.contains("already exited"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_command_runs_polls_and_exits() {
        use std::sync::Arc;
        let ctx = BackgroundCtx {
            shells: Arc::new(BackgroundShells::new()),
            events: None,
        };
        let tmp = std::env::temp_dir();
        let ack = spawn_background(&ctx, &tmp, "echo first; sleep 0.3; echo second")
            .await
            .unwrap();
        assert!(ack.contains("bash_1"));

        // Poll until exited (bounded).
        let mut saw_first = false;
        let mut exited = false;
        for _ in 0..50 {
            let out = ctx.shells.read("bash_1", None).unwrap();
            if out.contains("first") {
                saw_first = true;
            }
            if out.contains("[exited: 0]") {
                exited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(saw_first, "should have read 'first'");
        assert!(exited, "should report exit");
        assert_eq!(ctx.shells.running_count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_shell_terminates_a_running_background_process() {
        use std::sync::Arc;
        let ctx = BackgroundCtx {
            shells: Arc::new(BackgroundShells::new()),
            events: None,
        };
        let tmp = std::env::temp_dir();
        spawn_background(&ctx, &tmp, "sleep 30").await.unwrap();
        // Give it a moment to register its pgid.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let msg = ctx.shells.kill("bash_1").unwrap();
        assert!(msg.contains("Killed"));
        assert_eq!(ctx.shells.running_count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_all_terminates_every_running_shell() {
        use std::sync::Arc;
        let ctx = BackgroundCtx {
            shells: Arc::new(BackgroundShells::new()),
            events: None,
        };
        let tmp = std::env::temp_dir();
        spawn_background(&ctx, &tmp, "sleep 30").await.unwrap();
        spawn_background(&ctx, &tmp, "sleep 30").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(ctx.shells.running_count(), 2);
        ctx.shells.kill_all();
        assert_eq!(ctx.shells.running_count(), 0, "kill_all marks all killed");
    }

    #[tokio::test]
    async fn bash_output_filter_narrows_lines() {
        let r = BackgroundShells::new();
        let id = r.register("x".into()).unwrap();
        r.append_output(&id, b"keep me\ndrop this\nkeep me too\n");
        let re = regex::Regex::new("keep").unwrap();
        let out = r.read(&id, Some(&re)).unwrap();
        assert!(out.contains("keep me"));
        assert!(out.contains("keep me too"));
        assert!(!out.contains("drop this"));
    }
}
