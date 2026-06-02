use crate::{AgentTool, ExecutionMode, IntoToolResult, ToolArgs, ToolOutcome, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

pub struct ReadFileTool {
    cwd: PathBuf,
}

impl ReadFileTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let path = args.require_str("path")?;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

        let resolved = crate::util::resolve_path(&self.cwd, path);
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| format!("Failed to read file: {e}"))?;

        let lines: Vec<&str> = content.lines().skip(offset).take(limit).collect();
        let truncated = lines.len() == limit;
        let mut result = lines.join("\n");
        if truncated {
            result.push_str("\n... [truncated]");
        }
        Ok(result)
    }
}

#[async_trait]
impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports optional line offset and limit."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read" },
                "offset": { "type": "integer", "description": "Line offset to start reading from (0-based)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        self.run(args).await.into_tool_result()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_file_basic() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_read_file_basic.txt");
        let content = "line 1\nline 2\nline 3";
        tokio::fs::write(&file_path, content).await.unwrap();

        let tool = ReadFileTool::new(&temp_dir);
        let res = tool
            .call(json!({ "path": "test_read_file_basic.txt" }))
            .await;

        assert!(!res.is_error);
        assert_eq!(res.content, content);

        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_read_file_offset_limit() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_read_file_offset_limit.txt");
        let content = "line 1\nline 2\nline 3\nline 4";
        tokio::fs::write(&file_path, content).await.unwrap();

        let tool = ReadFileTool::new(&temp_dir);
        let res = tool
            .call(json!({
                "path": "test_read_file_offset_limit.txt",
                "offset": 1,
                "limit": 2
            }))
            .await;

        assert!(!res.is_error);
        assert_eq!(res.content, "line 2\nline 3\n... [truncated]");

        let _ = tokio::fs::remove_file(file_path).await;
    }
}
