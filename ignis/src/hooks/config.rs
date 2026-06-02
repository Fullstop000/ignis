//! `~/.ignis/hooks.json` schema + loader.
//!
//! The file is optional. Absence = no hooks, no log noise. A malformed file
//! is a startup error — ignis aborts before the first prompt, mirroring the
//! posture for a broken `config.toml` (loud, not silent).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use super::protocol::HookEvent;

/// Default per-hook timeout when `timeout_ms` is omitted. 10s — comfortably
/// covers p99 of a healthy Haiku call; hooks doing heavier work declare a
/// larger budget explicitly.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// One declared hook: how to spawn it, and how long to wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    /// Executable path (post-`~` expansion).
    pub program: PathBuf,
    /// argv tail (everything after the program, whitespace-split, no shell
    /// interpolation).
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

impl HookSpec {
    /// Short, log-friendly identifier used in `[warn]` / `[info]` lines and
    /// the `· hook: <name>…` footer. The file stem of the program (no
    /// directory, no extension) — long enough to disambiguate when several
    /// hooks live in the same parent, short enough to fit a status line.
    pub fn display_name(&self) -> String {
        self.program
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.program.to_string_lossy().to_string())
    }
}

/// Parsed `hooks.json` keyed by event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HooksConfig {
    pub user_prompt_submit: Vec<HookSpec>,
    pub assistant_message_render: Vec<HookSpec>,
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.user_prompt_submit.is_empty() && self.assistant_message_render.is_empty()
    }

    pub fn total_len(&self) -> usize {
        self.user_prompt_submit.len() + self.assistant_message_render.len()
    }

    pub fn for_event(&self, event: HookEvent) -> &[HookSpec] {
        match event {
            HookEvent::UserPromptSubmit => &self.user_prompt_submit,
            HookEvent::AssistantMessageRender => &self.assistant_message_render,
        }
    }

    /// Load from `<home>/.ignis/hooks.json`. Returns `Ok(default)` when the
    /// file is absent. Returns `Err` on parse failure or invalid entry — the
    /// caller (Session::open) bubbles that up to startup.
    pub fn from_home(home: &Path) -> Result<Self> {
        let path = home.join(".ignis").join("hooks.json");
        if !path.exists() {
            return Ok(HooksConfig::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_str(&raw, home).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parse the raw JSON. `home` is used to expand a leading `~/` in
    /// command strings.
    pub fn from_str(raw: &str, home: &Path) -> Result<Self> {
        let parsed: HooksJson =
            serde_json::from_str(raw).context("hooks.json is not valid JSON")?;
        let mut out = HooksConfig::default();
        for (event_name, entries) in parsed.hooks {
            let event = match event_name.as_str() {
                "UserPromptSubmit" => HookEvent::UserPromptSubmit,
                "AssistantMessageRender" => HookEvent::AssistantMessageRender,
                other => {
                    // Forward-compat: unknown events ignored with a warning.
                    // Lets a single hooks.json work across ignis versions.
                    tracing::warn!(event = %other, "hooks.json: ignoring unknown event");
                    continue;
                }
            };
            let bucket = match event {
                HookEvent::UserPromptSubmit => &mut out.user_prompt_submit,
                HookEvent::AssistantMessageRender => &mut out.assistant_message_render,
            };
            for entry in entries {
                bucket.push(parse_entry(entry, home)?);
            }
        }
        Ok(out)
    }
}

fn parse_entry(entry: HookJsonEntry, home: &Path) -> Result<HookSpec> {
    let timeout_ms = entry.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    // Mutual exclusion: pick exactly one of `command` (single string,
    // whitespace-split — simple default) or `argv` (pre-tokenised, supports
    // paths-with-spaces — escape hatch).
    match (entry.command.as_deref(), entry.argv.as_deref()) {
        (Some(_), Some(_)) => Err(anyhow!(
            "hook entry has both `command` and `argv`; use exactly one"
        )),
        (None, None) => Err(anyhow!(
            "hook entry has neither `command` nor `argv`"
        )),
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
            Ok(HookSpec {
                program,
                args,
                timeout_ms,
            })
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
            Ok(HookSpec {
                program,
                args,
                timeout_ms,
            })
        }
    }
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
struct HooksJson {
    #[serde(default)]
    hooks: std::collections::BTreeMap<String, Vec<HookJsonEntry>>,
}

#[derive(Debug, Deserialize)]
struct HookJsonEntry {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_events_with_tilde_expansion() {
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
        let cfg = HooksConfig::from_str(raw, &home).unwrap();
        assert_eq!(cfg.user_prompt_submit.len(), 1);
        assert_eq!(
            cfg.user_prompt_submit[0].program,
            home.join(".ignis/hooks/translate-en/run.py")
        );
        assert_eq!(cfg.user_prompt_submit[0].timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(cfg.assistant_message_render[0].timeout_ms, 30_000);
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
        let cfg = HooksConfig::from_str(raw, &home).unwrap();
        let spec = &cfg.user_prompt_submit[0];
        assert_eq!(spec.program, PathBuf::from("/usr/bin/env"));
        assert_eq!(spec.args, vec!["python3", "/opt/run.py", "--display"]);
    }

    #[test]
    fn absent_file_is_ok_empty() {
        let tmp = crate::util::unique_temp_dir("ignis-hooks-absent");
        let cfg = HooksConfig::from_home(&tmp).unwrap();
        assert!(cfg.is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn malformed_json_is_an_error() {
        let home = PathBuf::from("/h");
        let err = HooksConfig::from_str("{not json}", &home).unwrap_err();
        assert!(err.to_string().contains("hooks.json"));
    }

    #[test]
    fn unknown_events_are_ignored() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"FuturePostToolUse": [{"command": "/bin/true"}]}}"#;
        let cfg = HooksConfig::from_str(raw, &home).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn empty_command_rejected() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{"command": "   "}]}}"#;
        let err = HooksConfig::from_str(raw, &home).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn command_with_space_in_program_path_silently_truncates_in_simple_form() {
        // Documented limitation of the simple form: whitespace-split treats
        // "/path with" as the program and "space/run.py" as the first arg.
        // The escape hatch is the `argv` form below — this test pins the
        // current behaviour so a future change is intentional.
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{"command": "/path with space/run.py"}]}}"#;
        let cfg = HooksConfig::from_str(raw, &home).unwrap();
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
        let cfg = HooksConfig::from_str(raw, &home).unwrap();
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
        let cfg = HooksConfig::from_str(raw, &home).unwrap();
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
        let err = HooksConfig::from_str(raw, &home).unwrap_err();
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn neither_command_nor_argv_rejected() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{}]}}"#;
        let err = HooksConfig::from_str(raw, &home).unwrap_err();
        assert!(err.to_string().contains("neither"));
    }

    #[test]
    fn empty_argv_rejected() {
        let home = PathBuf::from("/h");
        let raw = r#"{"hooks": {"UserPromptSubmit": [{"argv": []}]}}"#;
        let err = HooksConfig::from_str(raw, &home).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn display_name_strips_directory_and_extension() {
        let spec = HookSpec {
            program: PathBuf::from("/home/me/.ignis/hooks/translate-en/run.py"),
            args: vec![],
            timeout_ms: 1,
        };
        assert_eq!(spec.display_name(), "run");
    }
}
