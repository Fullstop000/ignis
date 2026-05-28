//! `PermissionChecker` — wires the pure `check()` decision into the existing
//! `ToolHooks::before_tool_call` integration point. When the decision is
//! `Ask`, opens a permission picker over the shared `PickerRequest` channel
//! (same channel `ask_user` uses) with three fixed options:
//! `Approve once`, `Approve session`, `Deny`. No "Other" free-text row —
//! `PickerQuestion::allow_other` is `false` for permission prompts.
//!
//! Behavior matrix:
//! - Allow → return Ok(())  (tool runs)
//! - Deny  → return Err(reason)  (agent loop wraps as "Blocked by hook: …")
//! - Ask + session-allow-set has the tool name → Allow
//! - Ask + console picker present → open picker, wait for response
//! - Ask + console picker absent (headless) → Deny with "no interactive console"

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use super::runtime::PermissionState;
use super::{check, default_policy_for_tool, Decision};
use crate::console::picker::{
    PickerAnswer, PickerOption, PickerQuestion, PickerRequest, PickerResponse,
};
use crate::tools::tool::ToolHooks;

const APPROVE_ONCE: &str = "Approve once";
const APPROVE_SESSION: &str = "Approve session";
const DENY: &str = "Deny";

/// Hook impl. Holds the runtime state and an optional picker channel — the
/// console wires the picker in when running interactively; headless callers
/// pass `None` and Ask decisions become Deny ("no console available").
pub struct PermissionChecker {
    state: Arc<PermissionState>,
    picker_tx: Option<mpsc::Sender<PickerRequest>>,
}

impl PermissionChecker {
    pub fn new(state: Arc<PermissionState>) -> Self {
        Self {
            state,
            picker_tx: None,
        }
    }

    /// Attach the picker channel. With it, an `Ask` decision opens the
    /// permission picker; without it, an `Ask` decision becomes `Deny`.
    pub fn with_picker(mut self, picker_tx: mpsc::Sender<PickerRequest>) -> Self {
        self.picker_tx = Some(picker_tx);
        self
    }

    pub fn state(&self) -> &Arc<PermissionState> {
        &self.state
    }

    /// Build the picker question shown for a tool-call Ask.
    fn picker_question(tool_name: &str, reason: &str, args: &serde_json::Value) -> PickerQuestion {
        // Short header chip (≤12 chars). Empty if the tool name is too long.
        let header = if tool_name.len() <= 12 {
            tool_name.to_string()
        } else {
            "tool call".to_string()
        };
        // Args summary for the question body — bash gets the command;
        // edit_file / create_file get the path; everything else gets a JSON-ish
        // truncated snapshot.
        let summary = match tool_name {
            "bash" => args
                .get("command")
                .and_then(|v| v.as_str())
                .map(|s| format!("`{s}`"))
                .unwrap_or_default(),
            "edit_file" | "create_file" => args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| format!("`{s}`"))
                .unwrap_or_default(),
            _ => {
                let s = args.to_string();
                if s.len() <= 80 {
                    s
                } else {
                    format!("{}…", &s[..79])
                }
            }
        };
        let question = if summary.is_empty() {
            format!("Approve `{tool_name}` — {reason}?")
        } else {
            format!("Approve `{tool_name}` {summary} — {reason}?")
        };
        PickerQuestion {
            question,
            kind: "permission".to_string(),
            header,
            multi_select: false,
            allow_other: false,
            options: vec![
                PickerOption {
                    label: APPROVE_ONCE.to_string(),
                    description: "Run this call this time only.".to_string(),
                    preview: None,
                },
                PickerOption {
                    label: APPROVE_SESSION.to_string(),
                    description: format!(
                        "Auto-approve `{tool_name}` for the rest of this session."
                    ),
                    preview: None,
                },
                PickerOption {
                    label: DENY.to_string(),
                    description: "Refuse — the model sees an error.".to_string(),
                    preview: None,
                },
            ],
        }
    }
}

#[async_trait]
impl ToolHooks for PermissionChecker {
    async fn before_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        // Session-level "Approve session" is honored inside `check()` AFTER
        // the safety floor (circuit breakers, protected paths) has had its
        // say. The floor is non-negotiable — even an explicit prior
        // "Approve session" can't allow `rm -rf /`.
        let session_allowed = self.state.is_session_allowed(tool_name);

        let decision = check(
            tool_name,
            &args.to_string(),
            default_policy_for_tool(tool_name),
            self.state.mode(),
            session_allowed,
        );

        match decision {
            Decision::Allow => Ok(()),
            Decision::Deny { reason } => Err(reason),
            Decision::Ask { reason } => {
                let Some(tx) = &self.picker_tx else {
                    return Err(format!(
                        "no interactive console available to approve ({reason}). \
                         Re-run with --afk for unattended runs."
                    ));
                };

                // Build the picker request and await the user's pick.
                let (reply_tx, reply_rx) = oneshot::channel();
                let request = PickerRequest {
                    questions: vec![Self::picker_question(tool_name, &reason, args)],
                    reply: reply_tx,
                };
                if tx.send(request).await.is_err() {
                    return Err("permission picker channel closed; refusing the call".to_string());
                }
                let response = match reply_rx.await {
                    Ok(r) => r,
                    Err(_) => {
                        return Err(
                            "permission picker dropped the reply; refusing the call".to_string()
                        );
                    }
                };

                match response {
                    PickerResponse::Cancelled => Err(format!(
                        "user cancelled the permission prompt for `{tool_name}`"
                    )),
                    PickerResponse::Answered(answers) => {
                        let label = match answers.first() {
                            Some(PickerAnswer::Single(s)) => s.as_str(),
                            // The picker is single-select with allow_other=false,
                            // so Multi shouldn't fire — refuse loudly if it does.
                            Some(PickerAnswer::Multi(_)) | None => {
                                return Err(
                                    "permission picker returned an unexpected answer shape"
                                        .to_string(),
                                );
                            }
                        };
                        match label {
                            APPROVE_ONCE => Ok(()),
                            APPROVE_SESSION => {
                                self.state.add_session_allow(tool_name);
                                Ok(())
                            }
                            DENY => Err(format!(
                                "user denied the permission prompt for `{tool_name}`"
                            )),
                            other => Err(format!(
                                "permission picker returned unknown option `{other}`"
                            )),
                        }
                    }
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
        let state = PermissionState::new(Mode::Off);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("read_file", &json!({"path": "src/main.rs"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn denies_bash_in_default_mode_without_picker() {
        let state = PermissionState::new(Mode::Off);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build"}))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("no interactive console"), "msg: {msg}");
    }

    #[tokio::test]
    async fn hands_free_allows_bash() {
        let state = PermissionState::new(Mode::HandsFree);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build && cargo test"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fully_unattended_allows_bash() {
        let state = PermissionState::new(Mode::FullyUnattended);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fully_unattended_denies_circuit_breaker() {
        let state = PermissionState::new(Mode::FullyUnattended);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "rm -rf /"}))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("fully-unattended"), "msg: {msg}");
    }

    #[tokio::test]
    async fn session_allow_bypasses_picker() {
        let state = PermissionState::new(Mode::Off);
        state.add_session_allow("bash");
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "cargo build"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn read_only_bash_auto_allows_without_session_approval() {
        let state = PermissionState::new(Mode::Off);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "git status"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn hands_free_still_blocks_circuit_breaker_in_headless() {
        // HandsFree + rm -rf / + no picker → Deny because Ask + no console.
        let state = PermissionState::new(Mode::HandsFree);
        let checker = PermissionChecker::new(state);
        let result = checker
            .before_tool_call("bash", &json!({"command": "rm -rf /"}))
            .await;
        assert!(result.is_err());
    }

    // ---- picker integration ---------------------------------------------

    /// Mock console: receives picker requests, replies with the given answer.
    async fn run_with_picker_reply(
        state: Arc<PermissionState>,
        tool: &str,
        args: serde_json::Value,
        reply: PickerResponse,
    ) -> (Result<(), String>, Option<PickerQuestion>) {
        let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
        let checker = PermissionChecker::new(state).with_picker(tx);
        // Spawn a fake console: pop the picker request, capture it, reply.
        let captured: tokio::task::JoinHandle<Option<PickerQuestion>> = tokio::spawn(async move {
            let req = rx.recv().await?;
            let q = req.questions.first().cloned();
            let _ = req.reply.send(reply);
            q
        });
        let result = checker.before_tool_call(tool, &args).await;
        let q = captured.await.unwrap();
        (result, q)
    }

    #[tokio::test]
    async fn picker_approve_once_allows_call_but_does_not_persist() {
        let state = PermissionState::new(Mode::Off);
        let (result, q) = run_with_picker_reply(
            state.clone(),
            "bash",
            json!({"command": "cargo build"}),
            PickerResponse::Answered(vec![PickerAnswer::Single(APPROVE_ONCE.to_string())]),
        )
        .await;
        assert!(result.is_ok());
        // Question shape: not multi-select, no Other row.
        let q = q.expect("picker request reached the channel");
        assert!(!q.multi_select);
        assert!(!q.allow_other);
        assert_eq!(q.options.len(), 3);
        // Did NOT persist into session_allow.
        assert!(!state.is_session_allowed("bash"));
    }

    #[tokio::test]
    async fn picker_approve_session_persists_into_state() {
        let state = PermissionState::new(Mode::Off);
        let (result, _) = run_with_picker_reply(
            state.clone(),
            "bash",
            json!({"command": "cargo build"}),
            PickerResponse::Answered(vec![PickerAnswer::Single(APPROVE_SESSION.to_string())]),
        )
        .await;
        assert!(result.is_ok());
        assert!(state.is_session_allowed("bash"));
    }

    #[tokio::test]
    async fn picker_deny_returns_user_denied_error() {
        let state = PermissionState::new(Mode::Off);
        let (result, _) = run_with_picker_reply(
            state,
            "bash",
            json!({"command": "rm /tmp/foo"}),
            PickerResponse::Answered(vec![PickerAnswer::Single(DENY.to_string())]),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("denied"));
    }

    #[tokio::test]
    async fn picker_cancelled_treated_as_deny() {
        let state = PermissionState::new(Mode::Off);
        let (result, _) = run_with_picker_reply(
            state,
            "bash",
            json!({"command": "rm /tmp/foo"}),
            PickerResponse::Cancelled,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cancelled"));
    }

    #[tokio::test]
    async fn picker_question_includes_bash_command_in_body() {
        let state = PermissionState::new(Mode::Off);
        let (_, q) = run_with_picker_reply(
            state,
            "bash",
            json!({"command": "cargo fmt --all"}),
            PickerResponse::Answered(vec![PickerAnswer::Single(APPROVE_ONCE.to_string())]),
        )
        .await;
        let q = q.unwrap();
        assert!(
            q.question.contains("cargo fmt --all"),
            "expected command in question body, got: {}",
            q.question
        );
    }

    #[tokio::test]
    async fn picker_question_includes_edit_path() {
        let state = PermissionState::new(Mode::Off);
        let (_, q) = run_with_picker_reply(
            state,
            "edit_file",
            json!({"path": "src/main.rs", "old_text": "x", "new_text": "y"}),
            PickerResponse::Answered(vec![PickerAnswer::Single(APPROVE_ONCE.to_string())]),
        )
        .await;
        let q = q.unwrap();
        assert!(
            q.question.contains("src/main.rs"),
            "expected path in question body, got: {}",
            q.question
        );
    }
}
