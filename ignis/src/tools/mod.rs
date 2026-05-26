pub mod tool;

mod agent;
mod bash;
mod create_file;
mod edit_file;
mod glob;
mod grep;
mod list_dir;
mod read_file;
mod skill;
mod web_fetch;
mod web_search;

pub use agent::SubagentTool;
pub use bash::BashTool;
pub use create_file::CreateFileTool;
pub use edit_file::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
pub use read_file::ReadFileTool;
pub use skill::SkillTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;

use crate::tools::tool::AgentTool;
use std::path::Path;
use std::sync::Arc;

/// The base native toolset shared by the main agent and sub-agents (everything
/// except the `agent` tool itself, so sub-agents don't nest).
pub fn native_tools(
    cwd: &Path,
    web_search: crate::config::WebSearchConfig,
) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(ReadFileTool::new(cwd)) as Arc<dyn AgentTool>,
        Arc::new(CreateFileTool::new(cwd)),
        Arc::new(EditFileTool::new(cwd)),
        Arc::new(ListDirTool::new(cwd)),
        Arc::new(GrepTool::new(cwd)),
        Arc::new(GlobTool::new(cwd)),
        Arc::new(BashTool::new(cwd)),
        Arc::new(WebFetchTool::new()),
        Arc::new(WebSearchTool::new(web_search.provider, web_search.api_key)),
    ]
}

pub fn register_native_tools(
    session: &mut crate::Session,
    cwd: &Path,
    config: &crate::config::Config,
) {
    register_native_tools_with_mcp(session, cwd, config, None)
}

/// Same as `register_native_tools` but also threads a shared MCP registry into
/// the `SubagentTool` so sub-agents inherit MCP tools.
pub fn register_native_tools_with_mcp(
    session: &mut crate::Session,
    cwd: &Path,
    config: &crate::config::Config,
    mcp: Option<Arc<crate::mcp::McpRegistry>>,
) {
    for tool in native_tools(cwd, config.web_search.clone()) {
        session.register_tool(tool);
    }
    // The `agent` tool builds sub-agents from the config; registered only at the
    // top level so sub-agents can't recurse.
    let mut subagent = SubagentTool::new(config.clone(), cwd);
    if let Some(mcp) = mcp {
        subagent = subagent.with_mcp(mcp);
    }
    session.register_tool(Arc::new(subagent));
}

/// Register every tool exposed by a connected MCP server as an `AgentTool`.
/// Disabled or failed servers contribute nothing — the registry knows.
pub fn register_mcp_tools(session: &mut crate::Session, registry: &crate::mcp::McpRegistry) {
    for wrapper in registry.wrappers() {
        session.register_tool(wrapper as Arc<dyn AgentTool>);
    }
}
