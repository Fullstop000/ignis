use crate::{AgentTool, ExecutionMode, IntoToolResult, ToolArgs, ToolOutcome, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

pub struct ListDirTool {
    cwd: PathBuf,
}

impl ListDirTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let path = args.require_str("path")?;
        let resolved = crate::util::resolve_path(&self.cwd, path);
        let mut entries = tokio::fs::read_dir(&resolved)
            .await
            .map_err(|e| format!("Failed to read directory: {e}"))?;

        let mut lines = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            match entry.metadata().await {
                Ok(meta) => {
                    let kind = if meta.is_dir() { "dir" } else { "file" };
                    let size = meta.len();
                    lines.push(format!("{kind}\t{size}\t{name}"));
                }
                Err(_) => {
                    lines.push(format!("?\t?\t{name}"));
                }
            }
        }

        lines.sort();
        Ok(lines.join("\n"))
    }
}

#[async_trait]
impl AgentTool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List directory contents showing file type and size."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the directory to list" }
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
    async fn test_list_dir() {
        let temp_dir = std::env::temp_dir();
        let test_dir = temp_dir.join("test_list_dir_subdir");
        let _ = tokio::fs::create_dir_all(&test_dir).await;

        let file_path = test_dir.join("file1.txt");
        tokio::fs::write(&file_path, "hello").await.unwrap();

        let sub_subdir = test_dir.join("subdir");
        tokio::fs::create_dir(&sub_subdir).await.unwrap();

        let tool = ListDirTool::new(&temp_dir);
        let res = tool.call(json!({ "path": "test_list_dir_subdir" })).await;

        assert!(!res.is_error);
        assert!(res.content.contains("file\t5\tfile1.txt"));
        assert!(res.content.contains("dir\t"));
        assert!(res.content.contains("subdir"));

        let _ = tokio::fs::remove_file(file_path).await;
        let _ = tokio::fs::remove_dir(sub_subdir).await;
        let _ = tokio::fs::remove_dir(test_dir).await;
    }
}
