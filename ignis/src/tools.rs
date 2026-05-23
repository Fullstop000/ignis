use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::json;

use crate::{AgentTool, ExecutionMode, ToolResult};

// ==========================================
// ReadFileTool
// ==========================================

pub struct ReadFileTool {
    cwd: PathBuf,
}

impl ReadFileTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

#[async_trait]
impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports optional line offset and limit."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read" },
                "offset": { "type": "integer", "description": "Line offset to start reading from (0-based)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ToolResult::error("Missing required parameter: path".to_string()),
        };
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

        let resolved = self.resolve(path);
        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to read file: {e}")),
        };

        let lines: Vec<&str> = content.lines().skip(offset).take(limit).collect();
        let truncated = lines.len() == limit;
        let mut result = lines.join("\n");
        if truncated {
            result.push_str("\n... [truncated]");
        }
        ToolResult::ok(result)
    }
}

// ==========================================
// CreateFileTool
// ==========================================

pub struct CreateFileTool {
    cwd: PathBuf,
}

impl CreateFileTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

#[async_trait]
impl AgentTool for CreateFileTool {
    fn name(&self) -> &str {
        "create_file"
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

        let resolved = self.resolve(path);
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

// ==========================================
// ListDirTool
// ==========================================

pub struct ListDirTool {
    cwd: PathBuf,
}

impl ListDirTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
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
        let path = match args["path"].as_str() {
            Some(p) => p,
            None => return ToolResult::error("Missing required parameter: path".to_string()),
        };

        let resolved = self.resolve(path);
        let mut entries = match tokio::fs::read_dir(&resolved).await {
            Ok(e) => e,
            Err(e) => return ToolResult::error(format!("Failed to read directory: {e}")),
        };

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
        ToolResult::ok(lines.join("\n"))
    }
}

// ==========================================
// BashTool
// ==========================================

pub struct BashTool {
    cwd: PathBuf,
}

impl BashTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

const BASH_OUTPUT_LIMIT: usize = 50 * 1024;

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command via bash and return its output."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 60)" }
            },
            "required": ["command"]
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let command = match args["command"].as_str() {
            Some(c) => c,
            None => return ToolResult::error("Missing required parameter: command".to_string()),
        };
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);

        let child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to spawn command: {e}")),
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await;

        match result {
            Err(_) => ToolResult::error("Command timed out".to_string()),
            Ok(Err(e)) => ToolResult::error(format!("Command failed: {e}")),
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);

                let mut combined = String::new();
                if !stdout.is_empty() {
                    combined.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[stderr]\n");
                    combined.push_str(&stderr);
                }

                if combined.len() > BASH_OUTPUT_LIMIT {
                    combined.truncate(BASH_OUTPUT_LIMIT);
                    combined.push_str("\n... [truncated]");
                }

                if !output.status.success() {
                    combined.push_str(&format!("\n[exit code: {exit_code}]"));
                    ToolResult::error(combined)
                } else {
                    ToolResult::ok(combined)
                }
            }
        }
    }
}

// ==========================================
// EditFileTool
// ==========================================

pub struct EditFileTool {
    cwd: PathBuf,
}

impl EditFileTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
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

        let resolved = self.resolve(path);
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

// ==========================================
// WebSearchTool
// ==========================================

pub struct WebSearchTool;

impl WebSearchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using DuckDuckGo and return result titles and URLs."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query" }
            },
            "required": ["query"]
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let query = match args["query"].as_str() {
            Some(q) => q,
            None => return ToolResult::error("Missing required parameter: query".to_string()),
        };

        let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoded(query));
        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            )
            .send()
            .await;

        let response = match response {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("HTTP request failed: {e}")),
        };

        let body = match response.text().await {
            Ok(t) => t,
            Err(e) => return ToolResult::error(format!("Failed to read response: {e}")),
        };

        let results = parse_duckduckgo_results(&body);
        if results.is_empty() {
            return ToolResult::ok("No results found.".to_string());
        }

        let formatted: Vec<String> = results
            .iter()
            .enumerate()
            .map(|(i, (title, url))| format!("{}. {} - {}", i + 1, title, url))
            .collect();
        ToolResult::ok(formatted.join("\n"))
    }
}

fn urlencoded(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push('%');
                result.push_str(&format!("{byte:02X}"));
            }
        }
    }
    result
}

fn parse_duckduckgo_results(html: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    let marker = "class=\"result__a\"";

    let mut search_from = 0;
    while let Some(marker_pos) = html[search_from..].find(marker) {
        let abs_marker = search_from + marker_pos;

        // Find the href attribute before or after the class attribute within the same <a> tag
        // Look backwards for the opening <a
        let tag_start = match html[..abs_marker].rfind("<a ") {
            Some(pos) => pos,
            None => {
                search_from = abs_marker + marker.len();
                continue;
            }
        };

        // Find closing > of this tag
        let tag_end = match html[abs_marker..].find('>') {
            Some(pos) => abs_marker + pos,
            None => break,
        };

        let tag = &html[tag_start..tag_end + 1];

        // Extract href
        let href = match extract_attr(tag, "href") {
            Some(h) => h,
            None => {
                search_from = tag_end;
                continue;
            }
        };

        // Extract title: content between > and </a>
        let content_start = tag_end + 1;
        let title = match html[content_start..].find("</a>") {
            Some(pos) => {
                let raw = &html[content_start..content_start + pos];
                strip_html_tags(raw).trim().to_string()
            }
            None => {
                search_from = content_start;
                continue;
            }
        };

        if !title.is_empty() && !href.is_empty() {
            results.push((title, href));
        }

        search_from = content_start;
    }

    results
}

fn extract_attr(tag: &str, attr_name: &str) -> Option<String> {
    let pattern = format!("{attr_name}=\"");
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(html_decode(&tag[start..end]))
}

fn strip_html_tags(s: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

// ==========================================
// Registration
// ==========================================

pub fn register_native_tools(agent: &mut crate::Agent, cwd: &std::path::Path) {
    use std::sync::Arc;
    agent.register_tool(Arc::new(ReadFileTool::new(cwd)));
    agent.register_tool(Arc::new(CreateFileTool::new(cwd)));
    agent.register_tool(Arc::new(ListDirTool::new(cwd)));
    agent.register_tool(Arc::new(BashTool::new(cwd)));
    agent.register_tool(Arc::new(EditFileTool::new(cwd)));
    agent.register_tool(Arc::new(WebSearchTool::new()));
}
