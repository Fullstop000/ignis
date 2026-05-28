//! `PermissionChecker` — wires the pure `check()` decision into the existing
//! `ToolHooks::before_tool_call` integration point. When the decision is
//! `Ask`, this is also where we'd open a picker (v0.16.0 lands the engine +
//! state + per-tool gating; the picker wiring is added in the next chunk so
//! we don't blow context here).
//!
//! v0.16.0 behavior:
//! - Allow → return Ok(())  (tool runs)
//! - Deny  → return Err(reason)  (agent loop wraps as "Blocked by hook: …")
//! - Ask + session-allow-set has the tool name → Allow
//! - Ask + console picker present → open picker, wait for response
//! - Ask + console picker absent (headless) → Deny with "no interactive console"

use async_trait::async_trait;
use std::sync::Arc;

use super::runtime::PermissionState;
use super::{check, default_policy_for_tool, Decision};
use crate::tools::tool::ToolHooks;

/// Hook impl. Holds the runtime state plus an optional picker channel; the
/// console wires the picker in when running interactively.
pub struct PermissionChecker {
    state: Arc<PermissionState>,
    // Picker channel is added in chunk 3 (TUI integration). For now the
    // checker either Allows / Denies based on the pure decision, with Ask
    // resolved by the session-allow set or → Deny in headless mode.
}

impl PermissionChecker {
    pub fn new(state: Arc<PermissionState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &Arc<PermissionState> {
        &self.state
    }
}

#[async_trait]
impl ToolHooks for PermissionChecker {
    async fn before_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        // Fast path: session-level "Approve session" overrides everything
        // short of circuit breakers + protected paths. Re-evaluate the safety
        // floor explicitly to avoid bypassing it.
        let session_pre_approved = self.state.is_session_allowed(tool_name);

        let decision = check(
            tool_name,
            &args.to_string(),
            default_policy_for_tool(tool_name),
            self.state.mode(),
            self.state.afk(),
        );

        match decision {
            Decision::Allow => Ok(()),
            Decision::Deny { reason } => Err(reason),
            Decision::Ask { reason } => {
                if session_pre_approved {
                    // The user pre-approved this tool for the session AND the
                    // decision wasn't a safety-floor Deny (circuit breaker etc.
                    // would have returned Ask above; we still let session
                    // approval shortcut here because the user explicitly chose
                    // "Approve session" — they accepted the risk).
                    Ok(())
                } else {
                    // No picker channel in this chunk; the next commit wires
                    // it. For now refuse loudly so headless runs don't silently
                    // proceed and dogfood surfaces the gap immediately.
                    Err(format!(
                        "no interactive console available to approve ({reason}). \
                         Re-run with --permission-mode bypassPermissions or --afk."
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Mode;
    use serde_json::json;

    #[tokio::test]
    async fn allows_read_tool_in_default_mode() {
        let state = PermissionState::new(Mode::Default, false);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("read_file", &json!({"path": "src/main.rs"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn denies_bash_in_default_mode_without_picker() {
        let state = PermissionState::new(Mode::Default, false);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build"}))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("no interactive console"), "msg: {msg}");
    }

    #[tokio::test]
    async fn bypass_allows_bash() {
        let state = PermissionState::new(Mode::BypassPermissions, false);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build && cargo test"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn afk_allows_bash() {
        let state = PermissionState::new(Mode::Default, true);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn afk_denies_circuit_breaker() {
        let state = PermissionState::new(Mode::Default, true);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "rm -rf /"}))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("AFK"), "msg: {msg}");
    }

    #[tokio::test]
    async fn session_allow_bypasses_picker() {
        let state = PermissionState::new(Mode::Default, false);
        state.add_session_allow("bash");
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn read_only_bash_auto_allows_without_session_approval() {
        let state = PermissionState::new(Mode::Default, false);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "git status"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn bypass_still_blocks_circuit_breaker_in_headless() {
        // Bypass mode + rm -rf / + no picker → Deny because Ask + no console.
        let state = PermissionState::new(Mode::BypassPermissions, false);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "rm -rf /"}))
            .await;
        assert!(result.is_err());
    }
}
