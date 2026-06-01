//! Streamable HTTP transport for MCP (spec 2025-03-26 / 2025-06-18). Pairs
//! with the stdio path in [`super::connect_one`]; the rest of the registry,
//! tool wrapper, and shutdown plumbing is transport-agnostic.
//!
//! No legacy SSE (the 2024-11-05 two-endpoint variant) and no OAuth in v2 —
//! see `docs/superpowers/specs/2026-05-31-mcp-http-transport-design.md` §13.
use std::collections::HashMap;

// `http` v1 — the same lineage rmcp's HTTP transport expects. NOT reqwest's
// re-export (the workspace also pulls in http v0.2 via reqwest 0.11; type
// mismatch is a fun half-hour debug if you mix them).
use http::header::{HeaderName, HeaderValue};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;

use crate::config::McpServerConfig;

/// Build the rmcp transport and run `initialize`. The outer
/// [`super::connect_one`] applies the startup timeout and post-init plumbing
/// (peer_info, list_all_tools, McpServer construction).
pub(super) async fn connect_streamable_http(
    cfg: &McpServerConfig,
) -> Result<RunningService<RoleClient, ()>, String> {
    let url = cfg
        .url
        .as_deref()
        .expect("validated: McpServerConfig.url is set on the HTTP path");

    let bearer = resolve_bearer(cfg.bearer_token_env_var.as_deref())?;
    let custom_headers = build_headers(&cfg.headers)?;

    let mut http_cfg = StreamableHttpClientTransportConfig::with_uri(url);
    // rmcp's reqwest backend calls `reqwest::RequestBuilder::bearer_auth(...)`
    // which prepends `Bearer ` itself; pass the raw token, NOT `Bearer <tok>`.
    http_cfg.auth_header = bearer;
    http_cfg.custom_headers = custom_headers;
    // Defaults we intentionally keep:
    //   allow_stateless = false → server MUST issue Mcp-Session-Id
    //   reinit_on_expired_session = true → rmcp re-runs `initialize` on session
    //     expiry transparently (the agent loop never sees the blip)

    let transport = StreamableHttpClientTransport::from_config(http_cfg);
    ().serve(transport)
        .await
        .map_err(|e| format!("initialize failed: {e}"))
}

/// Look up the env var named by `bearer_token_env_var`. None when the field
/// is unset; Err when the field is set but the env var is missing.
fn resolve_bearer(env_var: Option<&str>) -> Result<Option<String>, String> {
    let Some(name) = env_var else {
        return Ok(None);
    };
    match std::env::var(name) {
        Ok(val) if val.is_empty() => Err(format!("env var {name} is empty (bearer_token_env_var)")),
        Ok(val) => Ok(Some(val)),
        Err(_) => Err(format!("env var {name} not set (bearer_token_env_var)")),
    }
}

/// Convert the literal `headers` map into rmcp's expected
/// `HashMap<HeaderName, HeaderValue>`. Rejects invalid header names/values at
/// this layer; rmcp itself rejects reserved names (Mcp-Session-Id, Accept,
/// Mcp-Protocol-Version, etc.) at connect time.
fn build_headers(
    headers: &HashMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, String> {
    let mut out = HashMap::with_capacity(headers.len());
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| format!("invalid header name `{k}`: {e}"))?;
        let val =
            HeaderValue::from_str(v).map_err(|e| format!("invalid header value for `{k}`: {e}"))?;
        out.insert(name, val);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::ENV_TEST_LOCK;

    #[test]
    fn resolve_bearer_none_when_unset() {
        assert_eq!(resolve_bearer(None).unwrap(), None);
    }

    #[test]
    fn resolve_bearer_reads_env() {
        let _g = ENV_TEST_LOCK.lock().unwrap();
        std::env::set_var("IGNIS_TEST_BEARER_OK", "tok-123");
        let got = resolve_bearer(Some("IGNIS_TEST_BEARER_OK")).unwrap();
        std::env::remove_var("IGNIS_TEST_BEARER_OK");
        assert_eq!(got.as_deref(), Some("tok-123"));
    }

    #[test]
    fn resolve_bearer_errors_when_env_missing() {
        let _g = ENV_TEST_LOCK.lock().unwrap();
        std::env::remove_var("IGNIS_TEST_BEARER_MISSING");
        let err = resolve_bearer(Some("IGNIS_TEST_BEARER_MISSING")).unwrap_err();
        assert!(err.contains("IGNIS_TEST_BEARER_MISSING"), "err={err}");
    }

    #[test]
    fn resolve_bearer_errors_when_env_empty() {
        let _g = ENV_TEST_LOCK.lock().unwrap();
        std::env::set_var("IGNIS_TEST_BEARER_EMPTY", "");
        let err = resolve_bearer(Some("IGNIS_TEST_BEARER_EMPTY")).unwrap_err();
        std::env::remove_var("IGNIS_TEST_BEARER_EMPTY");
        assert!(err.contains("empty"), "err={err}");
    }

    #[test]
    fn build_headers_roundtrips_literal() {
        let mut h = HashMap::new();
        h.insert("X-Tenant".to_string(), "acme".to_string());
        h.insert("X-Region".to_string(), "us-east-1".to_string());
        let map = build_headers(&h).unwrap();
        assert_eq!(
            map.get(&HeaderName::from_static("x-tenant"))
                .map(|v| v.to_str().unwrap()),
            Some("acme")
        );
        assert_eq!(
            map.get(&HeaderName::from_static("x-region"))
                .map(|v| v.to_str().unwrap()),
            Some("us-east-1")
        );
    }

    #[test]
    fn build_headers_rejects_invalid_name() {
        let mut h = HashMap::new();
        h.insert("not a header".to_string(), "ok".to_string());
        let err = build_headers(&h).unwrap_err();
        assert!(err.contains("invalid header name"), "err={err}");
    }
}
