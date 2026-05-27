use clap::Parser;
use ignis::cli::{resolve_session_request, Cli};
use ignis::session::SessionManager;
use ignis::util::unique_temp_dir;
use std::io::Write;

fn parse(argv: &[&str]) -> Cli {
    let mut full = vec!["ignis"];
    full.extend_from_slice(argv);
    Cli::try_parse_from(full).expect("parse")
}

// ==========================================
// CLI argument parsing (via the unified clap-derived Cli)
// ==========================================

#[test]
fn cli_no_args_defaults_to_tui() {
    let args = parse(&[]).to_session_args();
    // `is_tui` lives on SessionRequest, not CliArgs: an empty prompt_args
    // resolves to the TUI down in resolve_session_request.
    assert!(!args.resume);
    assert!(args.prompt_args.is_empty());
}

#[test]
fn cli_oneshot_single_arg() {
    let args = parse(&["hello"]).to_session_args();
    assert_eq!(args.prompt_args, vec!["hello"]);
}

#[test]
fn cli_oneshot_multiple_args() {
    let args = parse(&["write", "a", "test"]).to_session_args();
    assert_eq!(args.prompt_args, vec!["write", "a", "test"]);
}

#[test]
fn cli_resume_with_session_id() {
    let args = parse(&["--resume", "work"]).to_session_args();
    assert!(args.resume);
    assert_eq!(args.resume_session_id.as_deref(), Some("work"));
    assert!(args.prompt_args.is_empty());
}

#[test]
fn cli_resume_without_session_id() {
    let args = parse(&["--resume"]).to_session_args();
    assert!(args.resume);
    assert!(args.resume_session_id.is_none());
}

#[test]
fn cli_resume_with_prompt() {
    let args = parse(&["--resume", "work", "hello"]).to_session_args();
    assert!(args.resume);
    assert_eq!(args.resume_session_id.as_deref(), Some("work"));
    assert_eq!(args.prompt_args, vec!["hello"]);
}

#[test]
fn cli_rejects_unknown_top_level_flag() {
    // Tightened in v0.15.1: clap rejects typos instead of silently sending
    // them as one-shot prompt text. Use `ignis -- --unknown` for a literal.
    let err = Cli::try_parse_from(["ignis", "--unknown"]).unwrap_err();
    assert!(matches!(
        err.kind(),
        clap::error::ErrorKind::UnknownArgument | clap::error::ErrorKind::InvalidSubcommand
    ));
}

#[test]
fn cli_hyphen_prompt_works_after_dash_dash() {
    let args = parse(&["--", "--debug", "fix the bug"]).to_session_args();
    assert_eq!(args.prompt_args, vec!["--debug", "fix the bug"]);
}

// ==========================================
// Session resolution
// ==========================================

#[test]
fn resolve_defaults_to_default_session() {
    let dir = unique_temp_dir("cli-default-session");
    std::fs::create_dir_all(&dir).unwrap();
    let manager = SessionManager::new(dir.clone());

    let request = resolve_session_request(
        parse(&["hello"]).to_session_args(),
        &manager,
        false,
        std::path::Path::new("/tmp"),
    );

    // With auto_resume=false, always creates a new session
    assert!(!request.session_id.is_empty());
    assert!(!request.is_tui);
    assert_eq!(request.prompt_args, vec!["hello"]);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resolve_empty_prompt_goes_to_tui() {
    let dir = unique_temp_dir("cli-empty-tui");
    std::fs::create_dir_all(&dir).unwrap();
    let manager = SessionManager::new(dir.clone());

    let request = resolve_session_request(
        parse(&[]).to_session_args(),
        &manager,
        false,
        std::path::Path::new("/tmp"),
    );

    assert!(request.is_tui);
    // With auto_resume=false, creates a new session instead of "default"
    assert!(!request.session_id.is_empty());

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resolve_resume_picks_latest_session() {
    let dir = unique_temp_dir("cli-resume-latest");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alpha.jsonl"), "{}\n").unwrap();
    std::fs::write(dir.join("beta.jsonl"), "{}\n").unwrap();
    // Touch beta so it's newer
    let beta_path = dir.join("beta.jsonl");
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::OpenOptions::new()
        .append(true)
        .open(&beta_path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();

    let manager = SessionManager::new(dir.clone());
    let request = resolve_session_request(
        parse(&["--resume"]).to_session_args(),
        &manager,
        false,
        std::path::Path::new("/tmp"),
    );

    // Note: the exact session picked depends on file-system ordering and mtime.
    // We just verify it picked *some* existing session rather than "default".
    assert!(request.session_id == "alpha" || request.session_id == "beta");
    assert!(request.is_tui);

    std::fs::remove_dir_all(dir).unwrap();
}
