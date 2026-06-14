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
        Some(Command::Sessions(cmd)) => {
            let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
            if let Err(e) = ignis::logger::init(&home.join(".ignis/logs")) {
                eprintln!("Failed to initialize logger: {}", e);
            }
            return ignis::cli::sessions::run(cmd).await;
        }
        None => {}
    }

    // Headless engine mode (`--engine`): captured before `cli` is consumed by
    // `to_session_args`. Routes to `run_engine` instead of the ratatui TUI.
    let engine_mode = cli.engine;
    let cli = cli.to_session_args();
    let is_oneshot = !cli.prompt_args.is_empty();

    // 1. Load config
    let config = load_config()?;

    // Resolve permission mode. Precedence: `--afk` flag > one-shot implicit
    // > persisted `state.json` > built-in `Off`. The `--afk` flag and the
    // one-shot implicit both pin `FullyUnattended` — they signal "no TTY,
    // don't pause for input" and that's what FullyUnattended encodes.
    let persisted_state = ignis::state::load_state();
    let resolved_mode = if cli.afk || is_oneshot {
        ignis::permissions::Mode::FullyUnattended
    } else {
        persisted_state
            .mode
            .as_deref()
            .and_then(ignis::permissions::Mode::parse)
            .unwrap_or_default()
    };
    // The user-declared rule layer: `[permissions]` from config.toml plus the
    // persisted "always allow" grants from state.json (folded into `allow`).
    let perm_rules = ignis::permissions::rule::RuleSet::from_strings(
        &config.permissions.allow,
        &config.permissions.ask,
        &config.permissions.deny,
    );
    let permissions = ignis::permissions::runtime::PermissionState::with_rules(
        resolved_mode,
        perm_rules,
        persisted_state.permission_grants.clone(),
    );

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

    // Telemetry — no-op unless IGNIS_ENABLE_TELEMETRY=1 (or [telemetry] enabled).
    // Guard's Drop flushes + shuts down OTel providers on exit.
    let _telemetry_guard = ignis::telemetry::init(&config);

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

    // When no provider is configured, hand the TUI empty strings; it renders
    // the no-provider welcome and routes the user through `/connect`. The
    // one-shot CLI path still hard-errors below — there's no interactive way
    // to recover from "no provider" in a single-shot invocation.
    let active_provider = config.active_provider().unwrap_or_default();
    let active_model = config.active_model().unwrap_or_default();

    if !active_provider.is_empty() {
        ignis::telemetry::record_session_start(&active_provider, &active_model);
    }

    // Load the external-subprocess hook registry once at startup; the
    // runner shares this handle so `/hooks reload` swaps the live config
    // for every session that follows.
    let hook_registry = ignis::hooks::HookRegistry::from_config_dir(&home)?;

    // Route: headless engine (`--engine`) — protocol over stdin/stdout, no TUI.
    // The out-of-process frontend (Ink `ignis-tui`) owns the terminal and
    // spawns this; it reads Outbound frames from our stdout and writes
    // ClientCommands to our stdin.
    if engine_mode {
        let res = ignis::console::run_engine(
            session_request.session_id,
            system_prompt,
            storage_dir,
            cwd,
            config,
            skill_registry.clone(),
            mcp_registry.clone(),
            permissions.clone(),
            hook_registry.clone(),
        )
        .await;
        mcp_registry.shutdown().await;
        return res;
    }

    // Route: TUI mode (default when no args, or explicit --tui)
    if session_request.is_tui || !is_oneshot {
        // Frontend selection (PR #174, topology ii). Ratatui in-process is the
        // default + always-available fallback; the Ink frontend is opt-in via
        // IGNIS_FRONTEND=ink + IGNIS_TUI_ENTRY=<path to ignis-tui/src/cli.js>.
        // When selected, the Ink host owns the terminal and spawns THIS binary
        // as `--engine` (IGNIS_ENGINE_BIN below). Any failure — Node missing,
        // entry unset, spawn error — falls through to the built-in TUI.
        //
        // PACKAGING (deferred, needs sign-off): a real install must decide how
        // `ignis-tui` + a Node runtime ship — bundle Node, require system Node,
        // or vendor the JS — and only then can the entry be auto-located instead
        // of passed via IGNIS_TUI_ENTRY. Not finalized here; release.yml unchanged.
        if let ignis::cli::Frontend::Ink { entry } = ignis::cli::resolve_frontend(
            std::env::var("IGNIS_FRONTEND").ok().as_deref(),
            std::env::var("IGNIS_TUI_ENTRY").ok().as_deref(),
        ) {
            match launch_ink_frontend(&entry).await {
                Ok(code) => {
                    mcp_registry.shutdown().await;
                    std::process::exit(code);
                }
                Err(e) => {
                    eprintln!("ignis: Ink frontend unavailable ({e}); using the built-in TUI.");
                }
            }
        }
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
            hook_registry.clone(),
        )
        .await;
        // Bring down MCP servers explicitly before tokio runtime tears down —
        // relying on Drop alone races runtime shutdown and orphans children.
        mcp_registry.shutdown().await;
        return res;
    }

    // Route: One-shot CLI mode (ignis "do something") — needs a real provider.
    if active_provider.is_empty() {
        return Err(anyhow::anyhow!(
            "No provider configured. Launch the TUI (run `ignis` with no args) and run /connect."
        ));
    }
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
    session.set_hook_registry(hook_registry);

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
            AgentEvent::TurnEnd => {
                println!();
            }
            _ => {}
        }
    }

    prompt_task.await?;
    mcp_registry.shutdown().await;
    Ok(())
}

/// Launch the out-of-process Ink frontend: run `node <entry>` with our terminal
/// inherited (so Ink owns the TTY) and `IGNIS_ENGINE_BIN` set to this
/// executable, so the Ink host spawns us back as `ignis --engine` and speaks
/// the NDJSON protocol over that child's pipes. Returns the child's exit code;
/// any error (e.g. `node` not on PATH) bubbles up so the caller falls back to
/// the built-in ratatui TUI.
async fn launch_ink_frontend(entry: &str) -> std::io::Result<i32> {
    let exe = std::env::current_exe()?;
    // `status()` inherits stdin/stdout/stderr — Ink renders to the real terminal.
    let status = tokio::process::Command::new("node")
        .arg(entry)
        .env("IGNIS_ENGINE_BIN", exe)
        .status()
        .await?;
    Ok(status.code().unwrap_or(0))
}
