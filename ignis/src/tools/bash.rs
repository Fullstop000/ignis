use crate::{ExecutionMode, StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

pub struct BashTool {
    cwd: PathBuf,
}

impl BashTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
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
            description: "Timeout in seconds (default: 60)",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["command"];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let command = args.require_str("command")?;
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);

        match tokio::fs::metadata(&self.cwd).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("cwd '{}' is not a directory", self.cwd.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("cwd '{}' does not exist", self.cwd.display()));
            }
            Err(e) => return Err(format!("cwd '{}': {e}", self.cwd.display())),
        }

        let mut builder = tokio::process::Command::new("bash");
        builder
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
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
