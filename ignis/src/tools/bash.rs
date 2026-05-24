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
                    combined.truncate(BASH_OUTPUT_LIMIT);
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
}
