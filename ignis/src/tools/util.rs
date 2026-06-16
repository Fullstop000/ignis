//! Shared helpers for the native tool implementations.

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
}
