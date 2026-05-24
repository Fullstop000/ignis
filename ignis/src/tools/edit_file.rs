use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

pub struct EditFileTool {
    cwd: PathBuf,
}

impl EditFileTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

}

#[async_trait]
impl AgentTool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing the first occurrence of old_text with new_text."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit" },
                "old_text": { "type": "string", "description": "The exact text to find and replace" },
                "new_text": { "type": "string", "description": "The replacement text" }
            },
            "required": ["path", "old_text", "new_text"]
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
        let old_text = match args["old_text"].as_str() {
            Some(t) => t,
            None => return ToolResult::error("Missing required parameter: old_text".to_string()),
        };
        let new_text = match args["new_text"].as_str() {
            Some(t) => t,
            None => return ToolResult::error("Missing required parameter: new_text".to_string()),
        };

        let resolved = crate::util::resolve_path(&self.cwd, path);
        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to read file: {e}")),
        };

        if !content.contains(old_text) {
            return ToolResult::error("old_text not found in file".to_string());
        }

        let new_content = content.replacen(old_text, new_text, 1);
        match tokio::fs::write(&resolved, &new_content).await {
            Ok(()) => ToolResult::ok(format!("Edited file: {}", resolved.display())),
            Err(e) => ToolResult::error(format!("Failed to write file: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_edit_file_success() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_edit.txt");
        let initial_content = "The quick brown fox jumps over the lazy dog";
        tokio::fs::write(&file_path, initial_content).await.unwrap();

        let tool = EditFileTool::new(&temp_dir);
        let res = tool
            .call(json!({
                "path": "test_edit.txt",
                "old_text": "brown fox",
                "new_text": "red panda"
            }))
            .await;

        assert!(!res.is_error);
        assert!(res.content.contains("Edited file"));

        let edited_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(
            edited_content,
            "The quick red panda jumps over the lazy dog"
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }

    #[tokio::test]
    async fn test_edit_file_not_found() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_edit_err.txt");
        tokio::fs::write(&file_path, "simple text").await.unwrap();

        let tool = EditFileTool::new(&temp_dir);
        let res = tool
            .call(json!({
                "path": "test_edit_err.txt",
                "old_text": "nonexistent",
                "new_text": "replaced"
            }))
            .await;

        assert!(res.is_error);
        assert!(res.content.contains("old_text not found"));

        let _ = tokio::fs::remove_file(&file_path).await;
    }
}
