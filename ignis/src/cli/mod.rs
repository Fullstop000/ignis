//! Top-level CLI surface. Owned by clap (derive macros). Subcommand bodies live
//! in `mcp` and `upgrade` and are embedded as `Command` variants here; that
//! keeps all parsing — including `--help` and `--version` formatting — in one
//! place. The legacy hand-rolled parser and hand-typed help string were removed
//! in v0.15.1 (no behavior change beyond stricter validation of unknown flags).

use clap::{Parser, Subcommand};

use crate::session::SessionManager;

pub mod mcp;
pub mod upgrade;

#[derive(Parser, Debug)]
#[command(
    name = "ignis",
    version,
    about = "A multi-provider AI coding agent for your terminal.",
    long_about = "With no prompt, launches the interactive TUI; with a prompt, runs one-shot to stdout.",
    after_help = "Repo: https://github.com/Fullstop000/ignis",
    disable_help_subcommand = true,
    arg_required_else_help = false
)]
pub struct Cli {
    /// Resume the latest session, or the given session id.
    #[arg(long, num_args = 0..=1, value_name = "ID")]
    pub resume: Option<Option<String>>,

    /// Permission mode for this session: `default` (ask for sensitive tools)
    /// or `bypassPermissions` (auto-allow everything except circuit breakers
    /// + protected paths). Overrides the persisted setting in state.json.
    #[arg(long = "permission-mode", value_name = "MODE")]
    pub permission_mode: Option<String>,

    /// Enable AFK mode for this session — auto-approves tool calls and
    /// auto-dismisses `ask_user` with "Make your best judgment and proceed."
    /// Bypasses the `/afk` mid-session confirmation gate (typing the flag at
    /// launch is explicit intent). One-shot invocations (with a prompt arg)
    /// imply `--afk` automatically.
    #[arg(long)]
    pub afk: bool,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// The prompt to send (any non-flag args, joined). Prompt tokens that
    /// start with `-` must be escaped with `--`, e.g. `ignis -- "--debug fix"`.
    //
    // The benefit of strict parsing here (no `allow_hyphen_values`): a typo
    // like `ignis --resme work` surfaces as "unknown argument" with clap's
    // built-in suggestion instead of being silently sent as a one-shot prompt.
    #[arg(trailing_var_arg = true)]
    pub prompt: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Update ignis to the latest release.
    #[command(alias = "update")]
    Upgrade(upgrade::UpgradeCmd),
    /// Manage MCP servers (add, list, get, remove, enable, disable).
    Mcp(mcp::McpCmd),
}

impl Cli {
    /// Map clap's parse result into the small struct that `resolve_session_request`
    /// already knows how to consume — keeps the resolver and its tests stable.
    pub fn to_session_args(self) -> CliArgs {
        let (resume, resume_session_id) = match self.resume {
            None => (false, None),
            Some(None) => (true, None),
            Some(Some(id)) => (true, Some(id)),
        };
        CliArgs {
            resume,
            resume_session_id,
            prompt_args: self.prompt,
            permission_mode: self.permission_mode,
            afk: self.afk,
        }
    }
}

/// Resolver-side view of the CLI: the slice of state that picks a session id
/// and decides TUI vs. one-shot. Built by `Cli::to_session_args` in the
/// production path; constructed directly in tests.
#[derive(Debug, PartialEq, Default)]
pub struct CliArgs {
    pub resume: bool,
    pub resume_session_id: Option<String>,
    pub prompt_args: Vec<String>,
    /// `--permission-mode` value from CLI. Falls through to state.json default
    /// when None; rejected with a clear error if non-empty and unparseable.
    pub permission_mode: Option<String>,
    /// `--afk` from CLI. One-shot invocations also force this to `true`.
    pub afk: bool,
}

pub struct SessionRequest {
    pub session_id: String,
    /// `true` when the resolved request has no prompt — launches the TUI.
    pub is_tui: bool,
    pub prompt_args: Vec<String>,
}

pub fn resolve_session_request(
    cli: CliArgs,
    session_manager: &SessionManager,
    auto_resume: bool,
    cwd: &std::path::Path,
) -> SessionRequest {
    let prompt_args = cli.prompt_args;
    let cwd_str = cwd.to_string_lossy().to_string();

    let session_id = if cli.resume {
        if let Some(session_id) = cli.resume_session_id {
            session_id
        } else {
            session_manager
                .latest()
                .map(|s| s.id)
                .unwrap_or_else(SessionManager::create_id)
        }
    } else if auto_resume {
        // Find the latest session whose start_dir matches current cwd
        session_manager
            .list()
            .into_iter()
            .find(|s| s.start_dir.as_ref() == Some(&cwd_str))
            .map(|s| s.id)
            .unwrap_or_else(SessionManager::create_id)
    } else {
        SessionManager::create_id()
    };

    // No prompt → TUI. The dedicated `--tui` flag was removed in v0.15.0;
    // the no-arg invocation already covered the same intent.
    let is_tui = prompt_args.is_empty();

    SessionRequest {
        session_id,
        is_tui,
        prompt_args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(argv: &[&str]) -> Cli {
        // clap's argv[0] is the program name.
        let mut full = vec!["ignis"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(full).expect("parse")
    }

    #[test]
    fn no_args_resolves_to_tui_session_args() {
        let args = parse(&[]).to_session_args();
        assert!(!args.resume);
        assert!(args.resume_session_id.is_none());
        assert!(args.prompt_args.is_empty());
    }

    #[test]
    fn bare_resume_means_latest_session() {
        let args = parse(&["--resume"]).to_session_args();
        assert!(args.resume);
        assert!(args.resume_session_id.is_none());
        assert!(args.prompt_args.is_empty());
    }

    #[test]
    fn resume_with_id_and_prompt() {
        let args = parse(&["--resume", "work", "follow-up"]).to_session_args();
        assert!(args.resume);
        assert_eq!(args.resume_session_id.as_deref(), Some("work"));
        assert_eq!(args.prompt_args, vec!["follow-up"]);
    }

    #[test]
    fn oneshot_collects_trailing_args() {
        let args = parse(&["write", "a", "test"]).to_session_args();
        assert_eq!(args.prompt_args, vec!["write", "a", "test"]);
    }

    #[test]
    fn prompt_can_contain_hyphenated_tokens_after_dash_dash() {
        // The trailing prompt allows hyphen values; first-token `--something`
        // still needs `--` to escape clap's flag scan.
        let args = parse(&["--", "--debug", "fix"]).to_session_args();
        assert_eq!(args.prompt_args, vec!["--debug", "fix"]);
    }

    #[test]
    fn subcommand_takes_precedence_over_prompt() {
        let cli = Cli::try_parse_from(["ignis", "upgrade", "--check"]).expect("parse");
        assert!(matches!(cli.command, Some(Command::Upgrade(_))));
        assert!(cli.prompt.is_empty());
    }

    #[test]
    fn update_alias_routes_to_upgrade() {
        let cli = Cli::try_parse_from(["ignis", "update", "--check"]).expect("parse");
        assert!(matches!(cli.command, Some(Command::Upgrade(_))));
    }

    /// `ignis --resume <id> upgrade` — `--resume` greedily consumes `<id>`,
    /// then `upgrade` is in subcommand position and clap routes to it.
    /// Pins the behavior so it can't drift if we change parser config.
    #[test]
    fn resume_id_followed_by_subcommand_routes_to_subcommand() {
        let cli = Cli::try_parse_from(["ignis", "--resume", "work", "upgrade", "--check"])
            .expect("parse");
        assert_eq!(cli.resume, Some(Some("work".to_string())));
        assert!(matches!(cli.command, Some(Command::Upgrade(_))));
        assert!(cli.prompt.is_empty());
    }

    /// `ignis --resume upgrade` (no id) — clap consumes `upgrade` as the
    /// resume id (greedy `num_args = 0..=1`), so `command` is None and the
    /// session resumes a session literally called "upgrade". To launch the
    /// upgrade subcommand without a resume target, just drop `--resume`.
    #[test]
    fn bare_resume_then_subcommand_name_consumes_name_as_id() {
        let cli = Cli::try_parse_from(["ignis", "--resume", "upgrade"]).expect("parse");
        assert_eq!(cli.resume, Some(Some("upgrade".to_string())));
        assert!(cli.command.is_none());
    }

    #[test]
    fn unknown_top_level_flag_is_rejected() {
        // The hand-rolled parser used to silently push --foo into prompt_args;
        // clap now surfaces the typo as an error.
        let err = Cli::try_parse_from(["ignis", "--foo"]).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                clap::error::ErrorKind::UnknownArgument | clap::error::ErrorKind::InvalidSubcommand
            ),
            "expected UnknownArgument/InvalidSubcommand, got {:?}",
            err.kind()
        );
    }

    #[test]
    fn help_text_contains_top_level_surface() {
        // `--help` exits cleanly via clap's DisplayHelp kind; we render the
        // string into a buffer and inspect it.
        let err = Cli::try_parse_from(["ignis", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let rendered = err.render().to_string();
        assert!(rendered.contains("Usage: ignis"));
        assert!(rendered.contains("--resume"));
        assert!(rendered.contains("upgrade"));
        assert!(rendered.contains("mcp"));
        // Things we deliberately don't expose at the top level.
        assert!(!rendered.contains("--tui"));
        assert!(!rendered.contains("skills"));
    }

    #[test]
    fn resolve_session_request_uses_resume_session_id() {
        let dir = crate::util::unique_temp_dir("ignis-resume-session-id");
        std::fs::create_dir_all(&dir).unwrap();
        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse(&["--resume", "default"]).to_session_args(),
            &manager,
            false,
            std::path::Path::new("/tmp"),
        );

        assert_eq!(request.session_id, "default");
        assert!(request.is_tui);
        assert!(request.prompt_args.is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolve_session_request_supports_latest_resume_without_id() {
        let dir = crate::util::unique_temp_dir("ignis-resume-prompt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("default.jsonl"), "{}\n").unwrap();
        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse(&["--resume"]).to_session_args(),
            &manager,
            false,
            std::path::Path::new("/tmp"),
        );

        assert_eq!(request.session_id, "default");
        assert!(request.is_tui);
        assert!(request.prompt_args.is_empty());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolve_session_request_supports_resume_session_id_with_prompt() {
        let dir = crate::util::unique_temp_dir("ignis-resume-session-id-prompt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("work.jsonl"), "{}\n").unwrap();
        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse(&["--resume", "work", "hello"]).to_session_args(),
            &manager,
            false,
            std::path::Path::new("/tmp"),
        );

        assert_eq!(request.session_id, "work");
        assert!(!request.is_tui);
        assert_eq!(request.prompt_args, vec!["hello"]);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolve_session_request_defaults_to_default_session() {
        let dir = crate::util::unique_temp_dir("ignis-default-session");
        std::fs::create_dir_all(&dir).unwrap();
        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse(&["hello"]).to_session_args(),
            &manager,
            false,
            std::path::Path::new("/tmp"),
        );

        // With auto_resume=false, always create a new session
        assert!(!request.session_id.is_empty());
        assert!(!request.is_tui);
        assert_eq!(request.prompt_args, vec!["hello"]);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolve_session_request_auto_resume_matches_start_dir() {
        let dir = crate::util::unique_temp_dir("ignis-auto-resume-match");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("previous.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":1,"payload":{"id":"previous","start_dir":"/tmp"}}"#,
                "\n",
                r#"{"type":"message","timestamp":2,"payload":{"role":"user","content":"hello"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse(&[]).to_session_args(),
            &manager,
            true,
            std::path::Path::new("/tmp"),
        );

        assert_eq!(request.session_id, "previous");
        assert!(request.is_tui);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolve_session_request_auto_resume_creates_new_when_no_match() {
        let dir = crate::util::unique_temp_dir("ignis-auto-resume-no-match");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("other.jsonl"),
            concat!(
                r#"{"type":"session_meta","timestamp":1,"payload":{"id":"other","start_dir":"/other"}}"#,
                "\n",
                r#"{"type":"message","timestamp":2,"payload":{"role":"user","content":"hello"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse(&[]).to_session_args(),
            &manager,
            true,
            std::path::Path::new("/tmp"),
        );

        // No matching start_dir, should create a new session
        assert_ne!(request.session_id, "other");
        assert!(!request.session_id.is_empty());
        assert!(request.is_tui);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolve_session_request_auto_resume_prefers_latest_match() {
        let dir = crate::util::unique_temp_dir("ignis-auto-resume-latest");
        std::fs::create_dir_all(&dir).unwrap();
        // older session — set mtime to 2 seconds ago so it's definitely older
        let older_path = dir.join("older.jsonl");
        std::fs::write(
            &older_path,
            concat!(
                r#"{"type":"session_meta","timestamp":1,"payload":{"id":"older","start_dir":"/tmp"}}"#,
                "\n",
                r#"{"type":"message","timestamp":2,"payload":{"role":"user","content":"older"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let older_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&older_path)
            .unwrap();
        let times = std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2));
        older_file.set_times(times).unwrap();

        // newer session
        let newer_path = dir.join("newer.jsonl");
        std::fs::write(
            &newer_path,
            concat!(
                r#"{"type":"session_meta","timestamp":1,"payload":{"id":"newer","start_dir":"/tmp"}}"#,
                "\n",
                r#"{"type":"message","timestamp":2,"payload":{"role":"user","content":"newer"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse(&[]).to_session_args(),
            &manager,
            true,
            std::path::Path::new("/tmp"),
        );

        assert_eq!(request.session_id, "newer");

        std::fs::remove_dir_all(dir).unwrap();
    }
}
