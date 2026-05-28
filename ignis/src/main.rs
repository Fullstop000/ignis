use clap::Parser;
use ignis::{
    cli::{resolve_session_request, Cli, Command},
    config::{build_provider, load_config},
    session::{project_sessions_dir, SessionManager},
    storage::FileStorage,
    AgentEvent, Session,
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // clap parses argv, handles --help / --version / errors with proper exits.
    let cli = Cli::parse();

    // Subcommands short-circuit the session flow.
    match cli.command {
        Some(Command::Mcp(cmd)) => {
            let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
            if let Err(e) = ignis::logger::init(&home.join(".ignis/logs")) {
                eprintln!("Failed to initialize logger: {}", e);
            }
            return ignis::cli::mcp::run(cmd).await;
        }
        Some(Command::Upgrade(cmd)) => {
            return ignis::cli::upgrade::run(cmd).await;
        }
        None => {}
    }

    let cli = cli.to_session_args();
    let is_oneshot = !cli.prompt_args.is_empty();

    // 1. Load config
    let config = load_config()?;

    // Resolve permission state: CLI flag > state.json > default. AFK is the
    // logical OR of the CLI flag, the persisted state, and the one-shot
    // implicit-AFK rule (no TTY to prompt on, so we must auto-handle).
    // A typo'd `--permission-mode foo` errors out — silently demoting it to
    // Default would be the worst kind of foot-gun (user thinks they got bypass,
    // gets safe-mode instead, or vice versa).
    let persisted_state = ignis::state::load_state();
    let resolved_mode = if let Some(raw) = cli.permission_mode.as_deref() {
        ignis::permissions::Mode::parse(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "--permission-mode: unknown value {raw:?}. Valid: default, bypassPermissions."
            )
        })?
    } else {
        persisted_state
            .permission_mode
            .as_deref()
            .and_then(ignis::permissions::Mode::parse)
            .unwrap_or_default()
    };
    let resolved_afk = cli.afk || persisted_state.afk || is_oneshot;
    let permissions =
        ignis::permissions::runtime::PermissionState::new(resolved_mode, resolved_afk);

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

    // Discover skills (global + project roots) and read the disabled set.
    let state = ignis::state::load_state();
    let disabled_skills: std::collections::HashSet<String> =
        state.disabled_skills.iter().cloned().collect();
    let skill_registry = std::sync::Arc::new(ignis::skills::SkillRegistry::load(
        Some(&home),
        &cwd,
        disabled_skills,
    ));

    // Spawn MCP servers (in parallel; each bounded by its `startup_timeout_secs`).
    // Failures don't block ignis — they surface in `/mcp` and `ignis mcp list`.
    let disabled_mcp: std::collections::HashSet<String> =
        state.disabled_mcp_servers.iter().cloned().collect();
    let mcp_registry = ignis::mcp::McpRegistry::spawn_all(&config.mcp.servers, disabled_mcp).await;

    let active_provider = config
        .active_provider()
        .unwrap_or_else(|| "default".to_string());
    let active_model = config
        .active_model()
        .unwrap_or_else(|| "default".to_string());

    // Route: TUI mode (default when no args, or explicit --tui)
    if session_request.is_tui || !is_oneshot {
        let res = ignis::console::run_console(
            active_provider,
            active_model,
            session_request.session_id,
            system_prompt,
            storage_dir,
            cwd,
            config,
            skill_registry.clone(),
            mcp_registry.clone(),
            permissions.clone(),
        )
        .await;
        // Bring down MCP servers explicitly before tokio runtime tears down —
        // relying on Drop alone races runtime shutdown and orphans children.
        mcp_registry.shutdown().await;
        return res;
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
    let mcp_for_subagent = if !mcp_registry.is_empty() {
        Some(mcp_registry.clone())
    } else {
        None
    };
    // One-shot CLI is headless — no interactive picker. AFK was force-set
    // above, so ask_user auto-dismisses with structured guidance instead of
    // hanging on the missing TTY.
    ignis::tools::register_native_tools_with_mcp(
        &mut session,
        &cwd,
        &config,
        mcp_for_subagent,
        None,
        Some(permissions.clone()),
    );

    // Permission gate. AFK is on for one-shot CLI so most Ask decisions
    // become Allow inside the checker; circuit breakers + protected paths
    // still Deny, surfacing the refusal to the model so it can adapt.
    session.set_hooks(Box::new(
        ignis::permissions::checker::PermissionChecker::new(permissions.clone()),
    ));
    if !skill_registry.is_empty() {
        session.set_skills(skill_registry.clone());
        session.register_tool(std::sync::Arc::new(ignis::tools::SkillTool::new(
            skill_registry.clone(),
        )));
    }
    if !mcp_registry.is_empty() {
        session.set_mcp(mcp_registry.clone());
        ignis::tools::register_mcp_tools(&mut session, &mcp_registry);
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
    mcp_registry.shutdown().await;
    Ok(())
}
