use crate::{ExecutionMode, StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

pub struct EditFileTool {
    cwd: PathBuf,
}

impl EditFileTool {
    pub const NAME: &'static str = "edit_file";

    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
impl StaticTool for EditFileTool {
    const NAME: &'static str = "edit_file";
    const DESCRIPTION: &'static str =
        "Edit a file by replacing occurrences of old_text with new_text. \
         By default only the first occurrence is replaced; set global_replace=true \
         to replace every occurrence.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "path",
            ty: "string",
            description: "Path to the file to edit",
        },
        ToolParam {
            name: "old_text",
            ty: "string",
            description: "The exact text to find and replace",
        },
        ToolParam {
            name: "new_text",
            ty: "string",
            description: "The replacement text",
        },
        ToolParam {
            name: "global_replace",
            ty: "boolean",
            description: "Replace all occurrences in the file (default: false)",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["path", "old_text", "new_text"];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let path = args.require_str("path")?;
        let old_text = args.require_str("old_text")?;
        let new_text = args.require_str("new_text")?;
        let global_replace = args
            .get("global_replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let resolved = crate::util::resolve_path(&self.cwd, path);
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| format!("Failed to read file: {e}"))?;

        if !content.contains(old_text) {
            return Err("old_text not found in file".to_string());
        }

        let new_content = if global_replace {
            content.replace(old_text, new_text)
        } else {
            content.replacen(old_text, new_text, 1)
        };
        tokio::fs::write(&resolved, &new_content)
            .await
            .map_err(|e| format!("Failed to write file: {e}"))?;
        Ok(render_edit_diff(old_text, new_text))
    }
}

/// Lines shown per side before truncating a large hunk.
const MAX_DIFF_LINES_PER_SIDE: usize = 25;

/// Render the replacement as a git-style hunk: removed lines prefixed `-`,
/// added lines prefixed `+`. The console colors these red/green.
fn render_edit_diff(old_text: &str, new_text: &str) -> String {
    let mut out = String::new();
    push_diff_side(&mut out, old_text, '-');
    push_diff_side(&mut out, new_text, '+');
    if out.is_empty() {
        out.push_str("(no changes)");
    }
    out
}

fn push_diff_side(out: &mut String, text: &str, sign: char) {
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(MAX_DIFF_LINES_PER_SIDE);
    for line in &lines[..shown] {
        out.push(sign);
        out.push(' ');
        out.push_str(line);
        out.push('\n');
    }
    if lines.len() > shown {
        out.push_str(&format!("{sign} … ({} more lines)\n", lines.len() - shown));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTool;
    use serde_json::json;

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
        // Output is a git-style diff of the replaced text.
        assert!(res.content.contains("- brown fox"), "got: {}", res.content);
        assert!(res.content.contains("+ red panda"), "got: {}", res.content);

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

    #[tokio::test]
    async fn test_edit_file_global_replace() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_edit_global.txt");
        tokio::fs::write(&file_path, "foo bar foo baz foo")
            .await
            .unwrap();

        let tool = EditFileTool::new(&temp_dir);
        let res = tool
            .call(json!({
                "path": "test_edit_global.txt",
                "old_text": "foo",
                "new_text": "qux",
                "global_replace": true
            }))
            .await;

        assert!(!res.is_error);
        let edited_content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(edited_content, "qux bar qux baz qux");

        let _ = tokio::fs::remove_file(&file_path).await;
    }
}
