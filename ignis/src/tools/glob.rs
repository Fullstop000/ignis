use crate::tools::cwd::SessionCwd;
use crate::{StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
use globset::Glob;
use ignore::WalkBuilder;

/// Cap on returned paths so a broad pattern can't flood the output.
const MAX_PATHS: usize = 300;

/// Find files by name/path glob (e.g. `**/*.rs`), gitignore-aware. Returns
/// matching paths relative to the project root.
pub struct GlobTool {
    cwd: SessionCwd,
}

impl GlobTool {
    pub fn new(cwd: impl Into<SessionCwd>) -> Self {
        Self { cwd: cwd.into() }
    }
}

#[async_trait]
impl StaticTool for GlobTool {
    const NAME: &'static str = "glob";
    const DESCRIPTION: &'static str =
        "Find files whose path matches a glob (e.g. `**/*.rs`, `src/**/mod.rs`), \
         respecting .gitignore. Returns matching paths. Prefer this over `find` via bash.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "pattern",
            ty: "string",
            description: "Glob pattern, matched against the path relative to the search root",
        },
        ToolParam {
            name: "path",
            ty: "string",
            description: "Directory to search under (default: project root)",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["pattern"];

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let pattern = args.require_str("pattern")?;
        let matcher = Glob::new(pattern)
            .map_err(|e| format!("Invalid glob: {e}"))?
            .compile_matcher();
        let base = match args["path"].as_str() {
            Some(p) => crate::util::resolve_path(&self.cwd.get(), p),
            None => self.cwd.get(),
        };

        let cwd = self.cwd.get();
        let (mut paths, truncated) = tokio::task::spawn_blocking(move || {
            let mut paths: Vec<String> = Vec::new();
            let mut truncated = false;
            for entry in WalkBuilder::new(&base).build().flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                // Match the glob against the path relative to the search base.
                let rel_to_base = path.strip_prefix(&base).unwrap_or(path);
                if matcher.is_match(rel_to_base) {
                    let rel = path.strip_prefix(&cwd).unwrap_or(path);
                    paths.push(rel.display().to_string());
                    if paths.len() >= MAX_PATHS {
                        truncated = true;
                        break;
                    }
                }
            }
            paths.sort();
            (paths, truncated)
        })
        .await
        .map_err(|e| format!("glob failed: {e}"))?;

        if paths.is_empty() {
            return Ok("No files matched.".to_string());
        }
        if truncated {
            paths.push(format!("... [truncated at {MAX_PATHS} files]"));
        }
        Ok(paths.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTool;
    use serde_json::json;

    #[tokio::test]
    async fn glob_matches_nested_files() {
        let dir = crate::util::unique_temp_dir("ignis-glob");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("notes.md"), "").unwrap();

        let tool = GlobTool::new(&dir);
        let res = tool.call(json!({ "pattern": "**/*.rs" })).await;

        assert!(!res.is_error);
        assert!(res.content.contains("src/main.rs"));
        assert!(res.content.contains("src/lib.rs"));
        assert!(!res.content.contains("notes.md"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn glob_reports_no_match() {
        let dir = crate::util::unique_temp_dir("ignis-glob-empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();

        let tool = GlobTool::new(&dir);
        let res = tool.call(json!({ "pattern": "*.rs" })).await;
        assert!(!res.is_error);
        assert_eq!(res.content, "No files matched.");

        std::fs::remove_dir_all(&dir).ok();
    }
}
