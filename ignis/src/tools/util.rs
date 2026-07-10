//! Shared helpers for the native tool implementations.

use futures_util::StreamExt;
use std::time::Duration;

/// The single marker every tool appends when it cuts its output, so the driving
/// model sees one consistent signal instead of `... [truncated]` (bash,
/// read_file) competing with `… (truncated)` (web_fetch, grep, glob).
pub(crate) const TRUNCATION_MARKER: &str = "... [truncated]";

/// Truncate `s` to at most `max` characters (not bytes), appending
/// [`TRUNCATION_MARKER`] on its own line when content was actually cut.
/// Char-based so a multibyte string is never split mid-codepoint.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\n{TRUNCATION_MARKER}")
}

/// Read the HTTP body with a hard byte cap, returning the prefix plus a
/// truncation marker if the server sent more than `limit` bytes. Uses a
/// streaming read so a huge response cannot OOM the process before truncation.
///
/// The returned `bool` is `true` when the body was larger than `limit` and was
/// cut; callers that need strict completeness (e.g. JSON parsers) can turn
/// this into their own error instead of attempting a parse.
pub(crate) async fn read_body_with_cap(
    resp: reqwest::Response,
    limit: usize,
) -> Result<(String, bool), String> {
    let mut bytes = Vec::with_capacity(limit.min(4096));
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Failed to read body: {e}"))?;
        if bytes.len() + chunk.len() > limit {
            let take = limit.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..take]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(truncate_bytes_with_marker(bytes, limit))
}

/// Apply the same truncation/marker logic that `read_body_with_cap` uses,
/// but synchronously on an already-collected byte buffer. Kept separate so
/// UTF-8 boundary handling can be unit-tested without constructing a live
/// HTTP response.
pub(crate) fn truncate_bytes_with_marker(bytes: Vec<u8>, limit: usize) -> (String, bool) {
    if bytes.len() <= limit {
        return (
            String::from_utf8(bytes).unwrap_or_else(|e| format!("Invalid UTF-8: {e}")),
            false,
        );
    }
    let mut trimmed = bytes;
    trimmed.truncate(limit);
    let mut end = trimmed.len();
    while end > 0 && std::str::from_utf8(&trimmed[..end]).is_err() {
        end -= 1;
    }
    trimmed.truncate(end);
    let mut text = String::from_utf8(trimmed).expect("truncated at a valid UTF-8 boundary");
    text.push('\n');
    text.push_str(TRUNCATION_MARKER);
    (text, true)
}

const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 5_000;

/// Send a request with retry/backoff for transient failures (timeouts,
/// connection errors, and 5xx responses). `RequestBuilder::try_clone` is used
/// to replay the body; if the body is not cloneable the request is still sent
/// once but is never retried.
pub(crate) async fn send_with_retry(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, String> {
    let mut request = Some(request);
    let mut attempt = 0u32;
    loop {
        let req = request.take().expect("request always set at loop start");
        // Save a clone for the next attempt before consuming `req` via `send`.
        let next = req.try_clone();
        attempt += 1;
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_server_error() && attempt <= MAX_RETRIES && next.is_some() {
                    request = next;
                    backoff(attempt).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt <= MAX_RETRIES
                    && next.is_some()
                    && (e.is_timeout()
                        || e.is_connect()
                        || e.status().is_none_or(|s| s.is_server_error()))
                {
                    request = next;
                    backoff(attempt).await;
                    continue;
                }
                return Err(format!("HTTP request failed: {e}"));
            }
        }
    }
}

async fn backoff(attempt: u32) {
    let ms = BASE_BACKOFF_MS
        .saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1)))
        .min(MAX_BACKOFF_MS);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_marks_only_when_cut_and_is_char_safe() {
        // Under the limit: returned unchanged, no marker.
        assert_eq!(truncate_chars("hello", 10), "hello");
        // Exactly the limit: still complete, no marker.
        assert_eq!(truncate_chars("hello", 5), "hello");
        // Over the limit: cut to `max` chars + the shared marker on its own line.
        assert_eq!(
            truncate_chars("hello world", 5),
            format!("hello\n{TRUNCATION_MARKER}")
        );

        // Char-based: a multibyte string is cut on a codepoint boundary, never
        // mid-byte (a byte slice at 5 would panic inside the 3-byte '中').
        let cjk = "中".repeat(10);
        let out = truncate_chars(&cjk, 4);
        assert!(out.ends_with(TRUNCATION_MARKER));
        assert_eq!(out.chars().filter(|&c| c == '中').count(), 4);
    }

    #[test]
    fn truncate_bytes_with_marker_under_limit_passes_through() {
        let (text, truncated) = truncate_bytes_with_marker(b"hello".to_vec(), 10);
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_bytes_with_marker_at_exact_limit_has_no_marker() {
        let (text, truncated) = truncate_bytes_with_marker(b"hello".to_vec(), 5);
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn truncate_bytes_with_marker_over_limit_appends_marker() {
        let (text, truncated) = truncate_bytes_with_marker(b"hello world".to_vec(), 5);
        assert!(truncated);
        assert!(text.starts_with("hello"));
        assert!(text.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn truncate_bytes_with_marker_respects_cjk_boundary() {
        // Each '中' is 3 bytes. A cap of 5 bytes lands inside the second CJK
        // character; the function must trim back to the last valid codepoint.
        let bytes = "中中".as_bytes().to_vec();
        let (text, truncated) = truncate_bytes_with_marker(bytes, 5);
        assert!(truncated);
        assert_eq!(text.chars().filter(|&c| c == '中').count(), 1);
        assert!(text.ends_with(TRUNCATION_MARKER));
    }
}
