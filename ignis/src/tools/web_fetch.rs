use crate::{StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
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
impl StaticTool for WebFetchTool {
    const NAME: &'static str = "web_fetch";
    const DESCRIPTION: &'static str =
        "Fetch a URL over HTTP(S) and return its readable text (HTML is stripped \
         to plain text). Use after web_search, or for known doc/API URLs.";
    const PARAMETERS: &'static [ToolParam] = &[ToolParam {
        name: "url",
        ty: "string",
        description: "Absolute http(s) URL to fetch",
    }];
    const REQUIRED: &'static [&'static str] = &["url"];

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let url = args.require_str("url")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("url must start with http:// or https://".to_string());
        }

        let resp = crate::tools::util::send_with_retry(self.client.get(url))
            .await
            .map_err(|e| format!("Request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status} fetching {url}"));
        }
        let is_html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("html"))
            .unwrap_or(false);
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read body: {e}"))?;

        let text = if is_html || looks_like_html(&body) {
            html_to_text(&body)
        } else {
            body
        };
        let text = text.trim();
        if text.is_empty() {
            return Ok("(empty response)".to_string());
        }
        let out: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
        let suffix = if text.chars().count() > MAX_OUTPUT_CHARS {
            "\n… (truncated)"
        } else {
            ""
        };
        Ok(format!("{out}{suffix}"))
    }
}

fn looks_like_html(s: &str) -> bool {
    let head = s.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

/// Minimal HTML → text: drop HTML comments, `<script>`/`<style>` blocks, strip
/// tags, decode a few common entities, and collapse blank runs. Good enough for
/// an agent to read; not a full renderer (kept dependency-free on purpose).
fn html_to_text(html: &str) -> String {
    let without_comments = strip_comments(html);
    let without_blocks = strip_blocks(&without_comments, "script");
    let without_blocks = strip_blocks(&without_blocks, "style");

    let mut out = String::with_capacity(without_blocks.len() / 2);
    let mut in_tag = false;
    let mut quote = None::<char>;
    for c in without_blocks.chars() {
        if in_tag {
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (None, '"') | (None, '\'') => quote = Some(c),
                (None, '>') => in_tag = false,
                _ => {}
            }
        } else {
            match c {
                '<' => {
                    in_tag = true;
                    quote = None;
                }
                _ => out.push(c),
            }
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

/// Remove `<!-- … -->` comments, including any `<`/`>` inside them.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '<' && s[i..].starts_with("<!--") {
            if let Some(end) = s[i + 4..].find("-->") {
                let after = i + 4 + end + 3;
                // Advance the iterator to the character after `-->`.
                while chars.peek().is_some_and(|(idx, _)| *idx < after) {
                    chars.next();
                }
                continue;
            }
            break; // unterminated comment; drop the rest
        }
        out.push(c);
    }
    out
}

/// Remove `<tag …> … </tag>` blocks (case-insensitive) for script/style. The
/// open-tag scanner is quote-aware so `>` characters inside attributes don't
/// end the tag early. The body scanner is quote-aware so `</script>` inside a
/// JS string is not mistaken for the real close tag. If quote state reaches
/// EOF, we fall back to a literal search so an unmatched quote (e.g. an
/// apostrophe in a comment) doesn't swallow the rest of the page.
fn strip_blocks(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let Some(rel) = lower[i..].find(&open) else {
            out.push_str(&s[i..]);
            break;
        };
        let start = i + rel;
        out.push_str(&s[i..start]);
        // Scan past the open tag (quote-aware) then find the matching close tag.
        let mut j = start + open.len();
        let mut in_tag = true;
        let mut quote = None::<char>;
        while j < s.len() && in_tag {
            let c = s[j..].chars().next().unwrap();
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (None, '"') | (None, '\'') => quote = Some(c),
                (None, '>') => in_tag = false,
                _ => {}
            }
            j += c.len_utf8();
        }

        // Find the close tag, respecting quote state so `'</script>'` inside a
        // JS string is not treated as the end of the block.
        let body_start = j;
        let mut quote = None::<char>;
        let mut close_off = None;
        while j < s.len() {
            let c = s[j..].chars().next().unwrap();
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (None, '"') | (None, '\'') => quote = Some(c),
                (None, '<') if lower[j..].starts_with(&close) => {
                    close_off = Some(j - body_start);
                    break;
                }
                _ => {}
            }
            j += c.len_utf8();
        }

        // If we hit EOF while still inside a quote, an unmatched quote (common
        // in comments or contractions like "don't") prevented finding the real
        // close tag. Fall back to a literal search from just after the quote.
        let off = match (close_off, quote) {
            (Some(off), _) => off,
            (None, Some(_)) => lower[body_start..].find(&close).unwrap_or(usize::MAX),
            (None, None) => usize::MAX,
        };

        if off == usize::MAX {
            break; // unterminated; drop the rest
        }
        i = body_start + off + close.len();
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
    use crate::AgentTool;
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

    #[test]
    fn html_to_text_handles_malformed_and_nested_tags() {
        // `>` inside a quoted attribute should not end the tag early.
        let html = r#"<p data-cond="a > b">kept</p>"#;
        assert_eq!(html_to_text(html).trim(), "kept");

        // `</script>` inside a JS string should not end the block early.
        let html = "<script>var s='</script>'; alert(1)</script><p>after</p>";
        let text = html_to_text(html);
        assert!(!text.contains("alert"), "got: {text:?}");
        assert!(text.contains("after"), "got: {text:?}");

        // Nested tags inside a script block are dropped as one block.
        let html = "<script><div>nested</div></script><p>safe</p>";
        let text = html_to_text(html);
        assert!(!text.contains("nested"), "got: {text:?}");
        assert!(text.contains("safe"), "got: {text:?}");

        // Unterminated script drops to EOF.
        let html = "<script>never ends<p>not seen</p>";
        assert_eq!(html_to_text(html).trim(), "");

        // Comments containing `<`/`>` are removed entirely.
        let html = "<!-- <weird> -->text";
        assert_eq!(html_to_text(html).trim(), "text");

        // Quotes inside script/style bodies must not prevent the close tag from
        // ending the block.
        let html = "<script>// don't</script><p>after</p>";
        assert_eq!(html_to_text(html).trim(), "after");
        let html = "<style>body::before { content: \"x\"; }</style><p>after</p>";
        assert_eq!(html_to_text(html).trim(), "after");
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
