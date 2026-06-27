use crate::tools::background::{spawn_background, BackgroundCtx};
use crate::tools::cwd::SessionCwd;
use crate::{ExecutionMode, StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
use std::path::PathBuf;

/// Filesystem-sandbox config for auto-run bash (unattended modes only). Confines
/// the spawned command's WRITES to the project + temp (+ configured extras), and
/// — on Linux — its READS to the system roots + project + toolchain caches
/// (+ configured extras) so `$HOME` credential dirs stay private. `None` on
/// `BashTool` = no sandbox (Off mode unchanged).
#[derive(Debug, Clone, Default)]
pub struct BashSandbox {
    /// Extra writable dirs beyond cwd + temp, from `[permissions]
    /// sandbox_write_paths` (already `~`-expanded). Default empty.
    pub extra_writes: Vec<PathBuf>,
    /// Extra readable dirs beyond the system roots + cwd + toolchain caches,
    /// from `[permissions] sandbox_read_paths` (already `~`-expanded). Default
    /// empty. Lets non-Rust stacks add their home caches without re-exposing
    /// all of `$HOME`.
    pub extra_reads: Vec<PathBuf>,
}

/// FHS roots the Linux bash sandbox grants read access to. These hold system
/// binaries, libraries, and config that builds read constantly — never user
/// credentials, which live under `$HOME` (deliberately excluded). Missing roots
/// are skipped by Landlock, so the list can be generous.
#[cfg(target_os = "linux")]
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/opt", "/var", "/proc", "/sys", "/dev",
    "/run",
];

pub struct BashTool {
    cwd: SessionCwd,
    /// Background-execution context (registry + event channel). `None` for
    /// sub-agents and headless one-shot — `run_in_background` is rejected there.
    background: Option<BackgroundCtx>,
    /// When set (unattended modes), each spawned command is confined by a
    /// Landlock/Seatbelt write sandbox via `pre_exec`. `None` = no sandbox.
    sandbox: Option<BashSandbox>,
}

impl BashTool {
    pub fn new(cwd: impl Into<SessionCwd>) -> Self {
        Self {
            cwd: cwd.into(),
            background: None,
            sandbox: None,
        }
    }

    /// Enable `run_in_background` by wiring the shared shell registry. `None`
    /// leaves the tool foreground-only (sub-agents, one-shot CLI).
    pub fn with_background(mut self, background: Option<BackgroundCtx>) -> Self {
        self.background = background;
        self
    }

    /// Validate that `cwd` exists and is a directory. Shared by the foreground
    /// and background spawn paths.
    async fn check_cwd(&self) -> Result<(), String> {
        let cwd = self.cwd.get();
        match tokio::fs::metadata(&cwd).await {
            Ok(meta) if meta.is_dir() => Ok(()),
            Ok(_) => Err(format!("cwd '{}' is not a directory", cwd.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("cwd '{}' does not exist", cwd.display()))
            }
            Err(e) => Err(format!("cwd '{}': {e}", cwd.display())),
        }
    }

    /// Enable the write sandbox (unattended modes). `None` leaves bash
    /// unsandboxed (Off mode, sub-agents, one-shot).
    pub fn with_sandbox(mut self, sandbox: Option<BashSandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// The read/write sets for the sandbox.
    ///
    /// Writes: the project (cwd), the temp dirs, the `/dev/null` sink, `$TMPDIR`
    /// if set, plus any configured `sandbox_write_paths`.
    ///
    /// Reads: see [`Self::sandbox_reads`] — narrowed on Linux to keep `$HOME`
    /// secrets unreadable, broad elsewhere.
    fn sandbox_paths(&self, sb: &BashSandbox) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut writes = vec![
            self.cwd.get(),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/dev/null"),
        ];
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            writes.push(PathBuf::from(tmp));
        }
        writes.extend(sb.extra_writes.iter().cloned());
        let reads = self.sandbox_reads(sb, &writes);
        (reads, writes)
    }

    /// Read allowlist on Linux (Landlock). Reads were once `/` (the whole
    /// filesystem), which let an unattended command `cat ~/.ssh/id_ed25519`
    /// (and, with the network open, exfiltrate it). Landlock is allowlist-only
    /// — it can't subtract `~/.ssh` from `/` — so instead we grant the system
    /// roots broadly (no user secret lives there) plus the project, the temp
    /// dirs, and the toolchain caches under `$HOME` that builds genuinely need
    /// (`~/.cargo`, `~/.rustup`, honoring `CARGO_HOME`/`RUSTUP_HOME`). `$HOME`
    /// itself is NOT granted, so credential dirs (`~/.ssh`, `~/.aws`,
    /// `~/.gnupg`, `~/.ignis`, …) are unreadable. Stacks whose caches live
    /// elsewhere under `$HOME` add them via `sandbox_read_paths`. Every writable
    /// path is also made readable (writing to a dir you can't read is never what
    /// you want).
    #[cfg(target_os = "linux")]
    fn sandbox_reads(&self, sb: &BashSandbox, writes: &[PathBuf]) -> Vec<PathBuf> {
        let mut reads: Vec<PathBuf> = SYSTEM_READ_ROOTS.iter().map(PathBuf::from).collect();
        for (var, default) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
            if let Some(dir) = std::env::var_os(var) {
                reads.push(PathBuf::from(dir));
            } else if let Some(home) = dirs::home_dir() {
                reads.push(home.join(default));
            }
        }
        reads.extend(sb.extra_reads.iter().cloned());
        reads.extend(writes.iter().cloned());
        reads
    }

    /// On non-Linux (macOS Seatbelt) the read narrowing isn't implemented:
    /// getting the allowlist right there means resolving macOS's `/etc` →
    /// `/private/etc` symlinks and the dyld cache layout, which can't be tested
    /// from Linux CI. Reads stay broad — same as before this change, so no
    /// regression — and the write confinement still applies.
    #[cfg(not(target_os = "linux"))]
    fn sandbox_reads(&self, _sb: &BashSandbox, _writes: &[PathBuf]) -> Vec<PathBuf> {
        vec![PathBuf::from("/")]
    }
}

#[async_trait]
impl StaticTool for BashTool {
    const NAME: &'static str = "bash";
    const DESCRIPTION: &'static str = "Run a shell command via bash and return its output.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "command",
            ty: "string",
            description: "The shell command to execute",
        },
        ToolParam {
            name: "timeout_secs",
            ty: "integer",
            description: "Timeout in seconds (default: 60). Ignored when run_in_background is set.",
        },
        ToolParam {
            name: "run_in_background",
            ty: "boolean",
            description: "Run without blocking and return a shell id immediately, for \
                          long-running commands (dev servers, watchers). Read its output \
                          later with bash_output and stop it with kill_shell. Default false.",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["command"];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let command = args.require_str("command")?;
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);

        if args["run_in_background"].as_bool().unwrap_or(false) {
            return match &self.background {
                Some(ctx) => {
                    // Validate cwd up front (matches the foreground path below).
                    self.check_cwd().await?;
                    spawn_background(ctx, &self.cwd.get(), command).await
                }
                None => Err(
                    "Background execution is not available here (sub-agents and one-shot runs \
                     use blocking bash). Run the command in the foreground instead."
                        .to_string(),
                ),
            };
        }

        self.check_cwd().await?;

        let mut builder = tokio::process::Command::new("bash");
        builder
            .arg("-c")
            .arg(command)
            .current_dir(self.cwd.get())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Backstop: dropping the wait future on timeout SIGKILLs the bash
            // wrapper. The process group below is what actually reaps the
            // command's descendants (#176).
            .kill_on_drop(true);
        // Put the shell in its own process group (PGID == its PID) so a timeout
        // can SIGKILL the *whole group*. `kill_on_drop` alone only kills the
        // `bash -c` wrapper; a compound command (`a; b`, pipes, redirections)
        // forks children that bash does not `exec`, which would otherwise be
        // orphaned into the next Sequential bash call.
        #[cfg(unix)]
        builder.process_group(0);

        // Write-sandbox the command in unattended modes (reuses the hook
        // sandbox machinery). The policy is built in the parent (allocations
        // here); `apply` runs in the forked child before exec and is
        // allocation-free. Fail-open: on a kernel without Landlock the policy
        // reports `NotEnforced` (Ok) and the command runs unconfined — a hard
        // ruleset error fails the exec rather than running silently unconfined.
        #[cfg(unix)]
        if let Some(sb) = &self.sandbox {
            let (reads, writes) = self.sandbox_paths(sb);
            let policy = crate::sandbox::SandboxPolicy::new(&reads, &writes);
            // SAFETY: runs in the forked child before execve; async-signal-safe
            // (syscalls only, no alloc/locks) — see `SandboxPolicy::apply`.
            unsafe {
                builder.pre_exec(move || policy.apply().map(|_| ()));
            }
        }

        let child = builder
            .spawn()
            .map_err(|e| format!("Failed to spawn command: {e}"))?;
        #[cfg(unix)]
        let child_pid = child.id();

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        {
            Ok(result) => result.map_err(|e| format!("Command failed: {e}"))?,
            Err(_elapsed) => {
                // SIGKILL the whole process group to reap any descendants the
                // shell forked; the wrapper itself is already dying via
                // kill_on_drop (the wait future was just dropped).
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    // SAFETY: async-signal-safe; negative pid targets the group.
                    // ESRCH (already dead) is harmless.
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                return Err("Command timed out".to_string());
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        let mut combined = String::new();
        if !stdout.is_empty() {
            combined.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str("[stderr]\n");
            combined.push_str(&stderr);
        }

        if combined.len() > BASH_OUTPUT_LIMIT {
            truncate_on_char_boundary(&mut combined, BASH_OUTPUT_LIMIT);
            combined.push('\n');
            combined.push_str(crate::tools::util::TRUNCATION_MARKER);
        }

        if !output.status.success() {
            combined.push_str(&format!("\n[exit code: {exit_code}]"));
            // When the sandbox is active and the failure looks like a denied
            // write, point at the escape hatch — otherwise a confined
            // cargo/npm build fails with an opaque "permission denied".
            if self.sandbox.is_some()
                && (stderr.contains("Permission denied")
                    || stderr.contains("Read-only file system"))
            {
                combined.push_str(
                    "\n[note: unattended-mode bash sandbox confines writes to the project + temp; \
                     if a write outside those is legitimate, add its directory to \
                     `[permissions] sandbox_write_paths` in config.]",
                );
            }
            return Err(combined);
        }
        Ok(combined)
    }
}

const BASH_OUTPUT_LIMIT: usize = 50 * 1024;

/// Truncate `s` to at most `limit` bytes without splitting a UTF-8 character.
/// `String::truncate` panics if the byte index lands inside a multibyte char,
/// which happens on binary/CJK command output (e.g. `cat`-ing an ISO); back off
/// to the nearest char boundary first.
fn truncate_on_char_boundary(s: &mut String, limit: usize) {
    if s.len() <= limit {
        return;
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTool;
    use serde_json::json;

    // The read narrowing is Linux-only (see `BashTool::sandbox_reads`); on other
    // targets reads stay broad, so this assertion is platform-specific.
    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_paths_narrow_reads_and_confine_writes() {
        let cwd = PathBuf::from("/home/u/proj");
        let tool = BashTool::new(&cwd);
        let sb = BashSandbox {
            extra_writes: vec![PathBuf::from("/home/u/.cargo")],
            extra_reads: vec![PathBuf::from("/opt/toolcache")],
        };
        let (reads, writes) = tool.sandbox_paths(&sb);
        // Reads grant the system roots + the project, but NOT the whole tree —
        // so $HOME credential dirs (`~/.ssh`, …) stay unreadable.
        assert!(reads.contains(&PathBuf::from("/usr")));
        assert!(reads.contains(&PathBuf::from("/etc")));
        assert!(reads.contains(&cwd), "the project must be readable");
        assert!(
            reads.contains(&PathBuf::from("/opt/toolcache")),
            "configured extra read must be honored"
        );
        assert!(
            !reads.contains(&PathBuf::from("/")),
            "the whole-tree read grant must be gone"
        );
        assert!(
            !reads.contains(&PathBuf::from("/home/u")),
            "$HOME root must not be granted"
        );
        // Every writable path is also readable.
        assert!(reads.contains(&PathBuf::from("/tmp")));
        // Writes are confined to cwd + temp + the configured extra + /dev/null.
        assert!(writes.contains(&cwd));
        assert!(writes.contains(&PathBuf::from("/tmp")));
        assert!(writes.contains(&PathBuf::from("/home/u/.cargo")));
        assert!(writes.contains(&PathBuf::from("/dev/null")));
        // Home / root are NOT writable (no broad grant).
        assert!(!writes.contains(&PathBuf::from("/home/u")));
        assert!(!writes.contains(&PathBuf::from("/")));
    }

    #[tokio::test]
    async fn test_bash_success() {
        let temp_dir = std::env::temp_dir();
        let tool = BashTool::new(&temp_dir);
        let res = tool.call(json!({ "command": "echo 'hello bash'" })).await;

        assert!(!res.is_error);
        assert_eq!(res.content.trim(), "hello bash");
    }

    #[tokio::test]
    async fn test_bash_error() {
        let temp_dir = std::env::temp_dir();
        let tool = BashTool::new(&temp_dir);
        let res = tool
            .call(json!({ "command": "nonexistentcommand_abc_123" }))
            .await;

        assert!(res.is_error);
        assert!(res.content.contains("not found") || res.content.contains("exit code: 127"));
    }

    #[tokio::test]
    async fn test_bash_timeout() {
        let temp_dir = std::env::temp_dir();
        let tool = BashTool::new(&temp_dir);
        let res = tool
            .call(json!({ "command": "sleep 3", "timeout_secs": 1 }))
            .await;

        assert!(res.is_error);
        assert!(res.content.contains("timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_bash_timeout_kills_child() {
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("ignis-bash-kill-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let tool = BashTool::new(&dir);

        // `exec sleep` makes the spawned bash *become* the sleep process, so the
        // `$$` written before exec is exactly the PID tokio holds — the one
        // `kill_on_drop` must kill on timeout.
        let cmd = format!("echo $$ > '{}'; exec sleep 30", pidfile.display());
        let res = tool
            .call(json!({ "command": cmd, "timeout_secs": 1 }))
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("timed out"), "got: {}", res.content);

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Poll for the SIGKILL to land and the zombie to be reaped (each sleep
        // yields to tokio's orphan reaper). Before the fix the process lingered.
        let mut alive = true;
        for _ in 0..40 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !alive,
            "bash child pid {pid} survived the timeout (orphaned)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_bash_timeout_kills_forked_descendants() {
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("ignis-bash-killgrp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let tool = BashTool::new(&dir);

        // Compound command: bash does NOT exec — it forks `sleep` (whose PID is
        // recorded via `$!`) and stays alive in `wait`. `kill_on_drop` would
        // only kill the bash wrapper; the process-group SIGKILL must reap the
        // forked sleep too.
        let cmd = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let res = tool
            .call(json!({ "command": cmd, "timeout_secs": 1 }))
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("timed out"), "got: {}", res.content);

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..40 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !alive,
            "forked sleep pid {pid} survived the timeout (process group not killed)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_bash_rejects_missing_cwd() {
        let missing = std::env::temp_dir().join("ignis-bash-missing-cwd-xyz");
        let tool = BashTool::new(&missing);
        let res = tool.call(json!({ "command": "echo hi" })).await;

        assert!(res.is_error);
        assert!(
            res.content.contains("does not exist"),
            "got: {}",
            res.content
        );
    }

    #[test]
    fn truncate_on_char_boundary_never_splits_a_multibyte_char() {
        // 'é' (U+00E9) is 2 bytes; a 10-char string is 20 bytes. Truncating to
        // an odd byte index lands mid-char — `String::truncate(5)` would panic.
        let mut s = "é".repeat(10);
        truncate_on_char_boundary(&mut s, 5);
        assert!(s.is_char_boundary(s.len()));
        assert_eq!(s.len(), 4); // largest char boundary <= 5
                                // Shorter-than-limit strings are left untouched.
        let mut short = "abc".to_string();
        truncate_on_char_boundary(&mut short, 50 * 1024);
        assert_eq!(short, "abc");
    }
}
