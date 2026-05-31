use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

pub struct CreateFileTool {
    cwd: PathBuf,
}

impl CreateFileTool {
    pub const NAME: &'static str = "create_file";

    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
impl AgentTool for CreateFileTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Create a new file with the given content. Creates parent directories if needed."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to create" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ToolResult::error("Missing required parameter: path".to_string()),
        };
        let content = match args["content"].as_str() {
            Some(c) => c,
            None => return ToolResult::error("Missing required parameter: content".to_string()),
        };

        let resolved = crate::util::resolve_path(&self.cwd, path);
        if let Some(parent) = resolved.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::error(format!("Failed to create directories: {e}"));
            }
        }

        match tokio::fs::write(&resolved, content).await {
            Ok(()) => ToolResult::ok(format!("Created file: {}", resolved.display())),
            Err(e) => ToolResult::error(format!("Failed to write file: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_file() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("subdir/test_create.txt");
        let content = "Hello World from create_file test";

        let tool = CreateFileTool::new(&temp_dir);
        let res = tool
            .call(json!({
                "path": "subdir/test_create.txt",
                "content": content
            }))
            .await;

        assert!(!res.is_error);
        assert!(res.content.contains("Created file"));

        // Verify the file actually exists and matches the content
        assert!(file_path.exists());
        let read_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(read_content, content);

        let _ = tokio::fs::remove_file(&file_path).await;
        let _ = tokio::fs::remove_dir(temp_dir.join("subdir")).await;
    }
}
