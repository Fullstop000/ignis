use ignis::{
    cli::{parse_cli_args, resolve_session_request},
    config::{build_provider, load_config},
    session::{project_sessions_dir, SessionManager},
    storage::FileStorage,
    AgentEvent, Session,
};
use std::path::PathBuf;
use std::sync::Arc;

fn get_git_status() -> String {
    std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "Not a git repository or git not installed".to_string())
}

fn get_git_diff() -> String {
    let output = std::process::Command::new("git")
        .args(["diff"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_else(|_| String::new());
    if output.trim().is_empty() {
        "No changes".to_string()
    } else {
        if output.len() > 2000 {
            format!("{}... (truncated)", &output[..2000])
        } else {
            output
        }
    }
}

fn get_current_date() -> String {
    std::process::Command::new("date")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "Unknown Date".to_string())
}

fn build_system_prompt(cwd: &std::path::Path) -> String {
    let git_status = get_git_status();
    let git_diff = get_git_diff();
    let current_date = get_current_date();
    let os_name = std::env::consts::OS;

    format!(
        "You are Ignis, an interactive agent that helps users with software engineering tasks. \
        Use the instructions below and the tools available to you to assist the user.

# Guidelines
 - All text you output outside of tool use is displayed to the user.
 - Tools are executed in a user-selected permission mode.
 - Read relevant code before changing it and keep changes tightly scoped to the request.
 - Do not add speculative abstractions, compatibility shims, or unrelated cleanup.
 - Do not create files unless they are required to complete the task.
 - If an approach fails, diagnose the failure before switching tactics.
 - Be careful not to introduce security vulnerabilities such as command injection, XSS, or SQL injection.
 - Report outcomes faithfully: if verification fails or was not run, say so explicitly.
 - Carefully consider reversibility and blast radius. Local, reversible actions like editing files or running tests are usually fine. Actions that affect shared systems, publish state, delete data, or otherwise have high blast radius should be explicitly authorized by the user.

# Tone & Style
 - Be concise. Start work immediately. No conversational fillers or preambles.
 - Answer directly without flattery or flippancy.
 - Don't summarize what you did unless asked. Don't explain your code unless asked.

# Environment Context
 - Operating System: {}
 - Working Directory: {}
 - Current Date/Time: {}

# Git Context
Git Status:
```
{}
```

Git Diff:
```
{}
```",
        os_name,
        cwd.display(),
        current_date,
        git_status,
        git_diff
    )
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Parse arguments
    let cli = parse_cli_args(std::env::args().skip(1).collect());
    let is_oneshot = !cli.is_tui && !cli.prompt_args.is_empty();

    // 1. Load config
    let config = load_config()?;

    // 2. Resolve paths
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not locate home directory"))?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let ignis_home = home.join(".ignis");
    let storage_root = ignis_home.clone();

    // Initialize logger
    if let Err(e) = ignis::logger::init(&ignis_home.join("logs")) {
        eprintln!("Failed to initialize logger: {}", e);
    }
    let storage_dir = project_sessions_dir(&storage_root, &cwd);
    let session_manager = SessionManager::new(storage_dir.clone());
    let auto_resume = config.auto_resume_last_session.unwrap_or(false);
    let session_request = resolve_session_request(cli, &session_manager, auto_resume, &cwd);
    let system_prompt = build_system_prompt(&cwd);
    let active_provider = config
        .active_provider()
        .unwrap_or_else(|| "default".to_string());
    let active_model = config
        .active_model()
        .unwrap_or_else(|| "default".to_string());

    // Route: TUI mode (default when no args, or explicit --tui)
    if session_request.is_tui || !is_oneshot {
        return ignis::console::run_console(
            active_provider,
            active_model,
            session_request.session_id,
            system_prompt,
            storage_dir,
            cwd,
            config,
        )
        .await;
    }

    // Route: One-shot CLI mode (ignis "do something")
    println!("=== Ignis (one-shot) ===");
    println!("Provider: {}/{}", active_provider, active_model);
    println!("Session: {}", session_request.session_id);

    let provider = build_provider(&config)?;
    let storage = FileStorage::new(storage_dir);
    let mut session = Session::open(
        session_request.session_id,
        system_prompt,
        provider,
        Box::new(storage),
        cwd.to_string_lossy().to_string(),
    )
    .await?;
    session.set_compaction(config.compaction.clone());

    // Register tools
    ignis::tools::register_native_tools(&mut session, &cwd, &config);
    let ext_dirs = ignis::plugin::default_extension_dirs();
    for d in &ext_dirs {
        if !d.exists() {
            let _ = std::fs::create_dir_all(d);
        }
    }
    let plugins = ignis::plugin::load_extensions(&ext_dirs);
    for plugin in plugins {
        session.register_tool(Arc::new(plugin));
    }

    // Run prompt
    let prompt_text = session_request.prompt_args.join(" ");
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    let prompt_task = tokio::spawn(async move {
        if let Err(e) = session.prompt(&prompt_text, tx).await {
            eprintln!("Agent error: {:?}", e);
        }
    });

    // Stream events to stdout
    use std::io::Write;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate { delta } => {
                print!("{}", delta);
                std::io::stdout().flush()?;
            }
            AgentEvent::ToolExecutionStart {
                tool_name,
                tool_call_id,
                arguments,
            } => {
                println!(
                    "\n>>> [Tool: {} ({})] args: {}",
                    tool_name, tool_call_id, arguments
                );
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
            } => {
                let prefix = if result.is_error { "ERR" } else { "OK" };
                println!(
                    "<<< [Tool {} ({}): {}]",
                    prefix, tool_call_id, result.content
                );
            }
            AgentEvent::AgentEnd => {
                println!();
            }
            _ => {}
        }
    }

    prompt_task.await?;
    Ok(())
}
