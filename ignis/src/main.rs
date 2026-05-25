use ignis::{
    cli::{parse_cli_args, resolve_session_request},
    config::{build_provider, load_config},
    session::{project_sessions_dir, SessionManager},
    storage::FileStorage,
    AgentEvent, Session,
};
use std::path::PathBuf;

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
    let system_prompt = ignis::agent::build_system_prompt(&cwd);
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
