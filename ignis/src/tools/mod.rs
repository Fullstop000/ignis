pub mod tool;

mod agent;
pub(crate) mod ask_user;
mod background;
mod bash;
mod create_file;
mod cwd;
mod edit_file;
mod glob;
mod grep;
mod list_dir;
mod read_file;
mod skill;
mod todo_write;
pub(crate) mod util;
mod web_fetch;
mod web_search;
mod worktree;

pub use agent::SubagentTool;
pub use ask_user::AskUserTool;
pub use background::{BackgroundCtx, BackgroundShells, BashOutputTool, KillShellTool};
pub use bash::{BashSandbox, BashTool};
pub use create_file::CreateFileTool;
pub use cwd::SessionCwd;
pub use edit_file::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
pub use read_file::ReadFileTool;
pub use skill::SkillTool;
pub use todo_write::{new_store as new_todo_store, Todo, TodoStatus, TodoStore, TodoWriteTool};
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;

use crate::tools::tool::AgentTool;
use std::path::Path;
use std::sync::Arc;

/// The base native toolset shared by the main agent and sub-agents (everything
/// except the `agent` tool itself, so sub-agents don't nest).
///
/// `background` enables `bash`'s `run_in_background` flag (top-level only).
/// `bash_sandbox` confines auto-run bash writes in unattended modes. Sub-agents
/// pass `None` for both — plain blocking bash, no background shells, no sandbox.
pub fn native_tools(
    cwd: impl Into<SessionCwd>,
    web_search: crate::config::WebSearchConfig,
    background: Option<background::BackgroundCtx>,
    bash_sandbox: Option<bash::BashSandbox>,
) -> Vec<Arc<dyn AgentTool>> {
    let cwd: SessionCwd = cwd.into();
    vec![
        Arc::new(ReadFileTool::new(cwd.clone())) as Arc<dyn AgentTool>,
        Arc::new(CreateFileTool::new(cwd.clone())),
        Arc::new(EditFileTool::new(cwd.clone())),
        Arc::new(ListDirTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(GlobTool::new(cwd.clone())),
        Arc::new(
            BashTool::new(cwd)
                .with_background(background)
                .with_sandbox(bash_sandbox),
        ),
        Arc::new(WebFetchTool::new()),
        Arc::new(WebSearchTool::new(web_search.provider, web_search.api_key)),
    ]
}

/// The read-only subset of native tools: file reads + search, with no write,
/// execution, or network reach. Used by the read-only sub-agent types
/// (`explore`, `review`). Read-only here is enforced by tool selection, not a
/// sandbox — the permission pipeline remains the hard gate underneath.
pub fn read_only_tools(cwd: impl Into<SessionCwd>) -> Vec<Arc<dyn AgentTool>> {
    let cwd: SessionCwd = cwd.into();
    vec![
        Arc::new(ReadFileTool::new(cwd.clone())) as Arc<dyn AgentTool>,
        Arc::new(ListDirTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd.clone())),
        Arc::new(GlobTool::new(cwd)),
    ]
}

pub fn register_native_tools(
    session: &mut crate::Session,
    cwd: &Path,
    config: &crate::config::Config,
) {
    register_native_tools_with_mcp(session, cwd, config, None, None, None, None, None)
}

/// Resolve the bash write-sandbox for the top-level agent. Two gates: it only
/// applies in the auto-approve (unattended) modes — `None` in `Off`, where the
/// permission prompt is the gate — AND only when the user has opted in via
/// `sandbox_enabled` (off by default, so AFK/headless runs are unconfined and
/// credentialed commands like `git push` work out of the box). Toggled by
/// `/sandbox` (Ink) and the `/settings` Sandbox tab (native). The configured
/// `sandbox_write_paths` are `~`-expanded.
fn resolve_bash_sandbox(
    config: &crate::config::Config,
    permissions: Option<&Arc<crate::permissions::runtime::PermissionState>>,
) -> Option<bash::BashSandbox> {
    let p = permissions?;
    if !p.mode().auto_approves_sensitive() || !p.sandbox_enabled() {
        return None;
    }
    let home = dirs::home_dir();
    let expand = |paths: &[String]| -> Vec<std::path::PathBuf> {
        paths
            .iter()
            .map(|s| match (s.strip_prefix("~/"), &home) {
                (Some(rest), Some(h)) => h.join(rest),
                _ => std::path::PathBuf::from(s),
            })
            .collect()
    };
    Some(bash::BashSandbox {
        extra_writes: expand(&config.permissions.sandbox_write_paths),
        extra_reads: expand(&config.permissions.sandbox_read_paths),
    })
}

/// Same as `register_native_tools` but threads a shared MCP registry into the
/// `SubagentTool` (so sub-agents inherit MCP tools), an optional picker
/// channel into `AskUserTool` (so the model can interactively ask the user),
/// and an optional shared `PermissionState` so `ask_user` honors AFK mode.
/// `picker_tx = None` in headless contexts disables `ask_user` cleanly;
/// `permissions = None` skips the AFK guard (e.g. tests with no permission
/// system attached). `bg_shells = Some` enables background bash + registers the
/// `bash_output`/`kill_shell` tools (top-level only); `events` is the frontend
/// channel for both the background-shell footer and `todo_write` surfacing
/// (`events = None` leaves them un-surfaced — the writes still persist).
#[allow(clippy::too_many_arguments)]
pub fn register_native_tools_with_mcp(
    session: &mut crate::Session,
    cwd: &Path,
    config: &crate::config::Config,
    mcp: Option<Arc<crate::mcp::McpRegistry>>,
    picker_tx: Option<tokio::sync::mpsc::Sender<crate::interaction::PickerRequest>>,
    permissions: Option<Arc<crate::permissions::runtime::PermissionState>>,
    bg_shells: Option<Arc<background::BackgroundShells>>,
    events: Option<tokio::sync::mpsc::Sender<crate::AgentEvent>>,
) {
    let bg_ctx = bg_shells.clone().map(|shells| background::BackgroundCtx {
        shells,
        events: events.clone(),
    });
    let bash_sandbox = resolve_bash_sandbox(config, permissions.as_ref());
    // One shared cwd handle for the session, so `enter_worktree` can redirect
    // file/bash tools, hooks, and sub-agents at once by swapping it.
    let session_cwd = session.cwd_handle();
    session_cwd.set(cwd.to_path_buf());
    let wt_state = worktree::new_state();
    if let Some(active) = worktree::recover_active(session.history()) {
        session_cwd.set(active.session_cwd.clone());
        *wt_state.lock().unwrap() = Some(active);
    }
    for tool in native_tools(
        session_cwd.clone(),
        config.web_search.clone(),
        bg_ctx,
        bash_sandbox,
    ) {
        session.register_tool(tool);
    }
    // Background-shell polling tools — top-level only (sub-agents return one
    // final answer; a background shell outliving them serves no purpose).
    if let Some(shells) = bg_shells {
        session.register_tool(Arc::new(BashOutputTool::new(shells.clone())));
        session.register_tool(Arc::new(KillShellTool::new(shells, events.clone())));
    }
    // Worktree tools — top-level only, sharing the session cwd so entering a
    // worktree redirects the whole toolset. Sub-agents don't get them (they
    // can't switch the session's working directory).
    session.register_tool(Arc::new(worktree::EnterWorktreeTool::new(
        session_cwd.clone(),
        wt_state.clone(),
    )));
    session.register_tool(Arc::new(worktree::ExitWorktreeTool::new(
        session_cwd.clone(),
        wt_state,
    )));
    // The `agent` tool builds sub-agents from the config; registered only at the
    // top level so sub-agents can't recurse.
    let mut subagent = SubagentTool::new(config.clone(), session_cwd);
    if let Some(mcp) = mcp {
        subagent = subagent.with_mcp(mcp);
    }
    session.register_tool(Arc::new(subagent));
    // The `ask_user` tool — registered only at the top level. Sub-agents are
    // for self-contained work and shouldn't pause to interrogate the user.
    let mut ask_user = AskUserTool::new(picker_tx);
    if let Some(p) = permissions {
        ask_user = ask_user.with_permissions(p);
    }
    session.register_tool(Arc::new(ask_user));
    // The `todo_write` tool shares the session's persisted task list and emits
    // `Todos` events over `events`. Top-level only — a sub-agent's task list
    // would be invisible and serves no purpose.
    let todo_store = session.todos_handle();
    session.register_tool(Arc::new(TodoWriteTool::new(todo_store, events)));
}

/// Register every tool exposed by a connected MCP server as an `AgentTool`.
/// Disabled or failed servers contribute nothing — the registry knows.
pub fn register_mcp_tools(session: &mut crate::Session, registry: &crate::mcp::McpRegistry) {
    for wrapper in registry.wrappers() {
        session.register_tool(wrapper as Arc<dyn AgentTool>);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmProvider, LlmResponseDelta, Message};
    use crate::permissions::runtime::PermissionState;
    use crate::permissions::Mode;
    use crate::storage::SessionStorage;
    use futures_util::{stream, StreamExt};
    use serde_json::json;
    use std::path::Path;
    use std::process::Command;

    struct NoopProvider;

    #[async_trait::async_trait]
    impl LlmProvider for NoopProvider {
        async fn chat_stream(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _tools: &[serde_json::Value],
        ) -> Result<
            futures_util::stream::BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>,
            anyhow::Error,
        > {
            Ok(stream::empty().boxed())
        }

        fn model_id(&self) -> &str {
            "noop"
        }

        fn provider_name(&self) -> &str {
            "noop"
        }
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cmd {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        sh(p, &["git", "init", "-q", "-b", "main"]);
        sh(p, &["git", "config", "user.email", "t@t.dev"]);
        sh(p, &["git", "config", "user.name", "Tester"]);
        std::fs::write(p.join("README.md"), "hi\n").unwrap();
        sh(p, &["git", "add", "."]);
        sh(p, &["git", "commit", "-qm", "init"]);
        tmp
    }

    #[test]
    fn bash_sandbox_off_by_default_even_in_unattended_modes() {
        let cfg = crate::config::Config::default();
        // Sandbox defaults OFF — even in the unattended modes — so the user's
        // credentialed commands work until they opt in.
        for mode in [Mode::Off, Mode::HandsFree, Mode::FullyUnattended] {
            let p = PermissionState::new(mode);
            assert!(
                resolve_bash_sandbox(&cfg, Some(&p)).is_none(),
                "{mode:?} should be unsandboxed by default"
            );
        }
        // No permission state (tests) → no sandbox.
        assert!(resolve_bash_sandbox(&cfg, None).is_none());
    }

    #[test]
    fn bash_sandbox_active_when_enabled_in_unattended_modes() {
        let cfg = crate::config::Config::default();
        // Opt-in: enabled + an unattended mode → sandbox active.
        let hf = PermissionState::new(Mode::HandsFree);
        hf.set_sandbox_enabled(true);
        assert!(resolve_bash_sandbox(&cfg, Some(&hf)).is_some());
        let fu = PermissionState::new(Mode::FullyUnattended);
        fu.set_sandbox_enabled(true);
        assert!(resolve_bash_sandbox(&cfg, Some(&fu)).is_some());
        // Enabled but interactive `Off` → still no sandbox (mode gate wins).
        let off = PermissionState::new(Mode::Off);
        off.set_sandbox_enabled(true);
        assert!(resolve_bash_sandbox(&cfg, Some(&off)).is_none());
    }

    #[test]
    fn bash_sandbox_expands_configured_write_paths() {
        let mut cfg = crate::config::Config::default();
        cfg.permissions.sandbox_write_paths =
            vec!["~/.cargo".to_string(), "/opt/cache".to_string()];
        let fu = PermissionState::new(Mode::FullyUnattended);
        fu.set_sandbox_enabled(true);
        let sb = resolve_bash_sandbox(&cfg, Some(&fu)).expect("sandbox active");
        // The `~/` entry is home-expanded; the absolute one passes through.
        assert!(sb.extra_writes.iter().any(|p| p.ends_with(".cargo")));
        assert!(sb
            .extra_writes
            .iter()
            .any(|p| p == std::path::Path::new("/opt/cache")));
        if let Some(home) = dirs::home_dir() {
            assert!(sb.extra_writes.contains(&home.join(".cargo")));
        }
    }

    #[test]
    fn bash_sandbox_expands_configured_read_paths() {
        let mut cfg = crate::config::Config::default();
        cfg.permissions.sandbox_read_paths = vec!["~/.npm".to_string(), "/opt/sdk".to_string()];
        let fu = PermissionState::new(Mode::FullyUnattended);
        fu.set_sandbox_enabled(true);
        let sb = resolve_bash_sandbox(&cfg, Some(&fu)).expect("sandbox active");
        assert!(sb.extra_reads.iter().any(|p| p.ends_with(".npm")));
        assert!(sb
            .extra_reads
            .iter()
            .any(|p| p == std::path::Path::new("/opt/sdk")));
        if let Some(home) = dirs::home_dir() {
            assert!(sb.extra_reads.contains(&home.join(".npm")));
        }
    }

    #[tokio::test]
    async fn registered_worktree_redirects_registered_file_tools() {
        let repo = init_repo();
        let subdir = repo.path().join("crates/app");
        std::fs::create_dir_all(subdir.join("src")).unwrap();
        std::fs::write(subdir.join("src/lib.rs"), "// original\n").unwrap();
        sh(repo.path(), &["git", "add", "."]);
        sh(repo.path(), &["git", "commit", "-qm", "add app"]);

        let mut session = crate::Session::open(
            "test".to_string(),
            "system".to_string(),
            Box::new(NoopProvider),
            Box::new(crate::storage::InMemoryStorage::new()),
            subdir.display().to_string(),
        )
        .await
        .unwrap();
        register_native_tools_with_mcp(
            &mut session,
            &subdir,
            &crate::config::Config::default(),
            None,
            None,
            None,
            None,
            None,
        );

        let enter = session.tool_for_test("enter_worktree").unwrap();
        let create = session.tool_for_test("create_file").unwrap();
        let read = session.tool_for_test("read_file").unwrap();
        let exit = session.tool_for_test("exit_worktree").unwrap();

        let entered = enter.call(json!({ "name": "registered" })).await;
        assert!(!entered.is_error, "{}", entered.content);
        let worktree = repo.path().join(".ignis/worktrees/registered");
        let worktree_subdir = worktree.join("crates/app");
        assert!(worktree.exists());
        assert!(worktree_subdir.exists());

        let created = create
            .call(json!({ "path": "src/generated.rs", "content": "from worktree" }))
            .await;
        assert!(!created.is_error, "{}", created.content);

        assert!(
            !subdir.join("src/generated.rs").exists(),
            "registered create_file must not write into the original checkout"
        );
        assert_eq!(
            std::fs::read_to_string(worktree_subdir.join("src/generated.rs")).unwrap(),
            "from worktree"
        );

        let read_back = read.call(json!({ "path": "src/generated.rs" })).await;
        assert!(!read_back.is_error, "{}", read_back.content);
        assert_eq!(read_back.content, "from worktree");

        let removed = exit
            .call(json!({ "action": "remove", "discard_changes": true }))
            .await;
        assert!(!removed.is_error, "{}", removed.content);
        assert!(!worktree.exists());
    }

    #[tokio::test]
    async fn registered_tools_recover_active_worktree_on_resume() {
        let repo = init_repo();
        let subdir = repo.path().join("crates/app");
        std::fs::create_dir_all(subdir.join("src")).unwrap();
        std::fs::write(subdir.join("src/lib.rs"), "// original\n").unwrap();
        sh(repo.path(), &["git", "add", "."]);
        sh(repo.path(), &["git", "commit", "-qm", "add app"]);

        let mut session = crate::Session::open(
            "resume".to_string(),
            "system".to_string(),
            Box::new(NoopProvider),
            Box::new(crate::storage::InMemoryStorage::new()),
            subdir.display().to_string(),
        )
        .await
        .unwrap();
        register_native_tools_with_mcp(
            &mut session,
            &subdir,
            &crate::config::Config::default(),
            None,
            None,
            None,
            None,
            None,
        );
        let enter = session.tool_for_test("enter_worktree").unwrap();
        let entered = enter.call(json!({ "name": "resume-wt" })).await;
        assert!(!entered.is_error, "{}", entered.content);

        let storage = crate::storage::InMemoryStorage::new();
        storage
            .save_session(
                "resume",
                &[Message {
                    role: "tool".to_string(),
                    content: Some(
                        json!({ "result": entered.content, "is_error": false }).to_string(),
                    ),
                    reasoning_content: None,
                    name: Some("enter_worktree".to_string()),
                    tool_call_id: Some("call_1".to_string()),
                    tool_calls: None,
                    created_at_ms: None,
                }],
                Some(&subdir.display().to_string()),
            )
            .await
            .unwrap();

        let mut resumed = crate::Session::open(
            "resume".to_string(),
            "system".to_string(),
            Box::new(NoopProvider),
            Box::new(storage),
            subdir.display().to_string(),
        )
        .await
        .unwrap();
        register_native_tools_with_mcp(
            &mut resumed,
            &subdir,
            &crate::config::Config::default(),
            None,
            None,
            None,
            None,
            None,
        );

        let create = resumed.tool_for_test("create_file").unwrap();
        let exit = resumed.tool_for_test("exit_worktree").unwrap();
        let created = create
            .call(json!({ "path": "src/resumed.rs", "content": "from resumed worktree" }))
            .await;
        assert!(!created.is_error, "{}", created.content);

        let worktree = repo.path().join(".ignis/worktrees/resume-wt");
        assert!(!subdir.join("src/resumed.rs").exists());
        assert_eq!(
            std::fs::read_to_string(worktree.join("crates/app/src/resumed.rs")).unwrap(),
            "from resumed worktree"
        );

        let removed = exit
            .call(json!({ "action": "remove", "discard_changes": true }))
            .await;
        assert!(!removed.is_error, "{}", removed.content);
        assert!(!worktree.exists());
    }
}
