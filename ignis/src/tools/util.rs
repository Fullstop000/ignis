//! Shared helpers for the native tool implementations.

use std::time::Duration;

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
