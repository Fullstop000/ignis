//! Agent permission control — gate every tool call through a small, predictable
//! decision pipeline before dispatch. Designed against the v0.16.0 cut of
//! `docs/superpowers/specs/2026-05-28-agent-permissions-design.md`:
//! safe-first defaults, no sandbox (advisory only), no allowlist grammar yet
//! (v0.17.0). The grammar-less v1 still delivers the safety floor: per-tool
//! defaults, read-only auto-allow set, circuit breaker, protected paths,
//! bypass mode, AFK, and the picker.
//!
//! Integration: a `PermissionChecker` impls `tools::tool::ToolHooks`; the
//! agent loop already invokes `before_tool_call` on every dispatch (see
//! `agent/mod.rs:608`). On `Decision::Ask`, the checker opens the existing
//! `PickerRequest` channel to the console (same plumbing as `ask_user`).

pub mod builtin;
pub mod checker;
pub mod runtime;

use serde::{Deserialize, Serialize};

/// Top-level permission mode. AFK is a separate independent toggle on
/// `PermissionState`, NOT a mode — they compose. Modes are deliberately few
/// (v1 ships `Default` + `BypassPermissions`; `AcceptEdits` and `Plan` ship
/// in v0.17.0+ once the allowlist grammar is in).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Ask for tools whose default policy is Ask; auto-allow the rest.
    #[default]
    Default,
    /// Auto-allow every tool call (subject to circuit breakers + protected
    /// paths). The escape hatch for trusted contexts (containers, scripted
    /// runs). Refused under sudo/root unless a sandbox env-var is detected.
    BypassPermissions,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "default" => Some(Mode::Default),
            "bypassPermissions" | "bypass" => Some(Mode::BypassPermissions),
            _ => None,
        }
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
///    `rm -rf $HOME`) — always `Ask`, regardless of mode. If AFK then
///    promote to `Deny` (no user available to authorize).
/// 2. **Protected-path edits** (`edit_file`/`create_file` targeting
///    `.git/**`, `.ignis/**`, shell init, etc.) — `Ask` under any mode
///    except `BypassPermissions`, where it stays `Ask` (intentional carve-out
///    so even bypass doesn't silently rewrite your `.bashrc`).
/// 3. **Read-only bash auto-allow** for ~30 curated commands (`ls`,
///    `cat`, `git status`, …) — `Allow`.
/// 4. **`BypassPermissions` mode** — `Allow` (after step 1+2 short-circuit).
/// 5. **Per-tool default** policy (the tool registry says what each tool's
///    baseline is: `read_file` → Allow, `bash` → Ask, etc.).
///
/// Then a post-pass:
/// - If final is `Ask` and AFK is on → promote to `Allow` (matches Kimi
///   semantic; matches `BypassPermissions` once step 1+2 have already had
///   their say).
/// - If final is `Ask` and console-closed (no picker channel) → `Deny`
///   with a "no interactive console" reason.
pub fn check(
    tool_name: &str,
    arguments_json: &str,
    default_for_tool: Decision,
    mode: Mode,
    afk: bool,
) -> Decision {
    let args: serde_json::Value = serde_json::from_str(arguments_json).unwrap_or_default();

    // Step 1: circuit breakers (raw bash arg scan).
    if tool_name == "bash" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            if builtin::is_circuit_breaker(cmd) {
                let reason = format!(
                    "destructive command pattern matched ({}); requires explicit confirmation",
                    builtin::circuit_breaker_label(cmd).unwrap_or("circuit breaker")
                );
                return if afk {
                    Decision::deny(format!("AFK: {reason}. Toggle off and authorize manually."))
                } else {
                    Decision::ask(reason)
                };
            }
        }
    }

    // Step 2: protected-path edits (Ask under any mode — bypass included).
    if matches!(tool_name, "edit_file" | "create_file") {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            if builtin::is_protected_path(path) {
                let reason = format!("edit targets a protected path: {path}");
                return if afk {
                    Decision::deny(format!("AFK: {reason}. Toggle off and authorize manually."))
                } else {
                    Decision::ask(reason)
                };
            }
        }
    }

    // Step 3: read-only bash auto-allow.
    if tool_name == "bash" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            if builtin::is_read_only_bash(cmd) {
                return Decision::Allow;
            }
        }
    }

    // Step 4: bypass mode (after steps 1+2 short-circuit, this is just allow).
    if mode == Mode::BypassPermissions {
        return Decision::Allow;
    }

    // Step 5: per-tool default policy applies.
    let raw = default_for_tool;

    // AFK post-pass: an Ask becomes Allow under AFK (the picker would just
    // confirm, and no one is there to confirm). Deny stays Deny; Allow stays.
    match raw {
        Decision::Ask { .. } if afk => Decision::Allow,
        other => other,
    }
}

/// The per-tool default policy table. Returned at tool-registration time so
/// `check()` doesn't have to know every tool name.
pub fn default_policy_for_tool(tool_name: &str) -> Decision {
    match tool_name {
        // Pure reads.
        "read_file" | "list_dir" | "grep" | "glob" | "web_search" | "skill" => Decision::Allow,
        // ask_user is gated separately (AFK auto-dismisses inside the tool).
        "ask_user" => Decision::Allow,
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

    fn json(s: &str) -> &str {
        s
    }

    #[test]
    fn mode_parse_roundtrip() {
        assert_eq!(Mode::parse("default"), Some(Mode::Default));
        assert_eq!(
            Mode::parse("bypassPermissions"),
            Some(Mode::BypassPermissions)
        );
        assert_eq!(Mode::parse("bypass"), Some(Mode::BypassPermissions));
        assert_eq!(Mode::parse("nonsense"), None);
        assert_eq!(Mode::Default.as_str(), "default");
        assert_eq!(Mode::BypassPermissions.as_str(), "bypassPermissions");
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
    fn read_tool_in_default_mode_allows() {
        let d = check(
            "read_file",
            json(r#"{"path":"src/main.rs"}"#),
            default_policy_for_tool("read_file"),
            Mode::Default,
            false,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn bash_in_default_mode_asks() {
        let d = check(
            "bash",
            json(r#"{"command":"cargo build"}"#),
            default_policy_for_tool("bash"),
            Mode::Default,
            false,
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }

    #[test]
    fn bash_read_only_auto_allows_even_in_default() {
        // `git status` is on the auto-allow list.
        let d = check(
            "bash",
            json(r#"{"command":"git status"}"#),
            default_policy_for_tool("bash"),
            Mode::Default,
            false,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn bypass_mode_allows_normal_tools() {
        let d = check(
            "bash",
            json(r#"{"command":"cargo build && cargo test"}"#),
            default_policy_for_tool("bash"),
            Mode::BypassPermissions,
            false,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn bypass_mode_still_asks_circuit_breaker() {
        // `rm -rf /` always asks even under bypass (the floor of safety).
        for cmd in [
            r#"{"command":"rm -rf /"}"#,
            r#"{"command":"rm -rf ~"}"#,
            r#"{"command":"rm -rf $HOME"}"#,
            r#"{"command":"rm -rf \"$HOME\""}"#,
        ] {
            let d = check(
                "bash",
                cmd,
                default_policy_for_tool("bash"),
                Mode::BypassPermissions,
                false,
            );
            assert!(
                matches!(d, Decision::Ask { .. }),
                "expected Ask for circuit breaker, got {:?} on {}",
                d,
                cmd
            );
        }
    }

    #[test]
    fn bypass_mode_still_asks_protected_path_edit() {
        let d = check(
            "edit_file",
            json(r#"{"path":".git/config"}"#),
            default_policy_for_tool("edit_file"),
            Mode::BypassPermissions,
            false,
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }

    #[test]
    fn afk_promotes_ask_to_allow_for_normal_tools() {
        let d = check(
            "bash",
            json(r#"{"command":"cargo build"}"#),
            default_policy_for_tool("bash"),
            Mode::Default,
            true, // afk
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn afk_denies_circuit_breakers() {
        // AFK doesn't auto-approve `rm -rf /`; with no user to authorize
        // a circuit-breaker match, the safest action is to refuse.
        let d = check(
            "bash",
            json(r#"{"command":"rm -rf /"}"#),
            default_policy_for_tool("bash"),
            Mode::Default,
            true, // afk
        );
        assert!(
            matches!(d, Decision::Deny { ref reason } if reason.contains("AFK")),
            "expected AFK-Deny, got {:?}",
            d
        );
    }

    #[test]
    fn afk_denies_protected_path_edits() {
        let d = check(
            "edit_file",
            json(r#"{"path":".bashrc"}"#),
            default_policy_for_tool("edit_file"),
            Mode::Default,
            true, // afk
        );
        assert!(
            matches!(d, Decision::Deny { ref reason } if reason.contains("AFK")),
            "expected AFK-Deny, got {:?}",
            d
        );
    }

    #[test]
    fn malformed_args_does_not_panic() {
        // Missing 'command' field — defaults to no-circuit-breaker, no-read-only.
        // Bash still asks under default mode.
        let d = check(
            "bash",
            "not even json",
            default_policy_for_tool("bash"),
            Mode::Default,
            false,
        );
        assert!(matches!(d, Decision::Ask { .. }));
    }
}
