use crate::{ExecutionMode, StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
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
impl StaticTool for CreateFileTool {
    const NAME: &'static str = "create_file";
    const DESCRIPTION: &'static str =
        "Create a new file with the given content. Creates parent directories if needed.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "path",
            ty: "string",
            description: "Path to the file to create",
        },
        ToolParam {
            name: "content",
            ty: "string",
            description: "Content to write to the file",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["path", "content"];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let path = args.require_str("path")?;
        let content = args.require_str("content")?;

        let resolved = crate::util::resolve_path(&self.cwd, path);
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create directories: {e}"))?;
        }
        tokio::fs::write(&resolved, content)
            .await
            .map_err(|e| format!("Failed to write file: {e}"))?;
        Ok(format!("Created file: {}", resolved.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTool;
    use serde_json::json;

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
