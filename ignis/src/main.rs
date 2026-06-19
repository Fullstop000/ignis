use clap::Parser;
use ignis::{
    cli::{resolve_session_request, Cli, Command},
    config::{build_provider, load_config},
    session::{project_sessions_dir_with_migration, SessionManager},
    storage::FileStorage,
    AgentEvent, Session,
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // clap parses argv, handles --help / --version / errors with proper exits.
    let cli = Cli::parse();

    // Home directory and file logger are shared by every branch (TUI, one-shot,
    // and subcommands). Initialize once up front and keep going even if logging
    // fails — the user still wants the command to run.
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not locate home directory"))?;
    if let Err(e) = ignis::logger::init(&home.join(".ignis/logs")) {
        eprintln!("Failed to initialize logger: {}", e);
    }

    // Subcommands short-circuit the session flow.
    match cli.command {
        Some(Command::Mcp(cmd)) => {
            return ignis::cli::mcp::run(cmd).await;
        }
        Some(Command::Upgrade(cmd)) => {
            return ignis::cli::upgrade::run(cmd).await;
        }
        Some(Command::Sessions(cmd)) => {
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let ignis_home = home.join(".ignis");
    let storage_root = ignis_home.clone();

    // Telemetry — no-op unless IGNIS_ENABLE_TELEMETRY=1 (or [telemetry] enabled).
    // Guard's Drop flushes + shuts down OTel providers on exit.
    let _telemetry_guard = ignis::telemetry::init(&config);

    let storage_dir = project_sessions_dir_with_migration(&storage_root, &cwd);
    let session_manager = SessionManager::new(storage_dir.clone());
    let auto_resume = config.auto_resume_last_session.unwrap_or(false);
    let session_request = resolve_session_request(cli, &session_manager, auto_resume, &cwd);
    let system_prompt = ignis::agent::build_system_prompt(&cwd);

    // Discover skills (global + project roots) and read the disabled set.
    // Reuse the state loaded earlier so permissions and skill/MCP disables are
    // consistent even if a background writer modifies state.json mid-startup.
    let disabled_skills: std::collections::HashSet<String> =
        persisted_state.disabled_skills.iter().cloned().collect();
    let skill_registry = std::sync::Arc::new(ignis::skills::SkillRegistry::load(
        Some(&home),
        &cwd,
        disabled_skills,
    ));

    // Spawn MCP servers (in parallel; each bounded by its `startup_timeout_secs`).
    // Failures don't block ignis — they surface in `/mcp` and `ignis mcp list`.
    let disabled_mcp: std::collections::HashSet<String> = persisted_state
        .disabled_mcp_servers
        .iter()
        .cloned()
        .collect();
    let mcp_registry = ignis::mcp::McpRegistry::spawn_all(&config.mcp.servers, disabled_mcp).await;

    // When no provider is configured, hand the TUI `None`; it renders the
    // no-provider welcome and routes the user through `/connect`. The one-shot
    // CLI path still hard-errors below — there's no interactive way to recover
    // from "no provider" in a single-shot invocation.
    let active_provider = config.active_provider();
    let active_model = config.active_model();

    if let (Some(p), Some(m)) = (&active_provider, &active_model) {
        ignis::telemetry::record_session_start(p, m);
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
        // The Ink launcher resolves `--resume <id>` / auto-resume in the parent
        // process, then spawns us with only `--engine` — so our own
        // `resolve_session_request` above never sees the resume and would start
        // fresh. It forwards the resolved id via `IGNIS_SESSION_ID`; adopt it so
        // the engine continues the right session. Absent (engine run standalone,
        // e.g. scripting/tests) we keep our own freshly-resolved id.
        let session_id = std::env::var("IGNIS_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or(session_request.session_id);
        let res = ignis::console::run_engine(
            session_id,
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
        // Frontend selection (PR #174, topology ii). When the Ink frontend can be
        // located — a source-checkout `ignis-tui/` next to the binary, the
        // `~/.ignis/ignis-tui` an install lays down, or an explicit IGNIS_TUI_ENTRY
        // — it is the default; the Ink host owns the terminal and spawns THIS
        // binary as `--engine` (IGNIS_ENGINE_BIN below). `IGNIS_FRONTEND=native`
        // forces the built-in ratatui TUI. Any failure — Node missing or <18, no
        // entry, spawn/launch error — falls through to the built-in TUI.
        let ink_entry =
            ignis::cli::locate_ink_entry(std::env::var("IGNIS_TUI_ENTRY").ok().as_deref());
        if let ignis::cli::Frontend::Ink { entry } = ignis::cli::resolve_frontend(
            std::env::var("IGNIS_FRONTEND").ok().as_deref(),
            ink_entry.as_deref(),
        ) {
            match launch_ink_frontend(&entry, &session_request.session_id).await {
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
    let (active_provider, active_model) = match (active_provider, active_model) {
        (Some(p), Some(m)) => (p, m),
        _ => {
            return Err(anyhow::anyhow!(
                "No provider configured. Launch the TUI (run `ignis` with no args) and run /connect."
            ));
        }
    };
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
        // One-shot CLI is a single blocking turn then exit: background shells
        // (which outlive a turn) and the footer make no sense, and there is no
        // live frontend to render a todo panel. Both tools still update their
        // persisted state here — it just isn't surfaced. (bg_shells, events)
        None,
        None,
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
///
/// `session_id` is the session this launch resolved (a `--resume`/auto-resume
/// target, or a fresh id). It's forwarded as `IGNIS_SESSION_ID` and inherited
/// down through `node` to the `--engine` child so the engine continues the same
/// session instead of minting its own — without this, `ignis --resume <id>`
/// silently starts fresh under Ink.
async fn launch_ink_frontend(entry: &str, session_id: &str) -> std::io::Result<i32> {
    let exe = std::env::current_exe()?;
    // Ink is now the default UI, so a too-old Node must NOT crash the user: ink 5
    // / react 18 throw at module load on Node <18. Probe the version first and
    // bubble an error so the caller falls back to the built-in TUI instead.
    let version = tokio::process::Command::new("node")
        .arg("--version")
        .output()
        .await?;
    if !version.status.success()
        || !node_version_supported(&String::from_utf8_lossy(&version.stdout))
    {
        return Err(std::io::Error::other(format!(
            "Node >=18 required (found {:?})",
            String::from_utf8_lossy(&version.stdout).trim()
        )));
    }
    // `status()` inherits stdin/stdout/stderr — Ink renders to the real terminal.
    let status = tokio::process::Command::new("node")
        .arg(entry)
        .env("IGNIS_ENGINE_BIN", exe)
        .env("IGNIS_SESSION_ID", session_id)
        .status()
        .await?;
    Ok(status.code().unwrap_or(0))
}

/// Does `node --version` output (e.g. `"v20.11.1\n"`) report a major >= 18?
/// Unparseable output is treated as unsupported — better to fall back than risk
/// a module-load crash.
fn node_version_supported(version_output: &str) -> bool {
    version_output
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 18)
}

#[cfg(test)]
mod tests {
    use super::node_version_supported;

    #[test]
    fn node_version_gate() {
        assert!(node_version_supported("v18.0.0\n"));
        assert!(node_version_supported("v20.11.1"));
        assert!(node_version_supported("v22.22.0\n"));
        assert!(!node_version_supported("v16.20.2\n"));
        assert!(!node_version_supported("v12.0.0"));
        assert!(!node_version_supported("")); // node missing / empty
        assert!(!node_version_supported("not-a-version"));
    }
}
