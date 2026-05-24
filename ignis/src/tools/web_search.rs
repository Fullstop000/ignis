use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde_json::json;

const RESULT_COUNT: u32 = 5;

/// A normalized search hit, independent of which backend produced it.
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Switchable web search backends. Add a variant + a `search_*` method to
/// support a new provider.
enum Backend {
    Brave,
    Tavily,
}

impl Backend {
    fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "brave" => Some(Backend::Brave),
            "tavily" => Some(Backend::Tavily),
            _ => None,
        }
    }
}

pub struct WebSearchTool {
    provider: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new(provider: Option<String>, api_key: Option<String>) -> Self {
        Self {
            provider: provider.unwrap_or_else(|| "brave".to_string()),
            api_key,
            client: reqwest::Client::new(),
        }
    }

    async fn search_brave(&self, query: &str, key: &str) -> Result<Vec<SearchResult>, String> {
        let count = RESULT_COUNT.to_string();
        let resp = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[("q", query), ("count", count.as_str())])
            .header("Accept", "application/json")
            .header("X-Subscription-Token", key)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Brave API error {status}: {}", truncate(&body)));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {e}"))?;
        Ok(parse_brave(&json))
    }

    async fn search_tavily(&self, query: &str, key: &str) -> Result<Vec<SearchResult>, String> {
        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .json(&json!({
                "api_key": key,
                "query": query,
                "max_results": RESULT_COUNT,
            }))
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Tavily API error {status}: {}", truncate(&body)));
        }
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {e}"))?;
        Ok(parse_tavily(&json))
    }
}

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return result titles, URLs, and snippets."
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
            Some(q) if !q.trim().is_empty() => q,
            _ => return ToolResult::error("Missing required parameter: query".to_string()),
        };

        let backend = match Backend::from_name(&self.provider) {
            Some(b) => b,
            None => {
                return ToolResult::error(format!(
                    "Unknown web_search provider '{}' (supported: brave, tavily)",
                    self.provider
                ))
            }
        };

        let key = match &self.api_key {
            Some(k) if !k.is_empty() => k.as_str(),
            _ => {
                return ToolResult::error(format!(
                    "web_search provider '{}' requires an API key (set web_search.api_key in config.toml)",
                    self.provider
                ))
            }
        };

        let results = match backend {
            Backend::Brave => self.search_brave(query, key).await,
            Backend::Tavily => self.search_tavily(query, key).await,
        };

        match results {
            Err(e) => ToolResult::error(e),
            Ok(items) if items.is_empty() => ToolResult::ok("No results found.".to_string()),
            Ok(items) => {
                let formatted: Vec<String> = items
                    .iter()
                    .enumerate()
                    .map(|(i, r)| format!("{}. {} - {}\n   {}", i + 1, r.title, r.url, r.snippet))
                    .collect();
                ToolResult::ok(formatted.join("\n"))
            }
        }
    }
}

fn parse_brave(json: &serde_json::Value) -> Vec<SearchResult> {
    json["web"]["results"]
        .as_array()
        .map(|items| items.iter().map(extract_result("description")).collect())
        .unwrap_or_default()
}

fn parse_tavily(json: &serde_json::Value) -> Vec<SearchResult> {
    json["results"]
        .as_array()
        .map(|items| items.iter().map(extract_result("content")).collect())
        .unwrap_or_default()
}

/// Build an extractor that pulls title/url plus a snippet from the given field.
fn extract_result(snippet_field: &str) -> impl Fn(&serde_json::Value) -> SearchResult + '_ {
    move |item| SearchResult {
        title: item["title"].as_str().unwrap_or_default().to_string(),
        url: item["url"].as_str().unwrap_or_default().to_string(),
        snippet: item[snippet_field].as_str().unwrap_or_default().to_string(),
    }
}

/// Truncate an error body so a failed request doesn't flood the context.
fn truncate(body: &str) -> String {
    body.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_resolves_known_names_only() {
        assert!(Backend::from_name("brave").is_some());
        assert!(Backend::from_name("Tavily").is_some());
        assert!(Backend::from_name("google").is_none());
    }

    #[test]
    fn parse_brave_extracts_results() {
        let j = json!({"web":{"results":[
            {"title":"Rust","url":"https://rust-lang.org","description":"systems lang"},
            {"title":"Docs","url":"https://doc.rust-lang.org","description":"the docs"}
        ]}});
        let r = parse_brave(&j);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Rust");
        assert_eq!(r[0].url, "https://rust-lang.org");
        assert_eq!(r[1].snippet, "the docs");
    }

    #[test]
    fn parse_tavily_reads_content_field() {
        let j = json!({"results":[
            {"title":"Rust","url":"https://rust-lang.org","content":"systems lang"}
        ]});
        let r = parse_tavily(&j);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].snippet, "systems lang");
    }

    #[tokio::test]
    async fn missing_query_returns_error() {
        let tool = WebSearchTool::new(Some("brave".into()), Some("k".into()));
        let res = tool.call(json!({})).await;
        assert!(res.is_error);
        assert!(res.content.contains("Missing required parameter"));
    }

    #[tokio::test]
    async fn missing_api_key_returns_loud_error() {
        let tool = WebSearchTool::new(Some("brave".into()), None);
        let res = tool.call(json!({ "query": "rust" })).await;
        assert!(res.is_error);
        assert!(res.content.contains("API key"));
    }

    #[tokio::test]
    async fn unknown_provider_returns_error() {
        let tool = WebSearchTool::new(Some("google".into()), Some("k".into()));
        let res = tool.call(json!({ "query": "rust" })).await;
        assert!(res.is_error);
        assert!(res.content.contains("Unknown web_search provider"));
    }
}
