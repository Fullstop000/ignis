//! Top-level CLI surface. Owned by clap (derive macros). Subcommand bodies live
//! in `mcp` and `upgrade` and are embedded as `Command` variants here; that
//! keeps all parsing — including `--help` and `--version` formatting — in one
//! place. The legacy hand-rolled parser and hand-typed help string were removed
//! in v0.15.1 (no behavior change beyond stricter validation of unknown flags).

use clap::{Parser, Subcommand};

use crate::session::SessionManager;

pub mod mcp;
pub mod sessions;
pub mod upgrade;

#[derive(Parser, Debug)]
#[command(
    name = "ignis",
    version,
    about = "A multi-provider AI coding agent for your terminal.",
    long_about = "With no prompt, launches the interactive TUI; with a prompt, runs one-shot to stdout.",
    after_help = "Repo: https://github.com/Fullstop000/ignis",
    disable_help_subcommand = true,
    disable_version_flag = true,
    arg_required_else_help = false
)]
pub struct Cli {
    /// Print version
    #[arg(short = 'v', long = "version", action = clap::ArgAction::Version)]
    pub version: (),

    /// Resume the latest session, or the given session id.
    #[arg(short = 'r', long, num_args = 0..=1, value_name = "ID")]
    pub resume: Option<Option<String>>,

    /// Enable AFK mode (fully unattended) for this session — auto-approves
    /// every tool call, auto-dismisses `ask_user`, and hard-denies the safety
    /// floor (`rm -rf /` family, `.git`/`.ignis`/shell-init edits). Bypasses
    /// the `/afk` confirmation picker (typing the flag at launch is explicit
    /// intent). One-shot invocations imply `--afk` automatically.
    #[arg(long)]
    pub afk: bool,

    /// Run as a headless protocol engine over stdin/stdout (NDJSON) with no
    /// terminal UI — driven by an out-of-process frontend (the Ink `ignis-tui`)
    /// that owns the terminal and spawns this process. Experimental.
    #[arg(long, hide = true)]
    pub engine: bool,

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
    /// Inspect or export session history.
    Sessions(sessions::SessionsCmd),
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

/// Which interactive frontend to launch for TUI mode (PR #174, topology ii).
#[derive(Debug, PartialEq, Eq)]
pub enum Frontend {
    /// The built-in ratatui TUI, rendered in-process. The default and the
    /// fallback — always available, no external runtime.
    Ratatui,
    /// The out-of-process Ink frontend at `entry` (a `node` script). It owns the
    /// terminal and spawns this binary as `--engine`. Opt-in until it reaches
    /// parity with ratatui.
    Ink { entry: String },
}

/// Resolve the frontend choice. When an Ink `entry` is available, Ink is the
/// default — `IGNIS_FRONTEND` unset or `=ink` both select it. `IGNIS_FRONTEND`
/// of `native`/`ratatui`/`tui` forces the built-in TUI even when Ink is present.
/// With no entry — released binaries ship no `ignis-tui` — we always fall back to
/// ratatui, so Ink only engages where its JS is actually located.
pub fn resolve_frontend(frontend: Option<&str>, entry: Option<&str>) -> Frontend {
    // Explicit opt-out always wins, even when an Ink entry exists.
    if matches!(frontend, Some("native") | Some("ratatui") | Some("tui")) {
        return Frontend::Ratatui;
    }
    match entry {
        Some(e) if !e.trim().is_empty() => Frontend::Ink {
            entry: e.to_string(),
        },
        _ => Frontend::Ratatui,
    }
}

/// A bundled `ignis-tui` is only *runnable* if its deps are installed: a bare
/// source checkout (or a fresh CI clone) has `src/cli.js` but no `node_modules`,
/// and launching it would crash on a missing `ink` import instead of falling back
/// to the built-in TUI. Require both so "Ink is available" stays honest. Installs
/// always bundle `node_modules`, so this only filters out un-`npm install`ed
/// checkouts.
fn runnable_ink_entry(tui_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let cli = tui_dir.join("src").join("cli.js");
    let deps = tui_dir.join("node_modules");
    (cli.is_file() && deps.is_dir()).then_some(cli)
}

/// Walk up from `start` (inclusive), returning the first runnable `ignis-tui`
/// found. `cargo run` puts the binary under `target/<profile>/`, two levels below
/// the repo root that holds `ignis-tui/`, so a shallow walk finds it.
fn find_ink_entry_from(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = Some(start);
    for _ in 0..6 {
        let d = dir?;
        if let Some(cli) = runnable_ink_entry(&d.join("ignis-tui")) {
            return Some(cli);
        }
        dir = d.parent();
    }
    None
}

/// The Ink entry an `install.sh` / `ignis upgrade` install lays down. The JS
/// ships to `~/.ignis/ignis-tui/` regardless of where the binary itself was
/// installed (the install dir is configurable), so this is checked independently
/// of the executable's location.
fn ink_entry_in_ignis_home(home: &std::path::Path) -> Option<std::path::PathBuf> {
    runnable_ink_entry(&home.join(".ignis").join("ignis-tui"))
}

/// Locate the Ink frontend entry script. Order: an explicit non-empty
/// `IGNIS_TUI_ENTRY` wins; then the source-layout `ignis-tui/src/cli.js` found by
/// walking up from the running binary (`cargo run`); then the installed copy at
/// `~/.ignis/ignis-tui/src/cli.js` that releases lay down. Returns `None` when
/// none exist, so the caller falls back to the built-in TUI.
pub fn locate_ink_entry(explicit: Option<&str>) -> Option<String> {
    if let Some(e) = explicit {
        if !e.trim().is_empty() {
            return Some(e.to_string());
        }
    }
    if let Some(found) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().and_then(find_ink_entry_from))
    {
        return Some(found.to_string_lossy().into_owned());
    }
    dirs::home_dir()
        .and_then(|h| ink_entry_in_ignis_home(&h))
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn ink_is_the_default_when_an_entry_is_available() {
        let ink = Frontend::Ink {
            entry: "/path/cli.js".to_string(),
        };
        // No entry → ratatui no matter what IGNIS_FRONTEND says.
        assert_eq!(resolve_frontend(None, None), Frontend::Ratatui);
        assert_eq!(resolve_frontend(Some("ink"), None), Frontend::Ratatui);
        assert_eq!(
            resolve_frontend(Some("ink"), Some("   ")),
            Frontend::Ratatui
        );
        // Entry present → Ink is the default (unset) and the explicit choice.
        assert_eq!(resolve_frontend(None, Some("/path/cli.js")), ink);
        assert_eq!(resolve_frontend(Some("ink"), Some("/path/cli.js")), ink);
        // Opt-out forces ratatui even with an entry available.
        for opt_out in ["native", "ratatui", "tui"] {
            assert_eq!(
                resolve_frontend(Some(opt_out), Some("/path/cli.js")),
                Frontend::Ratatui,
                "IGNIS_FRONTEND={opt_out} must force the built-in TUI"
            );
        }
    }

    #[test]
    fn locate_ink_entry_prefers_explicit_then_walks_up() {
        // Explicit non-empty wins outright (no filesystem touch).
        assert_eq!(
            locate_ink_entry(Some("/explicit/cli.js")),
            Some("/explicit/cli.js".to_string())
        );
        // Empty/whitespace explicit is ignored.
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("target").join("debug");
        std::fs::create_dir_all(&nested).unwrap();
        // No ignis-tui anywhere up the tree → None.
        assert_eq!(find_ink_entry_from(&nested), None);
        // Plant a source layout with cli.js but NO node_modules → not runnable.
        let tui = dir.path().join("ignis-tui");
        let cli = tui.join("src").join("cli.js");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "").unwrap();
        assert_eq!(
            find_ink_entry_from(&nested),
            None,
            "cli.js without node_modules must not count as available"
        );
        // Install the deps → now runnable, found by walking up from deep below.
        std::fs::create_dir_all(tui.join("node_modules")).unwrap();
        assert_eq!(find_ink_entry_from(&nested), Some(cli));
    }

    #[test]
    fn ink_entry_found_in_installed_ignis_home() {
        let home = tempfile::tempdir().unwrap();
        // Nothing laid down yet.
        assert_eq!(ink_entry_in_ignis_home(home.path()), None);
        // An install drops the JS + deps at ~/.ignis/ignis-tui/.
        let tui = home.path().join(".ignis").join("ignis-tui");
        let cli = tui.join("src").join("cli.js");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "").unwrap();
        // cli.js alone isn't enough.
        assert_eq!(ink_entry_in_ignis_home(home.path()), None);
        std::fs::create_dir_all(tui.join("node_modules")).unwrap();
        assert_eq!(ink_entry_in_ignis_home(home.path()), Some(cli));
    }

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
    fn short_r_is_alias_for_resume() {
        // `-r` bare: resume latest session.
        let args = parse(&["-r"]).to_session_args();
        assert!(args.resume);
        assert!(args.resume_session_id.is_none());
        assert!(args.prompt_args.is_empty());

        // `-r <id>`: resume the given session id.
        let args = parse(&["-r", "work"]).to_session_args();
        assert!(args.resume);
        assert_eq!(args.resume_session_id.as_deref(), Some("work"));

        // `-r <id> <prompt>`: id consumed, prompt trails — parity with
        // `--resume work follow-up` (resume_with_id_and_prompt).
        let args = parse(&["-r", "work", "follow-up"]).to_session_args();
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
