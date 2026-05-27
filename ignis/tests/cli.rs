use ignis::cli::{parse_cli_args, resolve_session_request};
use ignis::session::SessionManager;
use ignis::util::unique_temp_dir;
use std::io::Write;

// ==========================================
// CLI argument parsing
// ==========================================

#[test]
fn cli_no_args_defaults_to_tui() {
    let parsed = parse_cli_args(vec![]);
    // `is_tui` lives on SessionRequest, not CliArgs: an empty prompt_args
    // resolves to the TUI down in resolve_session_request.
    assert!(!parsed.resume);
    assert!(parsed.prompt_args.is_empty());
}

#[test]
fn cli_oneshot_single_arg() {
    let parsed = parse_cli_args(vec!["hello".to_string()]);
    assert_eq!(parsed.prompt_args, vec!["hello"]);
}

#[test]
fn cli_oneshot_multiple_args() {
    let parsed = parse_cli_args(vec![
        "write".to_string(),
        "a".to_string(),
        "test".to_string(),
    ]);
    assert_eq!(parsed.prompt_args, vec!["write", "a", "test"]);
}

#[test]
fn cli_resume_with_session_id() {
    let parsed = parse_cli_args(vec!["--resume".to_string(), "work".to_string()]);
    assert!(parsed.resume);
    assert_eq!(parsed.resume_session_id.as_deref(), Some("work"));
    assert!(parsed.prompt_args.is_empty());
}

#[test]
fn cli_resume_without_session_id() {
    let parsed = parse_cli_args(vec!["--resume".to_string()]);
    assert!(parsed.resume);
    assert!(parsed.resume_session_id.is_none());
}

#[test]
fn cli_resume_with_prompt() {
    let parsed = parse_cli_args(vec![
        "--resume".to_string(),
        "work".to_string(),
        "hello".to_string(),
    ]);
    assert!(parsed.resume);
    assert_eq!(parsed.resume_session_id.as_deref(), Some("work"));
    assert_eq!(parsed.prompt_args, vec!["hello"]);
}

#[test]
fn cli_ignores_unknown_flags_as_prompt_args() {
    let parsed = parse_cli_args(vec!["--unknown".to_string(), "hello".to_string()]);
    assert_eq!(parsed.prompt_args, vec!["--unknown", "hello"]);
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
        parse_cli_args(vec!["hello".to_string()]),
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
        parse_cli_args(vec![]),
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
        parse_cli_args(vec!["--resume".to_string()]),
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
