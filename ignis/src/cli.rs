use crate::session::SessionManager;

#[derive(Debug, PartialEq)]
pub struct CliArgs {
    pub is_tui: bool,
    pub resume: bool,
    pub resume_session_id: Option<String>,
    pub prompt_args: Vec<String>,
}

pub struct SessionRequest {
    pub session_id: String,
    pub is_tui: bool,
    pub prompt_args: Vec<String>,
}

pub fn parse_cli_args(args: Vec<String>) -> CliArgs {
    let mut is_tui = false;
    let mut resume = false;
    let mut resume_session_id = None;
    let mut prompt_args = Vec::new();
    let mut iter = args.into_iter().peekable();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--tui" | "tui" => is_tui = true,
            "--resume" => {
                resume = true;
                if let Some(next) = iter.peek() {
                    if next != "--tui" && next != "tui" && !next.starts_with("--") {
                        resume_session_id = iter.next();
                    }
                }
            }
            _ => prompt_args.push(arg),
        }
    }

    CliArgs {
        is_tui,
        resume,
        resume_session_id,
        prompt_args,
    }
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

    let is_tui = cli.is_tui || prompt_args.is_empty();

    SessionRequest {
        session_id,
        is_tui,
        prompt_args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_args_treats_resume_value_as_session_id() {
        let parsed = parse_cli_args(vec!["--resume".to_string(), "hello".to_string()]);

        assert!(parsed.resume);
        assert_eq!(parsed.resume_session_id.as_deref(), Some("hello"));
        assert!(parsed.prompt_args.is_empty());
    }

    #[test]
    fn parse_cli_args_tui_resume_has_no_prompt() {
        let parsed = parse_cli_args(vec!["--resume".to_string(), "--tui".to_string()]);

        assert!(parsed.resume);
        assert!(parsed.is_tui);
        assert!(parsed.resume_session_id.is_none());
        assert!(parsed.prompt_args.is_empty());
    }

    #[test]
    fn parse_cli_args_oneshot_with_prompt() {
        let parsed = parse_cli_args(vec!["hello world".to_string()]);

        assert!(!parsed.is_tui);
        assert!(!parsed.resume);
        assert_eq!(parsed.prompt_args, vec!["hello world"]);
    }

    #[test]
    fn parse_cli_args_explicit_tui() {
        let parsed = parse_cli_args(vec!["--tui".to_string()]);

        assert!(parsed.is_tui);
        assert!(parsed.prompt_args.is_empty());
    }

    #[test]
    fn parse_cli_args_oneshot_with_multiple_words() {
        let parsed = parse_cli_args(vec![
            "write".to_string(),
            "a".to_string(),
            "test".to_string(),
        ]);

        assert!(!parsed.is_tui);
        assert_eq!(parsed.prompt_args, vec!["write", "a", "test"]);
    }

    #[test]
    fn resolve_session_request_uses_resume_session_id() {
        let dir = crate::util::unique_temp_dir("ignis-resume-session-id");
        std::fs::create_dir_all(&dir).unwrap();
        let manager = SessionManager::new(dir.clone());

        let request = resolve_session_request(
            parse_cli_args(vec!["--resume".to_string(), "default".to_string()]),
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
            parse_cli_args(vec!["--resume".to_string()]),
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
            parse_cli_args(vec![
                "--resume".to_string(),
                "work".to_string(),
                "hello".to_string(),
            ]),
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
            parse_cli_args(vec!["hello".to_string()]),
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
            parse_cli_args(vec![]),
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
            parse_cli_args(vec![]),
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
            parse_cli_args(vec![]),
            &manager,
            true,
            std::path::Path::new("/tmp"),
        );

        assert_eq!(request.session_id, "newer");

        std::fs::remove_dir_all(dir).unwrap();
    }
}
