//! Serde structs for the hook stdin → stdout JSON envelope.
//!
//! Mirrors Claude Code's `hookSpecificOutput` shape so users transferring
//! from CC don't have to learn a new schema. v2 extends v1's 2 events to
//! 8, but the envelope shape is unchanged for `UserPromptSubmit` and
//! `AssistantMessageRender` — v1 hooks keep working byte-for-byte.

use serde::{Deserialize, Serialize};

/// Names of the hook events ignis emits. Stringly serialised in the
/// envelope to match Claude Code's wire format. New events extend this
/// enum; existing values must not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    UserPromptSubmit,
    AssistantMessageRender,
    SystemPromptCompose,
    PreToolUse,
    PostToolUse,
    PreCompact,
    PostCompact,
    SessionStart,
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::AssistantMessageRender => "AssistantMessageRender",
            HookEvent::SystemPromptCompose => "SystemPromptCompose",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::Stop => "Stop",
        }
    }

    /// Stable declaration order for the `/hooks` listing and any other
    /// consumer that needs to iterate every event. Adding a new variant
    /// is a three-line change (this slice + the `as_str` arm + the
    /// `from_event_name` match in config.rs).
    pub const ALL: &'static [HookEvent] = &[
        HookEvent::UserPromptSubmit,
        HookEvent::AssistantMessageRender,
        HookEvent::SystemPromptCompose,
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::SessionStart,
        HookEvent::Stop,
    ];

    /// Events whose envelope carries a `tool_name`. The `matcher` field
    /// on a hook spec is meaningful only for these — declaring `matcher`
    /// on (say) `SessionStart` triggers a one-time `[warn]` at load.
    pub fn uses_tool_matcher(self) -> bool {
        matches!(self, HookEvent::PreToolUse | HookEvent::PostToolUse)
    }
}

/// JSON written to the hook subprocess's stdin. Fields use camelCase /
/// snake_case as Claude Code does — `hook_event_name` is snake_case in CC.
///
/// Per-event fields are all `Option<…>`; the hook reads only the fields
/// its event populates and ignores the rest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HookInput {
    pub hook_event_name: String,
    pub session_id: String,
    pub cwd: String,
    /// RFC3339 timestamp of when ignis fired this hook. Stable downstream
    /// telemetry/log-correlation key; cribbed from Codex.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub triggered_at: Option<String>,

    /// `UserPromptSubmit`: the user's text.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt: Option<String>,
    /// `AssistantMessageRender`: the assistant text about to render.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<String>,
    /// `SystemPromptCompose`: the fully assembled system prompt.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub system_prompt: Option<String>,
    /// `SystemPromptCompose`: the model id this prompt is destined for.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    /// `PreToolUse` / `PostToolUse`: name of the tool about to run / that
    /// just ran. Same value the `matcher` regex is matched against.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_name: Option<String>,
    /// `PreToolUse` / `PostToolUse`: the JSON arguments the model called
    /// the tool with.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_input: Option<serde_json::Value>,
    /// `PostToolUse`: the tool's response — `success: bool` plus the
    /// payload (`content: string` or a structured object). Shape mirrors
    /// CC's `tool_response` field.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_response: Option<serde_json::Value>,
    /// `PreCompact` / `PostCompact`: `"auto"` or `"manual"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub trigger: Option<String>,
    /// `PreCompact` / `Stop`: path to the session transcript on disk.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub transcript_path: Option<String>,
    /// `PostCompact`: the LLM-produced summary text the session was
    /// compacted to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    /// `SessionStart`: `"new"`, `"resume"`, or `"subagent"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
}

/// JSON the hook subprocess writes back on stdout. All fields are optional;
/// an exit 0 with empty stdout means "pass through unchanged".
///
/// `Eq` is deliberately not derived: `HookSpecificOutput::updated_input`
/// holds a `serde_json::Value`, which is `PartialEq` but not `Eq` because
/// `Value::Number` can wrap an `f64`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct HookOutput {
    /// `false` blocks the chain. For `UserPromptSubmit` this rejects the
    /// turn; for `AssistantMessageRender` it degrades to a soft failure
    /// (the message already exists). See `dispatch.rs` for per-event
    /// semantics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#continue: Option<bool>,
    #[serde(rename = "systemMessage", skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Suppress this hook's stdout from the user-facing transcript. The
    /// rewrite still takes effect; only the hook's own logging is hidden.
    #[serde(rename = "suppressOutput", skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
    /// `"block"` short-circuits the chain with per-event semantics
    /// (see `dispatch.rs`). For `Stop`, `"block"` means "keep looping"
    /// — the CC inversion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Free-text reason surfaced as a system reminder to the model when
    /// `decision: "block"` fires. Required UX for any block to be useful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// One-line note shown in the TUI when `continue: false` fires.
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// The per-event payload that carries the rewrite. Unknown fields are
/// tolerated so future event types add their own without breaking older
/// hooks reading the same envelope.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName", skip_serializing_if = "Option::is_none")]
    pub hook_event_name: Option<String>,
    /// `UserPromptSubmit` (string) or `PreToolUse` (object): the rewrite.
    /// `Value` accepts both shapes — for `UserPromptSubmit` the value is
    /// a JSON string; for `PreToolUse` it's a JSON object replacing
    /// `tool_input`.
    #[serde(rename = "updatedInput", skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
    /// `AssistantMessageRender`: the rewritten output.
    #[serde(rename = "updatedOutput", skip_serializing_if = "Option::is_none")]
    pub updated_output: Option<String>,
    /// `SystemPromptCompose`: the rewritten system prompt.
    #[serde(
        rename = "updatedSystemPrompt",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_system_prompt: Option<String>,
    /// `SessionStart` / `UserPromptSubmit` / `PostToolUse` / `PreCompact` /
    /// `PostCompact` / `Stop`: free-text context spliced into the model's
    /// next turn as a system reminder. The "inject thinking" channel.
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_round_trips_user_prompt_submit() {
        let input = HookInput {
            hook_event_name: "UserPromptSubmit".to_string(),
            session_id: "s1".to_string(),
            cwd: "/tmp".to_string(),
            prompt: Some("hello".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&input).unwrap();
        // Pass-through fields are omitted in the wire shape.
        assert!(!json.contains("\"content\""));
        assert!(!json.contains("\"tool_name\""));
        assert!(json.contains("\"prompt\":\"hello\""));
        let back: HookInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn input_round_trips_pre_tool_use_envelope() {
        // PreToolUse-shaped envelope: tool_name + tool_input object, no
        // prompt or content. Pins the v2 wire format.
        let input = HookInput {
            hook_event_name: "PreToolUse".to_string(),
            session_id: "s2".to_string(),
            cwd: "/repo".to_string(),
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({ "command": "ls /tmp" })),
            ..Default::default()
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("\"tool_name\":\"Bash\""));
        assert!(json.contains("\"tool_input\""));
        assert!(!json.contains("\"prompt\""));
        let back: HookInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn output_parses_updated_input_string() {
        // v1 wire shape: a UserPromptSubmit hook returns a string rewrite.
        // The Value-typed updated_input must accept it unchanged for
        // v1-hook back-compat.
        let raw = r#"{
            "continue": true,
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "updatedInput": "rewritten prompt"
            }
        }"#;
        let out: HookOutput = serde_json::from_str(raw).unwrap();
        assert_eq!(out.r#continue, Some(true));
        let spec = out.hook_specific_output.expect("hookSpecificOutput");
        assert_eq!(
            spec.updated_input.as_ref().and_then(|v| v.as_str()),
            Some("rewritten prompt")
        );
        assert_eq!(spec.hook_event_name.as_deref(), Some("UserPromptSubmit"));
    }

    #[test]
    fn output_parses_updated_input_object() {
        // v2 wire shape: PreToolUse hook returns an object replacing
        // tool_input.
        let raw = r#"{
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "updatedInput": { "command": "echo safe" }
            }
        }"#;
        let out: HookOutput = serde_json::from_str(raw).unwrap();
        let spec = out.hook_specific_output.expect("hookSpecificOutput");
        let obj = spec
            .updated_input
            .as_ref()
            .and_then(|v| v.as_object())
            .expect("expected object");
        assert_eq!(
            obj.get("command").and_then(|v| v.as_str()),
            Some("echo safe")
        );
    }

    #[test]
    fn output_parses_additional_context_and_decision_block() {
        let raw = r#"{
            "decision": "block",
            "reason": "destructive command",
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": "test suite failed"
            }
        }"#;
        let out: HookOutput = serde_json::from_str(raw).unwrap();
        assert_eq!(out.decision.as_deref(), Some("block"));
        assert_eq!(out.reason.as_deref(), Some("destructive command"));
        let spec = out.hook_specific_output.expect("hookSpecificOutput");
        assert_eq!(
            spec.additional_context.as_deref(),
            Some("test suite failed")
        );
    }

    #[test]
    fn output_tolerates_unknown_top_level_fields() {
        let raw = r#"{
            "continue": false,
            "futureField": 42,
            "hookSpecificOutput": {"updatedOutput": "x", "extra": "ignored"}
        }"#;
        let out: HookOutput = serde_json::from_str(raw).unwrap();
        assert_eq!(out.r#continue, Some(false));
        assert_eq!(
            out.hook_specific_output
                .and_then(|s| s.updated_output)
                .as_deref(),
            Some("x")
        );
    }

    #[test]
    fn empty_object_is_a_valid_passthrough() {
        let out: HookOutput = serde_json::from_str("{}").unwrap();
        assert_eq!(out, HookOutput::default());
    }

    #[test]
    fn uses_tool_matcher_is_true_only_for_tool_events() {
        // Pins the allow-list `matcher` is meaningful on. Adding a new
        // tool event must extend this set (otherwise users declaring
        // matcher silently lose it).
        assert!(HookEvent::PreToolUse.uses_tool_matcher());
        assert!(HookEvent::PostToolUse.uses_tool_matcher());
        for ev in HookEvent::ALL {
            if !matches!(ev, HookEvent::PreToolUse | HookEvent::PostToolUse) {
                assert!(
                    !ev.uses_tool_matcher(),
                    "{} unexpectedly uses tool matcher",
                    ev.as_str()
                );
            }
        }
    }

    #[test]
    fn all_event_names_round_trip_through_as_str() {
        // Every variant in ALL has a stable string label. The wire format
        // is keyed on these — a typo or rename here is a breaking change.
        for ev in HookEvent::ALL {
            let s = ev.as_str();
            assert!(!s.is_empty());
            // PascalCase, matches CC's convention.
            assert!(s.chars().next().unwrap().is_ascii_uppercase());
        }
    }
}
