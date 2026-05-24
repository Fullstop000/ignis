use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::time::Duration;

/// Cap on returned text so a large page can't blow up the context.
const MAX_OUTPUT_CHARS: usize = 20_000;

/// Fetch a URL and return its readable text. HTML is stripped to plain text.
/// Pairs with `web_search` (search finds URLs, fetch reads them).
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("ignis/0.2 (+https://github.com/Fullstop000/ignis)")
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

#[async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL over HTTP(S) and return its readable text (HTML is stripped \
         to plain text). Use after web_search, or for known doc/API URLs."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL to fetch" }
            },
            "required": ["url"]
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let Some(url) = args["url"].as_str() else {
            return ToolResult::error("Missing required parameter: url".to_string());
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolResult::error("url must start with http:// or https://".to_string());
        }

        let resp = match self.client.get(url).send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("Request failed: {e}")),
        };
        let status = resp.status();
        if !status.is_success() {
            return ToolResult::error(format!("HTTP {status} fetching {url}"));
        }
        let is_html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("html"))
            .unwrap_or(false);
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(format!("Failed to read body: {e}")),
        };

        let text = if is_html || looks_like_html(&body) {
            html_to_text(&body)
        } else {
            body
        };
        let text = text.trim();
        if text.is_empty() {
            return ToolResult::ok("(empty response)".to_string());
        }
        let out: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
        let suffix = if text.chars().count() > MAX_OUTPUT_CHARS {
            "\n… (truncated)"
        } else {
            ""
        };
        ToolResult::ok(format!("{out}{suffix}"))
    }
}

fn looks_like_html(s: &str) -> bool {
    let head = s.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

/// Minimal HTML → text: drop `<script>`/`<style>` blocks, strip tags, decode a
/// few common entities, and collapse blank runs. Good enough for an agent to
/// read; not a full renderer (kept dependency-free on purpose).
fn html_to_text(html: &str) -> String {
    let without_blocks = strip_blocks(html, "script");
    let without_blocks = strip_blocks(&without_blocks, "style");

    let mut out = String::with_capacity(without_blocks.len() / 2);
    let mut in_tag = false;
    for c in without_blocks.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }

    let out = decode_entities(&out);
    // Collapse 3+ newlines to 2, and trim trailing spaces on each line.
    let mut result = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                result.push('\n');
            }
        } else {
            blank_run = 0;
            result.push_str(trimmed);
            result.push('\n');
        }
    }
    result
}

/// Remove `<tag …> … </tag>` blocks (case-insensitive) for script/style.
fn strip_blocks(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(rel) = lower[i..].find(&open) {
            let start = i + rel;
            out.push_str(&s[i..start]);
            // Find the matching close tag after the open.
            if let Some(crel) = lower[start..].find(&close) {
                i = start + crel + close.len();
            } else {
                break; // unterminated; drop the rest
            }
        } else {
            out.push_str(&s[i..]);
            break;
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn html_to_text_strips_tags_scripts_and_entities() {
        let html = "<html><head><style>p{color:red}</style></head>\
                    <body><script>alert('x')</script><p>Hello &amp; welcome</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello & welcome"), "got: {text:?}");
        assert!(!text.contains("alert"), "script body must be dropped");
        assert!(!text.contains("color:red"), "style body must be dropped");
        assert!(!text.contains('<'), "tags must be stripped");
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http_url() {
        let tool = WebFetchTool::new();
        let res = tool.call(json!({ "url": "file:///etc/passwd" })).await;
        assert!(res.is_error);
    }

    #[tokio::test]
    async fn web_fetch_requires_url() {
        let tool = WebFetchTool::new();
        let res = tool.call(json!({})).await;
        assert!(res.is_error);
    }
}
