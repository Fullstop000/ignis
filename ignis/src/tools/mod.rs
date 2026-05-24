pub mod tool;

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

pub fn register_native_tools(
    session: &mut crate::Session,
    cwd: &std::path::Path,
    web_search: crate::config::WebSearchConfig,
) {
    use std::sync::Arc;
    session.register_tool(Arc::new(ReadFileTool::new(cwd)));
    session.register_tool(Arc::new(CreateFileTool::new(cwd)));
    session.register_tool(Arc::new(ListDirTool::new(cwd)));
    session.register_tool(Arc::new(BashTool::new(cwd)));
    session.register_tool(Arc::new(EditFileTool::new(cwd)));
    session.register_tool(Arc::new(WebSearchTool::new(
        web_search.provider,
        web_search.api_key,
    )));
}
