mod bash;
mod create_file;
mod edit_file;
mod list_dir;
pub mod plugin;
mod read_file;
mod web_search;

pub use bash::BashTool;
pub use create_file::CreateFileTool;
pub use edit_file::EditFileTool;
pub use list_dir::ListDirTool;
pub use read_file::ReadFileTool;
pub use web_search::WebSearchTool;

pub fn register_native_tools(agent: &mut crate::Agent, cwd: &std::path::Path) {
    use std::sync::Arc;
    agent.register_tool(Arc::new(ReadFileTool::new(cwd)));
    agent.register_tool(Arc::new(CreateFileTool::new(cwd)));
    agent.register_tool(Arc::new(ListDirTool::new(cwd)));
    agent.register_tool(Arc::new(BashTool::new(cwd)));
    agent.register_tool(Arc::new(EditFileTool::new(cwd)));
    agent.register_tool(Arc::new(WebSearchTool::new()));
}
