use crate::{StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
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
}

#[async_trait]
impl StaticTool for ReadFileTool {
    const NAME: &'static str = "read_file";
    const DESCRIPTION: &'static str =
        "Read the contents of a file. Supports optional line offset and limit.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "path",
            ty: "string",
            description: "Path to the file to read",
        },
        ToolParam {
            name: "offset",
            ty: "integer",
            description: "Line offset to start reading from (0-based)",
        },
        ToolParam {
            name: "limit",
            ty: "integer",
            description: "Maximum number of lines to read",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["path"];

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let path = args.require_str("path")?;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

        let resolved = crate::util::resolve_path(&self.cwd, path);
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| format!("Failed to read file: {e}"))?;

        // Peek one extra line so truncation is flagged only when content was
        // actually cut — a file with exactly `limit` lines remaining is
        // complete, not truncated (#179).
        let mut lines: Vec<&str> = content.lines().skip(offset).take(limit + 1).collect();
        let truncated = lines.len() > limit;
        if truncated {
            lines.truncate(limit);
        }

        // An offset past EOF yields nothing — say so instead of a blank result
        // that looks like an empty file.
        if lines.is_empty() && offset > 0 {
            let total = content.lines().count();
            return Ok(format!(
                "(offset {offset} is past end of file, {total} lines total)"
            ));
        }

        let mut result = lines.join("\n");
        if truncated {
            result.push_str("\n... [truncated]");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTool;
    use serde_json::json;

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

    #[tokio::test]
    async fn test_read_file_exactly_limit_lines_not_truncated() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_read_exact_limit.txt");
        // Exactly `limit` (3) lines remaining — nothing is cut.
        tokio::fs::write(&file_path, "a\nb\nc").await.unwrap();

        let tool = ReadFileTool::new(&temp_dir);
        let res = tool
            .call(json!({ "path": "test_read_exact_limit.txt", "limit": 3 }))
            .await;

        assert!(!res.is_error);
        assert_eq!(
            res.content, "a\nb\nc",
            "a complete file must not look truncated"
        );
        assert!(!res.content.contains("[truncated]"));

        let _ = tokio::fs::remove_file(file_path).await;
    }

    #[tokio::test]
    async fn test_read_file_offset_past_eof_hints() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_read_past_eof.txt");
        tokio::fs::write(&file_path, "a\nb\nc").await.unwrap();

        let tool = ReadFileTool::new(&temp_dir);
        let res = tool
            .call(json!({ "path": "test_read_past_eof.txt", "offset": 10 }))
            .await;

        assert!(!res.is_error);
        assert!(
            res.content.contains("past end of file"),
            "got: {}",
            res.content
        );

        let _ = tokio::fs::remove_file(file_path).await;
    }
}
