use crate::{StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
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
}

#[async_trait]
impl StaticTool for ListDirTool {
    const NAME: &'static str = "list_dir";
    const DESCRIPTION: &'static str = "List directory contents showing file type and size.";
    const PARAMETERS: &'static [ToolParam] = &[ToolParam {
        name: "path",
        ty: "string",
        description: "Path to the directory to list",
    }];
    const REQUIRED: &'static [&'static str] = &["path"];

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
        // Explicit sentinel so the model can tell an empty directory from a
        // tool that returned nothing (a bare "").
        if lines.is_empty() {
            return Ok("(empty directory)".to_string());
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTool;
    use serde_json::json;

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

    #[tokio::test]
    async fn test_list_dir_empty_returns_sentinel() {
        let temp_dir = std::env::temp_dir();
        let empty_dir = temp_dir.join("test_list_dir_empty_subdir");
        let _ = tokio::fs::remove_dir_all(&empty_dir).await;
        tokio::fs::create_dir_all(&empty_dir).await.unwrap();

        let tool = ListDirTool::new(&temp_dir);
        let res = tool
            .call(json!({ "path": "test_list_dir_empty_subdir" }))
            .await;

        assert!(!res.is_error);
        assert_eq!(res.content, "(empty directory)");

        let _ = tokio::fs::remove_dir(empty_dir).await;
    }
}
