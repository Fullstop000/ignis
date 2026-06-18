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

        let occurrences = content.matches(old_text).count();
        if occurrences == 0 {
            return Err("old_text not found in file".to_string());
        }

        let new_content = if global_replace {
            content.replace(old_text, new_text)
        } else if occurrences > 1 {
            // Refuse to silently edit the first of several matches — the wrong
            // site could change. Mirror Claude Code / aider: ask for a unique
            // anchor or an explicit global_replace (#177).
            return Err(format!(
                "old_text is not unique ({occurrences} occurrences); add more \
                 surrounding context to identify a single match, or set \
                 global_replace=true to replace every occurrence"
            ));
        } else {
            content.replacen(old_text, new_text, 1)
        };
        tokio::fs::write(&resolved, &new_content)
            .await
            .map_err(|e| format!("Failed to write file: {e}"))?;
        Ok(render_edit_diff(&content, &new_content))
    }
}

/// Render the edit as a git-style unified diff with `@@ -a,b +c,d @@` hunk
/// headers and 3 lines of context, computed against the **whole-file** before
/// and after states. The Ink frontend parses these hunks to render line numbers
/// and `⋮` separators between non-contiguous changes; the ratatui frontend
/// uses the `+`/`-` prefixes for its red/green coloring.
///
/// The leading `--- original` / `+++ modified` file headers that diffy emits
/// are stripped — the surrounding tool block already shows the path, and
/// keeping them just spends two scrollback rows on a redundant title.
fn render_edit_diff(old_content: &str, new_content: &str) -> String {
    if old_content == new_content {
        return "(no changes)".to_string();
    }
    let patch = diffy::create_patch(old_content, new_content);
    let body: String = patch
        .to_string()
        .lines()
        .filter(|l| !l.starts_with("--- ") && !l.starts_with("+++ "))
        .map(|l| format!("{l}\n"))
        .collect();
    if body.is_empty() {
        // create_patch produced no hunks — only possible if the inputs are
        // textually equal under diffy's eyes (shouldn't happen given the guard
        // above, but keep the same fallback so callers always get a body).
        return "(no changes)".to_string();
    }
    body
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
        // Output is a git-style unified diff of the file change.
        assert!(
            res.content.contains("@@ -1"),
            "expected unified-diff hunk header, got: {}",
            res.content
        );
        assert!(
            res.content
                .contains("-The quick brown fox jumps over the lazy dog"),
            "got: {}",
            res.content
        );
        assert!(
            res.content
                .contains("+The quick red panda jumps over the lazy dog"),
            "got: {}",
            res.content
        );

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
    async fn test_edit_file_non_unique_match_errors_and_leaves_file_untouched() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_edit_nonunique.txt");
        tokio::fs::write(&file_path, "foo bar foo").await.unwrap();

        let tool = EditFileTool::new(&temp_dir);
        let res = tool
            .call(json!({
                "path": "test_edit_nonunique.txt",
                "old_text": "foo",
                "new_text": "qux"
            }))
            .await;

        assert!(res.is_error);
        assert!(res.content.contains("not unique"), "got: {}", res.content);

        // No silent first-match edit — the file is unchanged.
        let after = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(after, "foo bar foo");

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

    /// Pin the new unified-diff format: changes far apart in a file produce
    /// **multiple** `@@`-prefixed hunks (one per region). The Ink frontend
    /// uses the gap between hunks to render a `⋮` separator; the ratatui
    /// frontend uses each hunk header as a section divider. The middle
    /// (unchanged) lines must not appear in the diff body.
    #[tokio::test]
    async fn diff_emits_multiple_hunks_for_non_contiguous_edits() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test_edit_multi_hunk.txt");
        // 16 lines, with `marker_a` near the top and `marker_b` near the bottom.
        // Three context lines on each side keep the two regions independent.
        let initial = (0..16)
            .map(|i| match i {
                2 => "marker_a".to_string(),
                12 => "marker_b".to_string(),
                _ => format!("line {i}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        tokio::fs::write(&file_path, &initial).await.unwrap();

        // Two separate edits via two tool calls — same as a model making two
        // edits to one file across a turn.
        let tool = EditFileTool::new(&temp_dir);
        let _ = tool
            .call(json!({
                "path": "test_edit_multi_hunk.txt",
                "old_text": "marker_a",
                "new_text": "MARKER_A",
            }))
            .await;
        let res = tool
            .call(json!({
                "path": "test_edit_multi_hunk.txt",
                "old_text": "marker_b",
                "new_text": "MARKER_B",
            }))
            .await;
        assert!(!res.is_error, "second edit failed: {}", res.content);

        // Now run a third call that changes both regions in one go to exercise
        // the multi-hunk path (one diff covers two non-contiguous regions).
        tokio::fs::write(&file_path, &initial).await.unwrap();
        let res = tool
            .call(json!({
                "path": "test_edit_multi_hunk.txt",
                "old_text": "marker_a",
                "new_text": "MARKER_A",
            }))
            .await;
        assert!(!res.is_error);
        let res = tool
            .call(json!({
                "path": "test_edit_multi_hunk.txt",
                "old_text": "marker_b",
                "new_text": "MARKER_B",
            }))
            .await;
        // The second call's diff is local to marker_b only — that's what
        // edit_file produces today (one tool call = one edit). The multi-hunk
        // case happens when one edit spans non-contiguous regions, e.g. via
        // global_replace across a long file. Drive that next.
        assert!(!res.is_error);
        assert!(res.content.contains("MARKER_B"), "got: {}", res.content);

        // global_replace across a wide file → one diff, multiple `@@` hunks.
        tokio::fs::write(&file_path, &initial).await.unwrap();
        let res = tool
            .call(json!({
                "path": "test_edit_multi_hunk.txt",
                "old_text": "marker",
                "new_text": "M",
                "global_replace": true,
            }))
            .await;
        assert!(!res.is_error, "got: {}", res.content);
        let hunks = res.content.matches("@@").count();
        assert!(
            hunks >= 4,
            "global_replace across 2 distant regions should emit 2 hunks (≥4 `@@` markers, two per header), got {hunks}: {}",
            res.content
        );
        // The far-apart context lines (`line 6`, `line 8`) must NOT appear —
        // they're outside both hunks' 3-line context windows.
        assert!(
            !res.content.contains("line 6") && !res.content.contains("line 8"),
            "non-changed mid-file lines must be outside any hunk: {}",
            res.content
        );

        let _ = tokio::fs::remove_file(&file_path).await;
    }
}
