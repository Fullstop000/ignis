use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde_json::json;
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

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command via bash and return its output."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 60)" }
            },
            "required": ["command"]
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let command = match args["command"].as_str() {
            Some(c) => c,
            None => return ToolResult::error("Missing required parameter: command".to_string()),
        };
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);

        let child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to spawn command: {e}")),
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await;

        match result {
            Err(_) => ToolResult::error("Command timed out".to_string()),
            Ok(Err(e)) => ToolResult::error(format!("Command failed: {e}")),
            Ok(Ok(output)) => {
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
                    combined.push_str("\n... [truncated]");
                }

                if !output.status.success() {
                    combined.push_str(&format!("\n[exit code: {exit_code}]"));
                    ToolResult::error(combined)
                } else {
                    ToolResult::ok(combined)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
