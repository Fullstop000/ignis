//! Agent permission control — gate every tool call through a small, predictable
//! decision pipeline before dispatch.
//!
//! v0.17.0 collapses what used to be a 2D space (permission mode + AFK toggle)
//! into a single 3-state `Mode` enum. The middle "auto-approve but `ask_user`
//! still works" state lives on as `HandsFree`; the heavy headless state is
//! `FullyUnattended`. Permission-mode CLI flag and the `bypassPermissions` name
//! are gone.
//!
//! Integration: a `PermissionChecker` impls `tools::tool::ToolHooks`; the
//! agent loop already invokes `before_tool_call` on every dispatch (see
//! `agent/mod.rs:608`). On `Decision::Ask`, the checker opens the existing
//! `PickerRequest` channel to the console (same plumbing as `ask_user`).

pub mod builtin;
pub mod checker;
pub mod rule;
pub mod runtime;

use serde::{Deserialize, Serialize};

/// The single axis for "how much should ignis prompt me?" There are exactly
/// three real points on it: full prompts (Off), keyboard-present but flow
/// (HandsFree), and headless/unattended (FullyUnattended). The two AFK levels
/// share auto-approve of sensitive tools but differ on (a) safety-floor
/// behavior and (b) whether `ask_user` is dismissed.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Prompt for sensitive tools; safety floor asks; `ask_user` prompts.
    #[default]
    Off,
    /// Auto-approve sensitive tools, but safety floor still asks and
    /// `ask_user` still prompts. For interactive sessions where you want
    /// flow without losing oversight on judgment calls.
    HandsFree,
    /// Auto-approve sensitive tools; safety floor hard-denies (no one to
    /// confirm); `ask_user` is auto-dismissed. For CI, overnight, one-shot.
    FullyUnattended,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::HandsFree => "hands_free",
            Mode::FullyUnattended => "fully_unattended",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" => Some(Mode::Off),
            "hands_free" => Some(Mode::HandsFree),
            "fully_unattended" => Some(Mode::FullyUnattended),
            _ => None,
        }
    }

    /// True for HandsFree and FullyUnattended — the two states where the
    /// per-tool default `Ask` is auto-promoted to `Allow`.
    pub fn auto_approves_sensitive(&self) -> bool {
        matches!(self, Mode::HandsFree | Mode::FullyUnattended)
    }

    /// True only for FullyUnattended — the state where safety-floor `Ask`
    /// becomes hard `Deny` (no human to confirm) and `ask_user` auto-dismisses.
    pub fn is_fully_unattended(&self) -> bool {
        matches!(self, Mode::FullyUnattended)
    }
}

/// The decision the checker hands back to the agent loop. `Ask` is the only
/// variant that triggers UI; the others short-circuit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow,
    Ask { reason: String },
    Deny { reason: String },
}

impl Decision {
    pub fn ask(reason: impl Into<String>) -> Self {
        Decision::Ask {
            reason: reason.into(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Decision::Deny {
            reason: reason.into(),
        }
    }
}

/// Resolve the decision for one tool call, applying (in order):
///
/// 1. **Circuit breakers** on raw bash args (`rm -rf /`, `rm -rf ~`,
///    `rm -rf $HOME`) — `Ask` under Off/HandsFree, **hard `Deny`** under
///    FullyUnattended (no user to authorize). Always runs first.
/// 2. **Protected-path edits** (`edit_file`/`create_file` targeting
///    `.git/**`, `.ignis/**`, shell init, etc.) — same `Ask`/`Deny` split as
///    step 1. Same precedence rule: floor first.
/// 3. **User rule layer** — config `[permissions]` + persisted grants, ordered
///    `deny > ask > allow`. Beats session-allow and the auto-approve modes; an
///    `ask` hardens to `Deny` under FullyUnattended. Still below the floor.
/// 4. **Session-allow** shortcut (only after the floor has had its say).
/// 5. **Read-only bash auto-allow** for ~30 curated commands.
/// 6. **HandsFree / FullyUnattended** auto-approve the sensitive tools whose
///    per-tool default would otherwise be `Ask`.
/// 7. **Per-tool default** policy.
pub fn check(
    tool_name: &str,
    args: &serde_json::Value,
    default_for_tool: Decision,
    mode: Mode,
    session_allowed: bool,
    ruleset: &rule::RuleSet,
) -> Decision {
    // Step 1: circuit breakers (raw bash arg scan). These ALWAYS take
    // precedence over session-allow shortcuts and any auto-approve mode —
    // that's the whole point of the floor.
    if tool_name == "bash" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            if builtin::is_circuit_breaker(cmd) {
                let reason = format!(
                    "destructive command pattern matched ({}); requires explicit confirmation",
                    builtin::circuit_breaker_label(cmd).unwrap_or("circuit breaker")
                );
                return if mode.is_fully_unattended() {
                    Decision::deny(format!(
                        "fully-unattended mode: {reason}. Toggle off and authorize manually."
                    ))
                } else {
                    Decision::ask(reason)
                };
            }
        }
    }

    // Step 2: protected-path edits. Same precedence rule as step 1: floor first.
    if matches!(tool_name, "edit_file" | "create_file") {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            if builtin::is_protected_path(path) {
                let reason = format!("edit targets a protected path: {path}");
                return if mode.is_fully_unattended() {
                    Decision::deny(format!(
                        "fully-unattended mode: {reason}. Toggle off and authorize manually."
                    ))
                } else {
                    Decision::ask(reason)
                };
            }
        }
    }

    // Step 3: user-declared rule layer (config `[permissions]` + persisted
    // grants). Sits *below* the non-negotiable floor (steps 1+2) but *above*
    // session-allow and the auto-approve modes: a config `deny` is the user's
    // explicit hard no (beats session-allow + HandsFree), and a config `ask`
    // forces oversight even under HandsFree. Under FullyUnattended an `ask`
    // becomes `Deny` (no human to confirm), mirroring the floor.
    match ruleset.decide(tool_name, args) {
        Some(Decision::Deny { reason }) => return Decision::deny(reason),
        Some(Decision::Ask { reason }) => {
            return if mode.is_fully_unattended() {
                Decision::deny(format!(
                    "fully-unattended mode: {reason}. Toggle off and authorize manually."
                ))
            } else {
                Decision::ask(reason)
            };
        }
        Some(Decision::Allow) => return Decision::Allow,
        None => {}
    }

    // Step 4: session-allow shortcut. Only reaches here once we've cleared
    // the safety floor (steps 1+2 return above). "Approve session" was
    // explicit consent for the tool — honor it, but never against the floor.
    if session_allowed {
        return Decision::Allow;
    }

    // Step 5: read-only bash auto-allow.
    if tool_name == "bash" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            if builtin::is_read_only_bash(cmd) {
                return Decision::Allow;
            }
        }
    }

    // Step 6: per-tool default. Auto-approve modes promote an `Ask` to `Allow`.
    match default_for_tool {
        Decision::Ask { .. } if mode.auto_approves_sensitive() => Decision::Allow,
        other => other,
    }
}

/// The per-tool default policy table. Returned at tool-registration time so
/// `check()` doesn't have to know every tool name.
pub fn default_policy_for_tool(tool_name: &str) -> Decision {
    match tool_name {
        // Pure reads.
        "read_file" | "list_dir" | "grep" | "glob" | "web_search" | "skill" => Decision::Allow,
        // ask_user is gated separately (FullyUnattended auto-dismisses inside
        // the tool, before the gate sees it).
        "ask_user" => Decision::Allow,
        // todo_write only updates the session's task list (in-memory state +
        // an internal sidecar) — no filesystem/network/exec reach. Gating it
        // would prompt on every plan update; allow like other state reads.
        "todo_write" => Decision::Allow,
        // Network + writes + execution + agent spawn → ask by default.
        "web_fetch" => Decision::ask("network fetch"),
        "bash" => Decision::ask("shell command"),
        "edit_file" => Decision::ask("file edit"),
        "create_file" => Decision::ask("file creation"),
        "agent" => Decision::ask("spawn subagent"),
        // MCP tools (mcp__server__tool naming per [[mcp-system-design]]).
        name if name.starts_with("mcp__") => Decision::ask("MCP server call"),
        // Unknown tool → ask, fail loud.
        _ => Decision::ask("unknown tool"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn mode_parse_roundtrip() {
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse("hands_free"), Some(Mode::HandsFree));
        assert_eq!(Mode::parse("fully_unattended"), Some(Mode::FullyUnattended));
        assert_eq!(Mode::parse("nonsense"), None);
        assert_eq!(Mode::Off.as_str(), "off");
        assert_eq!(Mode::HandsFree.as_str(), "hands_free");
        assert_eq!(Mode::FullyUnattended.as_str(), "fully_unattended");
    }

    #[test]
    fn mode_classification_predicates() {
        assert!(!Mode::Off.auto_approves_sensitive());
        assert!(Mode::HandsFree.auto_approves_sensitive());
        assert!(Mode::FullyUnattended.auto_approves_sensitive());
        assert!(!Mode::Off.is_fully_unattended());
        assert!(!Mode::HandsFree.is_fully_unattended());
        assert!(Mode::FullyUnattended.is_fully_unattended());
    }

    #[test]
    fn default_policy_covers_every_known_tool() {
        for tool in [
            "read_file",
            "list_dir",
            "grep",
            "glob",
            "web_search",
            "skill",
            "ask_user",
            "todo_write",
            "web_fetch",
            "bash",
            "edit_file",
            "create_file",
            "agent",
        ] {
            // None of these may panic; each yields a concrete decision.
            let _ = default_policy_for_tool(tool);
        }
    }

    #[test]
    fn todo_write_allows_by_default() {
        assert_eq!(default_policy_for_tool("todo_write"), Decision::Allow);
    }

    #[test]
    fn mcp_tools_default_to_ask() {
        assert!(matches!(
            default_policy_for_tool("mcp__shell__exec"),
            Decision::Ask { .. }
        ));
        assert!(matches!(
            default_policy_for_tool("mcp__filesystem__write"),
            Decision::Ask { .. }
        ));
    }

    #[test]
    fn read_tool_in_off_mode_allows() {
        let d = check(
            "read_file",
            &json(r#"{"path":"src/main.rs"}"#),
            default_policy_for_tool("read_file"),
            Mode::Off,
            false,
            &rule::RuleSet::default(),
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn bash_in_off_mode_asks() {
        let d = check(
            "bash",
            &json(r#"{"command":"cargo build"}"#),
            default_policy_for_tool("bash"),
            Mode::Off,
            false,
            &rule::RuleSet::default(),
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }

    #[test]
    fn bash_read_only_auto_allows_even_in_off() {
        let d = check(
            "bash",
            &json(r#"{"command":"git status"}"#),
            default_policy_for_tool("bash"),
            Mode::Off,
            false,
            &rule::RuleSet::default(),
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn hands_free_allows_normal_tools() {
        let d = check(
            "bash",
            &json(r#"{"command":"cargo build && cargo test"}"#),
            default_policy_for_tool("bash"),
            Mode::HandsFree,
            false,
            &rule::RuleSet::default(),
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn fully_unattended_allows_normal_tools() {
        let d = check(
            "bash",
            &json(r#"{"command":"cargo build"}"#),
            default_policy_for_tool("bash"),
            Mode::FullyUnattended,
            false,
            &rule::RuleSet::default(),
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn hands_free_still_asks_circuit_breaker() {
        // The floor is non-negotiable under hands-free.
        for cmd in [
            r#"{"command":"rm -rf /"}"#,
            r#"{"command":"rm -rf ~"}"#,
            r#"{"command":"rm -rf $HOME"}"#,
            r#"{"command":"rm -rf \"$HOME\""}"#,
        ] {
            let d = check(
                "bash",
                &json(cmd),
                default_policy_for_tool("bash"),
                Mode::HandsFree,
                false,
                &rule::RuleSet::default(),
            );
            assert!(
                matches!(d, Decision::Ask { .. }),
                "expected Ask under HandsFree for circuit breaker, got {:?} on {}",
                d,
                cmd
            );
        }
    }

    #[test]
    fn hands_free_still_asks_protected_path_edit() {
        let d = check(
            "edit_file",
            &json(r#"{"path":".git/config"}"#),
            default_policy_for_tool("edit_file"),
            Mode::HandsFree,
            false,
            &rule::RuleSet::default(),
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }

    #[test]
    fn fully_unattended_denies_circuit_breakers() {
        let d = check(
            "bash",
            &json(r#"{"command":"rm -rf /"}"#),
            default_policy_for_tool("bash"),
            Mode::FullyUnattended,
            false,
            &rule::RuleSet::default(),
        );
        assert!(
            matches!(d, Decision::Deny { ref reason } if reason.contains("fully-unattended")),
            "expected FullyUnattended-Deny, got {:?}",
            d
        );
    }

    #[test]
    fn fully_unattended_denies_protected_path_edits() {
        let d = check(
            "edit_file",
            &json(r#"{"path":".bashrc"}"#),
            default_policy_for_tool("edit_file"),
            Mode::FullyUnattended,
            false,
            &rule::RuleSet::default(),
        );
        assert!(
            matches!(d, Decision::Deny { ref reason } if reason.contains("fully-unattended")),
            "expected FullyUnattended-Deny, got {:?}",
            d
        );
    }

    #[test]
    fn session_allow_does_not_bypass_circuit_breaker() {
        // Regression: a previous "Approve session" must NOT green-light a
        // subsequent destructive command. Safety floor runs before the
        // session-allow shortcut by design.
        let d = check(
            "bash",
            &json(r#"{"command":"rm -rf /"}"#),
            default_policy_for_tool("bash"),
            Mode::Off,
            true, // session_allowed — must NOT win against the safety floor
            &rule::RuleSet::default(),
        );
        assert!(
            matches!(d, Decision::Ask { .. }),
            "session-allow must NOT bypass circuit breaker, got {:?}",
            d
        );
    }

    #[test]
    fn session_allow_does_not_bypass_protected_path() {
        let d = check(
            "edit_file",
            &json(r#"{"path":".bashrc"}"#),
            default_policy_for_tool("edit_file"),
            Mode::Off,
            true,
            &rule::RuleSet::default(),
        );
        assert!(
            matches!(d, Decision::Ask { .. }),
            "session-allow must NOT bypass protected path, got {:?}",
            d
        );
    }

    #[test]
    fn session_allow_does_speed_up_normal_tool_calls() {
        let d = check(
            "bash",
            &json(r#"{"command":"cargo build"}"#),
            default_policy_for_tool("bash"),
            Mode::Off,
            true,
            &rule::RuleSet::default(),
        );
        assert_eq!(d, Decision::Allow);
    }

    // ---- config rule layer (deny > ask > allow, between floor and session) ----

    fn rules(allow: &[&str], ask: &[&str], deny: &[&str]) -> rule::RuleSet {
        let conv = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        rule::RuleSet::from_strings(&conv(allow), &conv(ask), &conv(deny))
    }

    #[test]
    fn config_allow_silences_a_normally_asked_tool() {
        let d = check(
            "bash",
            &json(r#"{"command":"cargo build"}"#),
            default_policy_for_tool("bash"),
            Mode::Off,
            false,
            &rules(&["bash(cargo *)"], &[], &[]),
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn config_deny_beats_session_allow() {
        // Even with the tool session-allowed, a config deny rule blocks it.
        let d = check(
            "bash",
            &json(r#"{"command":"cargo publish"}"#),
            default_policy_for_tool("bash"),
            Mode::Off,
            true, // session_allowed
            &rules(&[], &[], &["bash(cargo publish *)"]),
        );
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn config_deny_beats_hands_free_auto_approve() {
        let d = check(
            "bash",
            &json(r#"{"command":"cargo publish"}"#),
            default_policy_for_tool("bash"),
            Mode::HandsFree,
            false,
            &rules(&[], &[], &["bash(cargo publish *)"]),
        );
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn config_ask_beats_hands_free_auto_approve() {
        // HandsFree would auto-approve, but a config ask rule forces the prompt.
        let d = check(
            "bash",
            &json(r#"{"command":"git push origin main"}"#),
            default_policy_for_tool("bash"),
            Mode::HandsFree,
            false,
            &rules(&[], &["bash(git push *)"], &[]),
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }

    #[test]
    fn config_ask_becomes_deny_under_fully_unattended() {
        let d = check(
            "bash",
            &json(r#"{"command":"git push origin main"}"#),
            default_policy_for_tool("bash"),
            Mode::FullyUnattended,
            false,
            &rules(&[], &["bash(git push *)"], &[]),
        );
        assert!(matches!(d, Decision::Deny { .. }));
    }

    #[test]
    fn floor_still_beats_config_allow() {
        // A blanket bash allow does NOT override the circuit breaker.
        let d = check(
            "bash",
            &json(r#"{"command":"rm -rf /"}"#),
            default_policy_for_tool("bash"),
            Mode::Off,
            false,
            &rules(&["bash"], &[], &[]),
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }

    #[test]
    fn floor_still_beats_config_allow_under_fully_unattended() {
        let d = check(
            "bash",
            &json(r#"{"command":"rm -rf /"}"#),
            default_policy_for_tool("bash"),
            Mode::FullyUnattended,
            false,
            &rules(&["bash"], &[], &[]),
        );
        assert!(
            matches!(d, Decision::Deny { ref reason } if reason.contains("fully-unattended")),
            "floor must win over config allow, got {:?}",
            d
        );
    }

    #[test]
    fn missing_args_does_not_panic() {
        let d = check(
            "bash",
            &serde_json::Value::default(),
            default_policy_for_tool("bash"),
            Mode::Off,
            false,
            &rule::RuleSet::default(),
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }
}
