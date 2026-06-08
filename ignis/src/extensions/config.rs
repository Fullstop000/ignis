//! `~/.ignis/hooks.json` schema + loader.
//!
//! The file is optional. Absence = no hooks, no log noise. A malformed file
//! is a startup error — ignis aborts before the first prompt, mirroring the
//! posture for a broken `config.toml` (loud, not silent).
//!
//! v2 adds:
//! - 6 new events (`PreToolUse`, `PostToolUse`, `PreCompact`, `PostCompact`,
//!   `SessionStart`, `Stop`, `SystemPromptCompose`) — see [`ExtensionEvent`].
//! - A `matcher` field (regex on `tool_name`) compiled at load. Malformed
//!   regex is a startup error. Declaring `matcher` on a non-tool event is
//!   not a startup error but is reported once via `[warn]` when the
//!   registry loads — silently ignoring would mask a config mistake.
//!
//! v1 configs parse unchanged.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde::Deserialize;

use super::protocol::ExtensionEvent;

/// Default per-hook timeout when `timeout_ms` is omitted. 10s — comfortably
/// covers p99 of a healthy Haiku call; hooks doing heavier work declare a
/// larger budget explicitly.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// One declared hook: how to spawn it, how long to wait, and (optionally)
/// which tool names it applies to.
#[derive(Debug, Clone)]
pub struct ExtensionSpec {
    /// Executable path (post-`~` expansion).
    pub program: PathBuf,
    /// argv tail (everything after the program, whitespace-split, no shell
    /// interpolation).
    pub args: Vec<String>,
    pub timeout_ms: u64,
    /// Tool-name filter. Compiled at parse so a malformed pattern is a
    /// startup error rather than a per-call surprise. Meaningful only for
    /// `PreToolUse` / `PostToolUse` (see [`ExtensionEvent::uses_tool_matcher`]).
    pub matcher: Option<ExtensionMatcher>,
}

/// Tool-name regex paired with the source pattern. The pattern is kept
/// for equality + display; the compiled regex is what `matches()` uses.
#[derive(Debug, Clone)]
pub struct ExtensionMatcher {
    /// Raw pattern as it appeared in `hooks.json`. Used for equality,
    /// the `/hooks` listing, and `[warn]` lines.
    pub raw: String,
    /// Compiled at parse — startup error on malformed regex.
    pub re: Regex,
}

impl ExtensionMatcher {
    pub fn matches(&self, tool_name: &str) -> bool {
        self.re.is_match(tool_name)
    }
}

impl PartialEq for ExtensionSpec {
    /// Equality compares the raw matcher pattern, not the compiled regex
    /// (`Regex` doesn't implement `Eq`). The compiled form derives from
    /// the raw — comparing one implies comparing the other.
    fn eq(&self, other: &Self) -> bool {
        self.program == other.program
            && self.args == other.args
            && self.timeout_ms == other.timeout_ms
            && self.matcher.as_ref().map(|m| &m.raw) == other.matcher.as_ref().map(|m| &m.raw)
    }
}
impl Eq for ExtensionSpec {}

impl ExtensionSpec {
    /// Short, log-friendly identifier used in `[warn]` / `[info]` lines and
    /// the `· hook: <name>…` footer. The file stem of the program (no
    /// directory, no extension).
    pub fn display_name(&self) -> String {
        self.program
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.program.to_string_lossy().to_string())
    }

    /// True when this hook should run for the given tool name. Hooks
    /// without a matcher always match (the pre-v2 default).
    pub fn applies_to_tool(&self, tool_name: &str) -> bool {
        self.matcher
            .as_ref()
            .map(|m| m.matches(tool_name))
            .unwrap_or(true)
    }
}

/// Parsed `hooks.json` keyed by event. v2 carries one `Vec<ExtensionSpec>` per
/// declared `ExtensionEvent`. Adding a new event extends this struct + the
/// `for_event` match below + the parser's name table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionsConfig {
    pub user_prompt_submit: Vec<ExtensionSpec>,
    pub assistant_message_render: Vec<ExtensionSpec>,
    pub system_prompt_compose: Vec<ExtensionSpec>,
    pub pre_tool_use: Vec<ExtensionSpec>,
    pub post_tool_use: Vec<ExtensionSpec>,
    pub pre_compact: Vec<ExtensionSpec>,
    pub post_compact: Vec<ExtensionSpec>,
    pub session_start: Vec<ExtensionSpec>,
    pub stop: Vec<ExtensionSpec>,
}

impl ExtensionsConfig {
    pub fn is_empty(&self) -> bool {
        ExtensionEvent::ALL
            .iter()
            .all(|ev| self.for_event(*ev).is_empty())
    }

    pub fn total_len(&self) -> usize {
        ExtensionEvent::ALL
            .iter()
            .map(|ev| self.for_event(*ev).len())
            .sum()
    }

    pub fn for_event(&self, event: ExtensionEvent) -> &[ExtensionSpec] {
        match event {
            ExtensionEvent::UserPromptSubmit => &self.user_prompt_submit,
            ExtensionEvent::AssistantMessageRender => &self.assistant_message_render,
            ExtensionEvent::SystemPromptCompose => &self.system_prompt_compose,
            ExtensionEvent::PreToolUse => &self.pre_tool_use,
            ExtensionEvent::PostToolUse => &self.post_tool_use,
            ExtensionEvent::PreCompact => &self.pre_compact,
            ExtensionEvent::PostCompact => &self.post_compact,
            ExtensionEvent::SessionStart => &self.session_start,
            ExtensionEvent::Stop => &self.stop,
        }
    }

    fn bucket_mut(&mut self, event: ExtensionEvent) -> &mut Vec<ExtensionSpec> {
        match event {
            ExtensionEvent::UserPromptSubmit => &mut self.user_prompt_submit,
            ExtensionEvent::AssistantMessageRender => &mut self.assistant_message_render,
            ExtensionEvent::SystemPromptCompose => &mut self.system_prompt_compose,
            ExtensionEvent::PreToolUse => &mut self.pre_tool_use,
            ExtensionEvent::PostToolUse => &mut self.post_tool_use,
            ExtensionEvent::PreCompact => &mut self.pre_compact,
            ExtensionEvent::PostCompact => &mut self.post_compact,
            ExtensionEvent::SessionStart => &mut self.session_start,
            ExtensionEvent::Stop => &mut self.stop,
        }
    }

    /// Load from `<home>/.ignis/extensions.json` (preferred). Falls back
    /// to the legacy `<home>/.ignis/hooks.json` if the new name doesn't
    /// exist — v1 configs keep working byte-for-byte. Returns
    /// `Ok(default)` when neither file is present. Returns `Err` on
    /// parse failure or invalid entry — the caller (`Session::open`)
    /// bubbles that up to startup.
    pub fn from_home(home: &Path) -> Result<Self> {
        let preferred = home.join(".ignis").join("extensions.json");
        let legacy = home.join(".ignis").join("hooks.json");
        let path = if preferred.exists() {
            preferred
        } else if legacy.exists() {
            legacy
        } else {
            return Ok(ExtensionsConfig::default());
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_str(&raw, home).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parse the raw JSON. `home` is used to expand a leading `~/` in
    /// command strings. Accepts either `{"extensions": {...}}` (v2
    /// preferred) or `{"hooks": {...}}` (v1 back-compat) as the top-
    /// level key.
    pub fn from_str(raw: &str, home: &Path) -> Result<Self> {
        let parsed: ExtensionsJson =
            serde_json::from_str(raw).context("extensions.json is not valid JSON")?;
        let entries = match (parsed.extensions, parsed.hooks) {
            (Some(e), _) => e,
            (None, Some(h)) => h,
            (None, None) => std::collections::BTreeMap::new(),
        };
        let mut out = ExtensionsConfig::default();
        for (event_name, entries) in entries {
            let event = match parse_event_name(&event_name) {
                Some(ev) => ev,
                None => {
                    // Forward-compat: unknown events ignored with a warning.
                    // Lets a single extensions.json work across ignis versions.
                    tracing::warn!(event = %event_name, "extensions.json: ignoring unknown event");
                    continue;
                }
            };
            for entry in entries {
                let spec = parse_entry(entry, home)
                    .with_context(|| format!("invalid extension entry under `{event_name}`"))?;
                out.bucket_mut(event).push(spec);
            }
        }
        Ok(out)
    }

    /// Per-hook check used by the registry at load time to surface the
    /// "matcher declared on a non-tool event" warning. Returns `(event,
    /// display_name, raw_pattern)` for every offending spec. Empty when
    /// the config is well-formed for tool-event semantics.
    pub fn non_tool_matchers(&self) -> Vec<(ExtensionEvent, String, String)> {
        let mut out = Vec::new();
        for ev in ExtensionEvent::ALL {
            if ev.uses_tool_matcher() {
                continue;
            }
            for spec in self.for_event(*ev) {
                if let Some(m) = &spec.matcher {
                    out.push((*ev, spec.display_name(), m.raw.clone()));
                }
            }
        }
        out
    }
}

fn parse_event_name(s: &str) -> Option<ExtensionEvent> {
    match s {
        "UserPromptSubmit" => Some(ExtensionEvent::UserPromptSubmit),
        "AssistantMessageRender" => Some(ExtensionEvent::AssistantMessageRender),
        "SystemPromptCompose" => Some(ExtensionEvent::SystemPromptCompose),
        "PreToolUse" => Some(ExtensionEvent::PreToolUse),
        "PostToolUse" => Some(ExtensionEvent::PostToolUse),
        "PreCompact" => Some(ExtensionEvent::PreCompact),
        "PostCompact" => Some(ExtensionEvent::PostCompact),
        "SessionStart" => Some(ExtensionEvent::SessionStart),
        "Stop" => Some(ExtensionEvent::Stop),
        _ => None,
    }
}

fn parse_entry(entry: ExtensionJsonEntry, home: &Path) -> Result<ExtensionSpec> {
    let timeout_ms = entry.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    // Compile matcher at parse so a malformed regex is a startup error,
    // not a per-call surprise.
    let matcher = entry
        .matcher
        .as_deref()
        .map(|raw| -> Result<ExtensionMatcher> {
            let re = Regex::new(raw).with_context(|| format!("invalid `matcher` regex `{raw}`"))?;
            Ok(ExtensionMatcher {
                raw: raw.to_string(),
                re,
            })
        })
        .transpose()?;
    // Mutual exclusion: pick exactly one of `command` (single string,
    // whitespace-split — simple default) or `argv` (pre-tokenised, supports
    // paths-with-spaces — escape hatch).
    let (program, args) = match (entry.command.as_deref(), entry.argv.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(anyhow!(
                "hook entry has both `command` and `argv`; use exactly one"
            ));
        }
        (None, None) => return Err(anyhow!("hook entry has neither `command` nor `argv`")),
        (Some(command), None) => {
            let command = command.trim();
            if command.is_empty() {
                return Err(anyhow!("hook entry has empty `command`"));
            }
            let mut parts = command.split_whitespace();
            let program_raw = parts
                .next()
                .ok_or_else(|| anyhow!("hook entry has empty `command`"))?;
            let program = expand_home(program_raw, home);
            let args: Vec<String> = parts.map(|s| s.to_string()).collect();
            (program, args)
        }
        (None, Some(argv)) => {
            let mut iter = argv.iter();
            let program_raw = iter
                .next()
                .ok_or_else(|| anyhow!("hook entry `argv` is empty"))?;
            if program_raw.trim().is_empty() {
                return Err(anyhow!("hook entry `argv[0]` is empty"));
            }
            let program = expand_home(program_raw, home);
            let args: Vec<String> = iter.cloned().collect();
            (program, args)
        }
    };
    Ok(ExtensionSpec {
        program,
        args,
        timeout_ms,
        matcher,
    })
}

fn expand_home(s: &str, home: &Path) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else if s == "~" {
        home.to_path_buf()
    } else {
        PathBuf::from(s)
    }
}

#[derive(Debug, Deserialize)]
struct ExtensionsJson {
    /// v2 preferred key.
    #[serde(default)]
    extensions: Option<std::collections::BTreeMap<String, Vec<ExtensionJsonEntry>>>,
    /// v1 legacy key. Read for back-compat; ignored if `extensions` is
    /// also present.
    #[serde(default)]
    hooks: Option<std::collections::BTreeMap<String, Vec<ExtensionJsonEntry>>>,
}

#[derive(Debug, Deserialize)]
struct ExtensionJsonEntry {
    /// Simple form: a single string split on whitespace, no shell. The
    /// program is the first token; subsequent tokens are argv. No
    /// `$VAR` expansion; only a leading `~/` is expanded against home.
    /// Cannot represent a program path containing whitespace.
    #[serde(default)]
    command: Option<String>,
    /// Explicit form: a pre-tokenised argv list. `argv[0]` is the
    /// program; the rest are args. Use this when the program path
    /// contains whitespace. Mutually exclusive with `command`.
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// v2: regex on the tool name. Meaningful only for `PreToolUse` and
    /// `PostToolUse`; declaring it elsewhere triggers a `[warn]` at load.
    #[serde(default)]
    matcher: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1 wire shape: no `matcher`, no per-event fields beyond v1's two.
    /// This test pins back-compat — a config written for v1 must parse
    /// unchanged in v2.
    #[test]
    fn v1_config_parses_unchanged_in_v2() {
        let home = PathBuf::from("/home/me");
        let raw = r#"{
            "hooks": {
                "UserPromptSubmit": [
                    {"command": "~/.ignis/hooks/translate-en/run.py"}
                ],
                "AssistantMessageRender": [
                    {"command": "~/.ignis/hooks/translate-en/run.py", "timeout_ms": 30000}
                ]
            }
        }"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        assert_eq!(cfg.user_prompt_submit.len(), 1);
        assert!(cfg.user_prompt_submit[0].matcher.is_none());
        assert_eq!(cfg.assistant_message_render[0].timeout_ms, 30_000);
        // The 6 new event chains are all empty.
        assert!(cfg.pre_tool_use.is_empty());
        assert!(cfg.post_tool_use.is_empty());
        assert!(cfg.pre_compact.is_empty());
        assert!(cfg.post_compact.is_empty());
        assert!(cfg.session_start.is_empty());
        assert!(cfg.stop.is_empty());
        assert!(cfg.system_prompt_compose.is_empty());
    }

    #[test]
    fn v2_pre_tool_use_with_matcher_compiles() {
        let home = PathBuf::from("/h");
        let raw = r#"{
            "hooks": {
                "PreToolUse": [
                    {"command": "/bin/true", "matcher": "Bash|Edit"}
                ]
            }
        }"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        let spec = &cfg.pre_tool_use[0];
        let m = spec.matcher.as_ref().expect("matcher compiled");
        assert_eq!(m.raw, "Bash|Edit");
        assert!(m.matches("Bash"));
        assert!(m.matches("Edit"));
        assert!(!m.matches("Read"));
    }

    #[test]
    fn malformed_matcher_is_startup_error() {
        let home = PathBuf::from("/h");
        // Unbalanced bracket — regex::Regex::new fails.
        let raw = r#"{
            "hooks": {"PreToolUse": [{"command": "/bin/true", "matcher": "[unbalanced"}]}
        }"#;
        let err = ExtensionsConfig::from_str(raw, &home).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("matcher"), "got: {chain}");
        assert!(chain.contains("[unbalanced"), "got: {chain}");
    }

    #[test]
    fn applies_to_tool_default_when_no_matcher() {
        let spec = ExtensionSpec {
            program: PathBuf::from("/bin/true"),
            args: vec![],
            timeout_ms: 1000,
            matcher: None,
        };
        // No matcher = applies to every tool.
        assert!(spec.applies_to_tool("Bash"));
        assert!(spec.applies_to_tool("Write"));
        assert!(spec.applies_to_tool(""));
    }

    #[test]
    fn non_tool_matchers_returns_offending_specs() {
        // A matcher on `UserPromptSubmit` is silently dropped from
        // behavior but flagged here so the registry can warn at load.
        let home = PathBuf::from("/h");
        let raw = r#"{
            "hooks": {
                "UserPromptSubmit": [{"command": "/bin/true", "matcher": "Bash"}],
                "PreToolUse": [{"command": "/bin/true", "matcher": "Bash"}]
            }
        }"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        let warnings = cfg.non_tool_matchers();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].0, ExtensionEvent::UserPromptSubmit);
        assert_eq!(warnings[0].2, "Bash");
    }

    #[test]
    fn parses_all_v2_event_names() {
        // Pin that every name in ExtensionEvent::ALL is parsable. A typo'd
        // arm in parse_event_name would silently drop one event class
        // and reach the "unknown event" warn branch.
        let home = PathBuf::from("/h");
        let mut raw = String::from("{\"hooks\":{");
        let mut first = true;
        for ev in ExtensionEvent::ALL {
            if !first {
                raw.push(',');
            }
            first = false;
            raw.push_str(&format!(
                "\"{}\":[{{\"command\":\"/bin/true\"}}]",
                ev.as_str()
            ));
        }
        raw.push_str("}}");
        let cfg = ExtensionsConfig::from_str(&raw, &home).unwrap();
        assert_eq!(cfg.total_len(), ExtensionEvent::ALL.len());
        for ev in ExtensionEvent::ALL {
            assert_eq!(cfg.for_event(*ev).len(), 1, "missing: {}", ev.as_str());
        }
    }

    #[test]
    fn argv_split_on_whitespace_no_shell() {
        let home = PathBuf::from("/h");
        let raw = r#"{
            "hooks": {
                "UserPromptSubmit": [
                    {"command": "/usr/bin/env python3 /opt/run.py --display"}
                ]
            }
        }"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        let spec = &cfg.user_prompt_submit[0];
        assert_eq!(spec.program, PathBuf::from("/usr/bin/env"));
        assert_eq!(spec.args, vec!["python3", "/opt/run.py", "--display"]);
    }

    #[test]
    fn absent_file_is_ok_empty() {
        let tmp = crate::util::unique_temp_dir("ignis-hooks-absent");
        let cfg = ExtensionsConfig::from_home(&tmp).unwrap();
        assert!(cfg.is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn malformed_json_is_an_error() {
        let home = PathBuf::from("/h");
        let err = ExtensionsConfig::from_str("{not json}", &home).unwrap_err();
        assert!(err.to_string().contains("extensions.json"));
    }

    #[test]
    fn unknown_events_are_ignored() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"FutureSomething": [{"command": "/bin/true"}]}}"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn empty_command_rejected() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{"command": "   "}]}}"#;
        let err = ExtensionsConfig::from_str(raw, &home).unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn command_with_space_in_program_path_silently_truncates_in_simple_form() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{"command": "/path with space/run.py"}]}}"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        let spec = &cfg.user_prompt_submit[0];
        assert_eq!(spec.program, PathBuf::from("/path"));
        assert_eq!(spec.args, vec!["with", "space/run.py"]);
    }

    #[test]
    fn argv_form_preserves_program_path_with_spaces() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [
            {"argv": ["/path with space/run.py", "--display"]}
        ]}}"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        let spec = &cfg.user_prompt_submit[0];
        assert_eq!(spec.program, PathBuf::from("/path with space/run.py"));
        assert_eq!(spec.args, vec!["--display"]);
    }

    #[test]
    fn argv_form_supports_tilde_expansion_on_program() {
        let home = PathBuf::from("/home/me");
        let raw = r#"{"hooks": {"UserPromptSubmit": [
            {"argv": ["~/.ignis/hooks/run.py"]}
        ]}}"#;
        let cfg = ExtensionsConfig::from_str(raw, &home).unwrap();
        assert_eq!(
            cfg.user_prompt_submit[0].program,
            home.join(".ignis/hooks/run.py")
        );
    }

    #[test]
    fn both_command_and_argv_rejected() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [
            {"command": "/bin/true", "argv": ["/bin/true"]}
        ]}}"#;
        let err = ExtensionsConfig::from_str(raw, &home).unwrap_err();
        assert!(format!("{err:#}").contains("both"));
    }

    #[test]
    fn neither_command_nor_argv_rejected() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{}]}}"#;
        let err = ExtensionsConfig::from_str(raw, &home).unwrap_err();
        assert!(format!("{err:#}").contains("neither"));
    }

    #[test]
    fn empty_argv_rejected() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{"argv": []}]}}"#;
        let err = ExtensionsConfig::from_str(raw, &home).unwrap_err();
        assert!(format!("{err:#}").contains("empty"));
    }

    #[test]
    fn display_name_strips_directory_and_extension() {
        let spec = ExtensionSpec {
            program: PathBuf::from("/home/me/.ignis/hooks/translate-en/run.py"),
            args: vec![],
            timeout_ms: 1,
            matcher: None,
        };
        assert_eq!(spec.display_name(), "run");
    }
}
