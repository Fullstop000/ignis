//! MCP (Model Context Protocol) client integration. Spawns user-configured
//! stdio MCP servers at session start, exposes their tools to the model as
//! `mcp__<server>__<tool>`, and folds each server's `instructions` field into
//! the system prompt.
//!
//! Public surface is [`McpRegistry`]; `server` and `tool` are implementation
//! details consumed inside the crate.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::config::McpServerConfig;

pub mod http;
pub mod server;
pub mod tool;

pub use server::McpServer;
pub use tool::{sanitize_tool_name, McpToolWrapper};

/// Hard cap on the qualified MCP tool name (`mcp__<server>__<tool>`). Matches
/// OpenAI's `^[a-zA-Z0-9_-]{1,64}$` for function names; oversize tools are
/// dropped (with a warning) instead of being silently truncated.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Why a server isn't currently usable (anything other than "connected").
#[derive(Debug, Clone)]
pub enum McpStatus {
    Connected { tool_count: usize },
    Failed { reason: String },
    Disabled,
}

impl McpStatus {
    pub fn label(&self) -> String {
        match self {
            McpStatus::Connected { tool_count } => format!("connected · {tool_count} tools"),
            McpStatus::Failed { reason } => format!("failed: {reason}"),
            McpStatus::Disabled => "disabled".to_string(),
        }
    }
}

/// One entry in the `/mcp` picker and `ignis mcp list` output.
#[derive(Debug, Clone)]
pub struct McpServerEntry {
    pub name: String,
    /// `"stdio"` or `"http"` — surfaced in the picker so users can tell at a
    /// glance which transport a server uses.
    pub transport: &'static str,
    pub status: McpStatus,
}

/// All MCP servers discovered for this session — connected, failed, and
/// disabled. The connected ones contribute tools (via [`McpRegistry::wrappers`])
/// and instructions (via [`McpRegistry::instructions_block`]).
pub struct McpRegistry {
    /// Connected servers by config-key name. Each `Arc<McpServer>` is
    /// shared with every `McpToolWrapper` that fronts one of its tools.
    connected: HashMap<String, Arc<McpServer>>,
    /// Servers that failed to start, with the reason text.
    failed: HashMap<String, String>,
    /// Live disabled set — toggling persists to `state.json`.
    disabled: Mutex<HashSet<String>>,
    /// Names of every server present in config (connected, failed, or
    /// disabled), kept for stable ordering in pickers/CLI output.
    all_names: Vec<String>,
    /// Captured-at-spawn transport ("stdio"/"http") per server name. Used by
    /// pickers and CLI output to label rows without re-reading the config.
    transports: HashMap<String, &'static str>,
}

impl McpRegistry {
    /// An empty registry — useful when no MCP servers are configured (zero
    /// runtime cost) and in tests.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            connected: HashMap::new(),
            failed: HashMap::new(),
            disabled: Mutex::new(HashSet::new()),
            all_names: Vec::new(),
            transports: HashMap::new(),
        })
    }

    /// Spawn every enabled server in parallel; bound each `initialize` by the
    /// per-server `startup_timeout_secs`. Servers that time out or error are
    /// recorded under `failed` — they never block startup, never crash ignis.
    pub async fn spawn_all(
        servers: &HashMap<String, McpServerConfig>,
        disabled: HashSet<String>,
    ) -> Arc<Self> {
        // Deterministic order in pickers/CLI output.
        let mut all_names: Vec<String> = servers.keys().cloned().collect();
        all_names.sort();

        let mut tasks: Vec<tokio::task::JoinHandle<(String, ConnectResult)>> = Vec::new();
        for name in &all_names {
            if disabled.contains(name) {
                continue;
            }
            let name = name.clone();
            let cfg = servers[&name].clone();
            tasks.push(tokio::spawn(async move {
                let res = connect_one(&name, &cfg).await;
                (name, res)
            }));
        }

        let mut connected = HashMap::new();
        let mut failed = HashMap::new();
        for handle in tasks {
            match handle.await {
                Ok((name, ConnectResult::Connected(server))) => {
                    connected.insert(name, Arc::from(server));
                }
                Ok((name, ConnectResult::Failed(reason))) => {
                    log::warn!("MCP server `{name}` failed to start: {reason}");
                    failed.insert(name, reason);
                }
                Err(join_err) => {
                    log::warn!("MCP startup task panicked: {join_err}");
                }
            }
        }

        let transports = servers
            .iter()
            .map(|(n, c)| (n.clone(), c.transport()))
            .collect();
        Arc::new(Self {
            connected,
            failed,
            disabled: Mutex::new(disabled),
            all_names,
            transports,
        })
    }

    /// Total servers known (connected + failed + disabled).
    pub fn len(&self) -> usize {
        self.all_names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.all_names.is_empty()
    }

    /// Snapshot of every known server with its current status. Stable order:
    /// matches `all_names` (sorted alphabetically at spawn time).
    pub fn entries(&self) -> Vec<McpServerEntry> {
        let disabled = self.disabled.lock().unwrap();
        self.all_names
            .iter()
            .map(|name| {
                let status = if disabled.contains(name) {
                    McpStatus::Disabled
                } else if let Some(server) = self.connected.get(name) {
                    McpStatus::Connected {
                        tool_count: server.tools().len(),
                    }
                } else if let Some(reason) = self.failed.get(name) {
                    McpStatus::Failed {
                        reason: reason.clone(),
                    }
                } else {
                    // In `disabled` set but not actually disabled? defensive default.
                    McpStatus::Disabled
                };
                McpServerEntry {
                    name: name.clone(),
                    transport: self.transports.get(name).copied().unwrap_or("stdio"),
                    status,
                }
            })
            .collect()
    }

    /// Toggle a server's enabled state, persist the new disabled set, and
    /// return whether it is now enabled. Note: takes effect on next ignis run
    /// (we don't (re)spawn the server mid-session in v1).
    pub fn toggle(&self, name: &str) -> bool {
        let (now_enabled, snapshot) = {
            let mut d = self.disabled.lock().unwrap();
            let now_enabled = if d.remove(name) {
                true
            } else {
                d.insert(name.to_string());
                false
            };
            let mut snapshot: Vec<String> = d.iter().cloned().collect();
            snapshot.sort();
            (now_enabled, snapshot)
        };
        if let Err(e) = crate::state::persist_disabled_mcp_servers(&snapshot) {
            log::warn!("failed to persist MCP disabled set: {e}");
        }
        now_enabled
    }

    /// Every `(McpToolWrapper)` from currently connected servers. The agent
    /// registers these alongside its native tools at session build time.
    ///
    /// Tools are skipped (with a warning) if their qualified name would
    /// exceed OpenAI's 64-character tool-name limit or collide with another
    /// tool from the same server after sanitization (e.g. `read.file` and
    /// `read/file` both sanitize to `read_file`).
    pub fn wrappers(&self) -> Vec<Arc<McpToolWrapper>> {
        let mut out: Vec<Arc<McpToolWrapper>> = Vec::new();
        // Iterate `all_names` (sorted) so wrappers come out in a stable order.
        for name in &self.all_names {
            let Some(server) = self.connected.get(name) else {
                continue;
            };
            let mut seen: HashSet<String> = HashSet::new();
            for tool in server.tools() {
                let real = tool.name.to_string();
                let sanitized = tool::sanitize_tool_name(&real);
                let qualified = format!("mcp__{name}__{sanitized}");
                if qualified.len() > MAX_TOOL_NAME_LEN {
                    log::warn!(
                        "MCP tool `{name}/{real}` skipped: qualified name `{qualified}` exceeds {MAX_TOOL_NAME_LEN}-char limit"
                    );
                    continue;
                }
                if !seen.insert(qualified.clone()) {
                    log::warn!(
                        "MCP tool `{name}/{real}` skipped: name `{qualified}` collides with another tool from server `{name}`"
                    );
                    continue;
                }
                let description = tool
                    .description
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                let schema = serde_json::Value::Object((*tool.input_schema).clone());
                out.push(Arc::new(McpToolWrapper::new(
                    server.clone(),
                    name,
                    real,
                    description,
                    schema,
                )));
            }
        }
        out
    }

    /// Short tool names (sanitized, NO `mcp__<server>__` prefix) advertised by
    /// a connected server, in advertised order. Empty when the server isn't
    /// connected. Skips tools that wouldn't actually be exposed to the model
    /// — same filter as [`Self::wrappers`].
    pub fn mcp_tool_list(&self, name: &str) -> Vec<String> {
        let Some(server) = self.connected.get(name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for tool in server.tools() {
            let sanitized = tool::sanitize_tool_name(&tool.name);
            let qualified_len = "mcp__".len() + name.len() + "__".len() + sanitized.len();
            if qualified_len > MAX_TOOL_NAME_LEN {
                continue;
            }
            if !seen.insert(sanitized.clone()) {
                continue;
            }
            out.push(sanitized);
        }
        out
    }

    /// `<mcp_servers>` block for the system prompt, listing each connected
    /// server's `instructions` text. Returns `None` if no server has
    /// instructions (so we don't emit an empty wrapper).
    pub fn instructions_block(&self) -> Option<String> {
        let mut entries: BTreeMap<&str, &str> = BTreeMap::new();
        for name in &self.all_names {
            if let Some(server) = self.connected.get(name) {
                if let Some(text) = server.instructions() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        entries.insert(name.as_str(), trimmed);
                    }
                }
            }
        }
        if entries.is_empty() {
            return None;
        }
        let mut out = String::new();
        out.push_str("The following MCP servers have provided instructions for ");
        out.push_str("how to use their tools.\n\n<mcp_servers>\n");
        for (name, text) in entries {
            out.push_str(&format!("  <server name=\"{name}\">\n"));
            for line in text.lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
            out.push_str("  </server>\n");
        }
        out.push_str("</mcp_servers>");
        Some(out)
    }

    /// Cancel every live rmcp service in parallel, then SIGTERM/SIGKILL each
    /// child's process group on Unix to catch any descendants left behind by
    /// shell-/`npx`-style wrappers. Takes `&self` so the caller doesn't need
    /// to own the only `Arc` (sub-agents and tool wrappers hold clones).
    ///
    /// Safe to call multiple times and against an empty registry.
    pub async fn shutdown(&self) {
        let mut handles = Vec::with_capacity(self.connected.len());
        for server in self.connected.values() {
            let server = server.clone();
            handles.push(tokio::spawn(async move { server.shutdown().await }));
        }
        for h in handles {
            let _ = h.await;
        }
    }
}

enum ConnectResult {
    // Boxed because `McpServer` carries the full rmcp service + mutex, which
    // dwarfs the `Failed(String)` variant; clippy warns on size disparity.
    Connected(Box<McpServer>),
    Failed(String),
}

/// Spawn one stdio MCP server, run the initialize handshake (bounded by
/// `startup_timeout_secs`), and fetch its tool list. Both bounded by the
/// timeout — a server that hangs the handshake or the first `list_tools` is
/// reported as `Failed`.
async fn connect_one(name: &str, cfg: &McpServerConfig) -> ConnectResult {
    let timeout = std::time::Duration::from_secs(cfg.startup_timeout_secs);
    let attempt = async {
        // Build the rmcp service over the right transport. `child_pid` is only
        // captured for stdio (Unix-only SIGTERM fallback in McpServer::shutdown);
        // HTTP transports leave it `None`.
        let (service, child_pid) = if cfg.url.is_some() {
            let svc = http::connect_streamable_http(cfg).await?;
            (svc, None)
        } else {
            connect_stdio(name, cfg).await?
        };

        let init = service.peer_info();
        let instructions = init.and_then(|r| r.instructions.clone());

        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| format!("list_tools failed: {e}"))?;

        Ok::<_, String>(Box::new(McpServer::new(
            name.to_string(),
            service,
            child_pid,
            tools,
            instructions,
            std::time::Duration::from_secs(cfg.tool_timeout_secs),
        )))
    };

    match tokio::time::timeout(timeout, attempt).await {
        Err(_) => ConnectResult::Failed(format!(
            "startup timeout after {}s",
            cfg.startup_timeout_secs
        )),
        Ok(Err(reason)) => ConnectResult::Failed(reason),
        Ok(Ok(server)) => ConnectResult::Connected(server),
    }
}

/// Spawn the child process and run the stdio `initialize` handshake. Returns
/// the rmcp service plus the immediate child PID (used by
/// [`McpServer::shutdown`] on Unix to terminate descendant process groups).
async fn connect_stdio(
    name: &str,
    cfg: &McpServerConfig,
) -> Result<
    (
        rmcp::service::RunningService<rmcp::RoleClient, ()>,
        Option<u32>,
    ),
    String,
> {
    let command = cfg
        .command
        .as_deref()
        .expect("validated: McpServerConfig.command is set on the stdio path");
    let mut cmd = Command::new(command);
    cmd.args(&cfg.args).envs(&cfg.env);
    if let Some(dir) = &cfg.cwd {
        cmd.current_dir(dir);
    }
    // `kill_on_drop` is set by rmcp's TokioChildProcess wrapper already, but a
    // Unix process group lets us terminate descendants too.
    #[cfg(unix)]
    cmd.process_group(0);

    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn error: {e}"))?;

    // Capture the PID before handing the transport off to rmcp's `serve`,
    // which consumes it.
    let child_pid = transport.id();

    // Forward server stderr to ignis's log; named per-server. Lives until the
    // child exits.
    if let Some(stderr) = stderr {
        let server_name = name.to_string();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                log::debug!(target: "mcp", "[{server_name}] {line}");
            }
        });
    }

    let service = ().serve(transport).await.map_err(|e| format!("initialize failed: {e}"))?;
    Ok((service, child_pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_no_tools_no_instructions_no_entries() {
        let reg = McpRegistry::empty();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.wrappers().is_empty());
        assert!(reg.instructions_block().is_none());
        assert!(reg.entries().is_empty());
    }

    #[test]
    fn status_labels_are_human_readable() {
        assert_eq!(
            McpStatus::Connected { tool_count: 3 }.label(),
            "connected · 3 tools"
        );
        assert_eq!(
            McpStatus::Failed {
                reason: "spawn error: not found".to_string()
            }
            .label(),
            "failed: spawn error: not found"
        );
        assert_eq!(McpStatus::Disabled.label(), "disabled");
    }
}
