//! Integration tests for the telemetry module's public API. The in-memory OTel
//! exporter is heavyweight to wire up; these tests focus on the critical
//! invariant: every public recorder must be a safe no-op before / after init,
//! since the agent loop calls them on every turn regardless of telemetry state.
//!
//! Env-var-dependent behavior is covered by the unit tests in `telemetry.rs`
//! to avoid parallel-test races over process env state.
//!
//! Spec: docs/superpowers/specs/2026-05-28-otel-integration-design.md

use ignis::telemetry;
use ignis::Usage;
use std::time::Duration;

/// `record_*` functions must be safe no-ops when `init()` has not been called
/// (or when telemetry is disabled). Critical: these run on every turn even
/// when telemetry is off.
#[test]
fn record_functions_are_safe_without_init() {
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 50,
        reasoning_tokens: 10,
        cache_read_tokens: 20,
        cache_write_tokens: 0,
    };
    telemetry::record_session_start("openai", "gpt-5");
    telemetry::record_tokens(&usage, "openai", "gpt-5");
    telemetry::record_tool_call("read_file", Duration::from_millis(42), true);
    telemetry::record_llm_request("openai", "gpt-5", Duration::from_millis(1500), true);
}

/// State snapshot must return a valid struct even before init.
#[test]
fn state_snapshot_works_without_init() {
    let snap = telemetry::state_snapshot();
    // Default snapshot exists; no panic. enabled may be false (init not called)
    // or true (some other test initialized — global state). Just confirm it's
    // a coherent struct.
    let _ = snap.enabled;
    let _ = snap.endpoint;
}
