use crate::{AgentTool, ExecutionMode, IntoToolResult, ToolArgs, ToolOutcome, ToolResult};
use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Maximum matches returned before truncating, to keep results readable.
const MAX_MATCHES: usize = 200;
/// Per-line character cap so a single long line can't flood the output.
const MAX_LINE_CHARS: usize = 300;

/// Search file *contents* with a regex, gitignore-aware (skips `target/`,
/// `.git/`, etc.). The bread-and-butter code-navigation tool.
pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let pattern = args.require_str("pattern")?;
        let re = Regex::new(pattern).map_err(|e| format!("Invalid regex: {e}"))?;
        let base = match args["path"].as_str() {
            Some(p) => crate::util::resolve_path(&self.cwd, p),
            None => self.cwd.clone(),
        };
        let matcher = match args["glob"].as_str() {
            Some(g) => Some(
                globset::Glob::new(g)
                    .map_err(|e| format!("Invalid glob: {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let cwd = self.cwd.clone();
        let (mut lines, truncated) = tokio::task::spawn_blocking(move || {
            let mut out: Vec<String> = Vec::new();
            let mut truncated = false;
            'walk: for entry in WalkBuilder::new(&base).build().flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if let Some(m) = &matcher {
                    if !m.is_match(path) {
                        continue;
                    }
                }
                // Skip non-UTF-8 / binary files silently.
                let Ok(content) = std::fs::read_to_string(path) else {
                    continue;
                };
                let rel = path.strip_prefix(&cwd).unwrap_or(path);
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        let shown: String = line.trim_end().chars().take(MAX_LINE_CHARS).collect();
                        out.push(format!("{}:{}:{}", rel.display(), i + 1, shown));
                        if out.len() >= MAX_MATCHES {
                            truncated = true;
                            break 'walk;
                        }
                    }
                }
            }
            (out, truncated)
        })
        .await
        .map_err(|e| format!("grep failed: {e}"))?;

        if lines.is_empty() {
            return Ok("No matches.".to_string());
        }
        if truncated {
            lines.push(format!("… (truncated at {MAX_MATCHES} matches)"));
        }
        Ok(lines.join("\n"))
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression, respecting .gitignore. \
         Returns matching `path:line:text`. Prefer this over running grep via bash."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: project root)" },
                "glob": { "type": "string", "description": "Only search files whose name matches this glob, e.g. *.rs" }
            },
            "required": ["pattern"]
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
    async fn grep_finds_matches_with_glob_filter() {
        let dir = crate::util::unique_temp_dir("ignis-grep");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}\nlet x = TODO;\n").unwrap();
        std::fs::write(dir.join("b.txt"), "TODO in text\n").unwrap();

        let tool = GrepTool::new(&dir);
        let res = tool
            .call(json!({ "pattern": "TODO", "glob": "*.rs" }))
            .await;

        assert!(!res.is_error);
        assert!(res.content.contains("a.rs:2:"), "should match in a.rs");
        assert!(!res.content.contains("b.txt"), "glob should exclude b.txt");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn grep_reports_no_matches() {
        let dir = crate::util::unique_temp_dir("ignis-grep-empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "nothing here\n").unwrap();

        let tool = GrepTool::new(&dir);
        let res = tool.call(json!({ "pattern": "zzz" })).await;
        assert!(!res.is_error);
        assert_eq!(res.content, "No matches.");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn grep_rejects_bad_regex() {
        let tool = GrepTool::new(Path::new("."));
        let res = tool.call(json!({ "pattern": "(" })).await;
        assert!(res.is_error);
    }
}
