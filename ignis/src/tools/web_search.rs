use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde_json::json;

pub struct WebSearchTool;

impl WebSearchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
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

        let tag_start = match html[..abs_marker].rfind("<a ") {
            Some(pos) => pos,
            None => {
                search_from = abs_marker + marker.len();
                continue;
            }
        };

        let tag_end = match html[abs_marker..].find('>') {
            Some(pos) => abs_marker + pos,
            None => break,
        };

        let tag = &html[tag_start..tag_end + 1];

        let href = match extract_attr(tag, "href") {
            Some(h) => h,
            None => {
                search_from = tag_end;
                continue;
            }
        };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_urlencoded() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("rust & cargo"), "rust+%26+cargo");
    }

    #[test]
    fn test_html_decode_and_strip() {
        assert_eq!(html_decode("A &amp; B"), "A & B");
        assert_eq!(strip_html_tags("<a href=\"url\">Title</a>"), "Title");
    }

    #[test]
    fn test_parse_duckduckgo_results() {
        let sample_html = r#"
            <div class="result">
                <a class="result__a" href="https://example.com/1">Example <b>Title 1</b></a>
            </div>
            <div class="result">
                <a class="result__a" href="https://example.com/2">Example Title 2</a>
            </div>
        "#;
        let parsed = parse_duckduckgo_results(sample_html);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "Example Title 1");
        assert_eq!(parsed[0].1, "https://example.com/1");
        assert_eq!(parsed[1].0, "Example Title 2");
        assert_eq!(parsed[1].1, "https://example.com/2");
    }

    #[tokio::test]
    async fn test_web_search_args_error() {
        let tool = WebSearchTool::new();
        let res = tool.call(json!({})).await;
        assert!(res.is_error);
        assert!(res.content.contains("Missing required parameter"));
    }
}
