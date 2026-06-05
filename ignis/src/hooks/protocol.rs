//! Serde structs for the hook stdin → stdout JSON envelope.
//!
//! Mirrors Claude Code's `hookSpecificOutput` shape so users transferring
//! from CC don't have to learn a new schema — the only addition is
//! `updatedInput` / `updatedOutput` on the events that mutate text.

use serde::{Deserialize, Serialize};

/// Names of the hook events ignis emits. Stringly serialised in the
/// envelope to match Claude Code's wire format. New events extend this
/// enum; existing values must not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    UserPromptSubmit,
    AssistantMessageRender,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::AssistantMessageRender => "AssistantMessageRender",
        }
    }

    /// Stable declaration order for the `/hooks` listing and any other
    /// consumer that needs to iterate every event. Adding a new variant
    /// is a two-line change (this slice + the `as_str` arm); the listing
    /// will pick it up automatically.
    pub const ALL: &'static [HookEvent] = &[
        HookEvent::UserPromptSubmit,
        HookEvent::AssistantMessageRender,
    ];
}

/// JSON written to the hook subprocess's stdin. Fields use camelCase /
/// snake_case as Claude Code does — `hook_event_name` is snake_case in CC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookInput {
    pub hook_event_name: String,
    pub session_id: String,
    pub cwd: String,
    /// Present for `UserPromptSubmit`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt: Option<String>,
    /// Present for `AssistantMessageRender`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<String>,
}

/// JSON the hook subprocess writes back on stdout. All fields are optional;
/// an exit 0 with empty stdout means "pass through unchanged".
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct HookOutput {
    /// `false` blocks the chain for `UserPromptSubmit` (exit 2 has the same
    /// effect with stderr shown). For `AssistantMessageRender` blocking is
    /// treated as a soft failure — see `dispatch.rs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#continue: Option<bool>,
    #[serde(rename = "systemMessage", skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// The per-event payload that carries the rewrite. Unknown fields are
/// tolerated so future event types add their own without breaking older
/// hooks reading the same envelope.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName", skip_serializing_if = "Option::is_none")]
    pub hook_event_name: Option<String>,
    /// `UserPromptSubmit`: the rewritten prompt.
    #[serde(rename = "updatedInput", skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<String>,
    /// `AssistantMessageRender`: the rewritten output.
    #[serde(rename = "updatedOutput", skip_serializing_if = "Option::is_none")]
    pub updated_output: Option<String>,
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
            content: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        // Pass-through "content" is omitted in the wire shape.
        assert!(!json.contains("\"content\""));
        assert!(json.contains("\"prompt\":\"hello\""));
        let back: HookInput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn output_parses_updated_input() {
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
        assert_eq!(spec.updated_input.as_deref(), Some("rewritten prompt"));
        assert_eq!(spec.hook_event_name.as_deref(), Some("UserPromptSubmit"));
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
}
