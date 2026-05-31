//! `ignis mcp …` subcommand. The parent `Cli` in `cli/mod.rs` owns all
//! top-level clap parsing and dispatches here via `Command::Mcp(McpCmd)`,
//! so this file only declares the subcommand shape and its handlers.
//!
//! Mutations to `~/.ignis/config.toml` go through `toml_edit`, which preserves
//! the user's comments and surrounding formatting; mutations to
//! `~/.ignis/state.json` go through `state::persist_disabled_mcp_servers`,
//! which already does a read-modify-write that keeps siblings intact.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use toml_edit::{value, Array, DocumentMut, InlineTable, Item, Table};

use crate::config::{validate_mcp_server_name, McpServerConfig};
use crate::mcp::{McpRegistry, McpStatus};
use crate::state;
use crate::tools::tool::AgentTool;

// The parent `Cli` (in `cli/mod.rs`) owns naming and routes here through the
// `Command::Mcp(McpCmd)` variant, so we don't set `name`/`about` again.
#[derive(Debug, Parser)]
pub struct McpCmd {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Add an MCP server (stdio or HTTP) to ~/.ignis/config.toml.
    Add(AddArgs),
    /// List configured MCP servers and their status.
    List(ListArgs),
    /// Show config + connect-test + tool list for one server.
    Get { name: String },
    /// Remove an MCP server from config.toml (also drops the disabled flag).
    Remove { name: String },
    /// Enable an MCP server (clears the runtime disable flag).
    Enable { name: String },
    /// Disable an MCP server (keeps the config entry, just doesn't connect).
    Disable { name: String },
}

/// Add a server. Pass either `--url <URL>` (Streamable HTTP) or `-- <command>
/// [args...]` (stdio). The clap `ArgGroup` makes them mutually exclusive and
/// requires exactly one.
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("transport")
        .required(true)
        .args(["url", "command"]),
))]
pub struct AddArgs {
    /// Server name (config key). Must match [a-zA-Z0-9_-]{1,40}.
    pub name: String,

    // -- HTTP transport --
    /// HTTP MCP server endpoint, e.g. `https://mcp.stripe.com`.
    #[arg(long)]
    pub url: Option<String>,
    /// Repeatable: `--header "Name: value"`. Non-secret values only — for
    /// secrets prefer `--bearer-token-env-var`.
    #[arg(long = "header", value_parser = parse_header, value_name = "NAME:VALUE")]
    pub headers: Vec<(String, String)>,
    /// Env var name holding a bearer token. ignis reads it at connect time and
    /// sends `Authorization: Bearer <value>`.
    #[arg(long, value_name = "ENV_VAR")]
    pub bearer_token_env_var: Option<String>,

    // -- stdio transport --
    /// Environment variables for the child process, repeatable: `-e KEY=VALUE`.
    #[arg(short = 'e', long = "env", value_parser = parse_key_val, value_name = "KEY=VALUE")]
    pub env: Vec<(String, String)>,
    /// Working directory for the child process.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    // -- shared --
    /// Initialize handshake timeout (seconds). Default 30.
    #[arg(long)]
    pub startup_timeout_secs: Option<u64>,
    /// Per-tool-call timeout (seconds). Default 120.
    #[arg(long)]
    pub tool_timeout_secs: Option<u64>,
    /// Overwrite an existing entry with the same name.
    #[arg(long)]
    pub force: bool,

    /// The command and its arguments (stdio). Everything after `--` is captured
    /// here. Omit when using `--url`.
    #[arg(last = true, num_args = 0.., value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Skip the connect probe and only report config / disabled flags.
    #[arg(long)]
    pub no_connect: bool,
}

fn parse_key_val(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected KEY=VALUE, got `{s}`"))
}

/// Parse `--header "Name: value"`. Splits on the FIRST `:` and trims one
/// optional space after it (matches HTTP convention).
fn parse_header(s: &str) -> Result<(String, String), String> {
    let (name, value) = s
        .split_once(':')
        .ok_or_else(|| format!("expected `Name: value`, got `{s}`"))?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(format!("header name is empty in `{s}`"));
    }
    let value = value.strip_prefix(' ').unwrap_or(value).to_string();
    Ok((name, value))
}

/// Entry point. `cmd` is the already-parsed `McpCmd` from the parent `Cli`.
pub async fn run(cmd: McpCmd) -> Result<()> {
    match cmd.cmd {
        Cmd::Add(args) => cmd_add(args),
        Cmd::List(args) => cmd_list(args).await,
        Cmd::Get { name } => cmd_get(name).await,
        Cmd::Remove { name } => cmd_remove(name),
        Cmd::Enable { name } => cmd_toggle(name, true),
        Cmd::Disable { name } => cmd_toggle(name, false),
    }
}

fn config_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("could not determine home directory"))?
        .join(".ignis/config.toml"))
}

fn load_doc(path: &Path) -> Result<DocumentMut> {
    let content = if path.exists() {
        std::fs::read_to_string(path)
            .map_err(|e| anyhow!("failed to read {}: {e}", path.display()))?
    } else {
        String::new()
    };
    content
        .parse::<DocumentMut>()
        .map_err(|e| anyhow!("failed to parse {}: {e}", path.display()))
}

fn save_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, doc.to_string())
        .map_err(|e| anyhow!("failed to write {}: {e}", path.display()))
}

fn cmd_add(args: AddArgs) -> Result<()> {
    validate_mcp_server_name(&args.name)?;
    let path = config_path()?;
    let mut doc = load_doc(&path)?;
    let servers = ensure_servers_table(&mut doc)?;
    if servers.contains_key(&args.name) && !args.force {
        return Err(anyhow!(
            "MCP server '{}' already exists in {}; pass --force to overwrite",
            args.name,
            path.display()
        ));
    }

    // Build a `McpServerConfig` from the flags and run the same `validate()`
    // the runtime loader uses. Catching mistakes here means a typo'd `--url`
    // or HTTP/stdio-flag mixing fails the CLI invocation — instead of writing
    // a config that breaks the next ignis launch.
    let (cmd_head, cmd_rest) = match args.command.split_first() {
        Some((h, r)) => (Some(h.clone()), r.to_vec()),
        None => (None, Vec::new()),
    };
    let headers = headers_to_map(&args.headers)?;
    let cfg = McpServerConfig {
        command: cmd_head,
        args: cmd_rest,
        env: args.env.iter().cloned().collect(),
        cwd: args.cwd.clone(),
        url: args.url.clone(),
        headers: headers.clone(),
        bearer_token_env_var: args.bearer_token_env_var.clone(),
        startup_timeout_secs: args.startup_timeout_secs.unwrap_or(30),
        tool_timeout_secs: args.tool_timeout_secs.unwrap_or(120),
    };
    cfg.validate(&args.name)?;

    let mut block = Table::new();
    block.set_implicit(false);
    if cfg.url.is_some() {
        block["url"] = value(cfg.url.as_deref().unwrap());
        if let Some(ev) = &cfg.bearer_token_env_var {
            block["bearer_token_env_var"] = value(ev);
        }
        if !headers.is_empty() {
            let mut hdrs = InlineTable::new();
            // Preserve CLI-given header order (stable for round-trip diffs)
            // by iterating the original Vec, which `headers_to_map` validated.
            for (k, v) in &args.headers {
                hdrs.insert(k, v.into());
            }
            block["headers"] = value(hdrs);
        }
    } else {
        block["command"] = value(cfg.command.as_deref().unwrap());
        if !cfg.args.is_empty() {
            let mut arr = Array::new();
            for a in &cfg.args {
                arr.push(a.clone());
            }
            block["args"] = value(arr);
        }
        if !cfg.env.is_empty() {
            let mut env = InlineTable::new();
            for (k, v) in &args.env {
                env.insert(k, v.into());
            }
            block["env"] = value(env);
        }
        if let Some(cwd) = &cfg.cwd {
            block["cwd"] = value(cwd.to_string_lossy().to_string());
        }
    }
    if let Some(t) = args.startup_timeout_secs {
        block["startup_timeout_secs"] = value(t as i64);
    }
    if let Some(t) = args.tool_timeout_secs {
        block["tool_timeout_secs"] = value(t as i64);
    }
    servers.insert(&args.name, Item::Table(block));
    save_doc(&path, &doc)?;
    println!("✓ added '{}' to {}", args.name, path.display());
    Ok(())
}

/// Fold `--header K:V` pairs into a `HashMap`, rejecting duplicate keys
/// (case-insensitive — `X-Foo` and `x-foo` are the same HTTP header). Without
/// this, the second occurrence would silently win in the `InlineTable::insert`
/// loop downstream, which is a real footgun for Set-Cookie / X-Forwarded-* etc.
fn headers_to_map(pairs: &[(String, String)]) -> Result<HashMap<String, String>> {
    let mut out: HashMap<String, String> = HashMap::with_capacity(pairs.len());
    let mut seen_lower: HashMap<String, String> = HashMap::with_capacity(pairs.len());
    for (k, v) in pairs {
        let key_lower = k.to_ascii_lowercase();
        if let Some(prev) = seen_lower.insert(key_lower, k.clone()) {
            return Err(anyhow!(
                "duplicate `--header` key: `{prev}` and `{k}` collide (HTTP headers are case-insensitive)"
            ));
        }
        out.insert(k.clone(), v.clone());
    }
    Ok(out)
}

fn cmd_remove(name: String) -> Result<()> {
    let path = config_path()?;
    let mut doc = load_doc(&path)?;
    let servers = ensure_servers_table(&mut doc)?;
    if servers.remove(&name).is_none() {
        return Err(anyhow!(
            "MCP server '{}' not found in {}",
            name,
            path.display()
        ));
    }
    save_doc(&path, &doc)?;
    // Also drop from the disabled set so re-adding later doesn't ghost-disable.
    let mut disabled: Vec<String> = state::load_state()
        .disabled_mcp_servers
        .into_iter()
        .filter(|n| n != &name)
        .collect();
    disabled.sort();
    state::persist_disabled_mcp_servers(&disabled)?;
    println!("✓ removed '{}' from {}", name, path.display());
    Ok(())
}

fn cmd_toggle(name: String, enable: bool) -> Result<()> {
    let cfg = crate::config::load_config()?;
    if !cfg.mcp.servers.contains_key(&name) {
        return Err(anyhow!(
            "MCP server '{name}' is not configured in ~/.ignis/config.toml"
        ));
    }
    let mut disabled: HashSet<String> = state::load_state()
        .disabled_mcp_servers
        .into_iter()
        .collect();
    let changed = if enable {
        disabled.remove(&name)
    } else {
        disabled.insert(name.clone())
    };
    let mut snapshot: Vec<String> = disabled.into_iter().collect();
    snapshot.sort();
    state::persist_disabled_mcp_servers(&snapshot)?;
    let verb = if enable { "enabled" } else { "disabled" };
    let suffix = if changed { "" } else { " (was already)" };
    println!("✓ '{name}' {verb}{suffix} (effective next ignis run)");
    Ok(())
}

async fn cmd_list(args: ListArgs) -> Result<()> {
    let cfg = crate::config::load_config()?;
    if cfg.mcp.servers.is_empty() {
        if args.json {
            println!("[]");
        } else {
            println!("No MCP servers configured in ~/.ignis/config.toml.");
        }
        return Ok(());
    }
    let disabled: HashSet<String> = state::load_state()
        .disabled_mcp_servers
        .into_iter()
        .collect();
    let registry = if args.no_connect {
        McpRegistry::empty()
    } else {
        // Re-spawn just for this command; tear down when we're done.
        McpRegistry::spawn_all(&cfg.mcp.servers, disabled.clone()).await
    };

    // Build a friendly list: include every configured server, even if no_connect.
    let mut rows: Vec<Row> = if args.no_connect {
        let mut names: Vec<&String> = cfg.mcp.servers.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| Row {
                name: name.clone(),
                transport: cfg.mcp.servers[name].transport(),
                status: if disabled.contains(name) {
                    "disabled".to_string()
                } else {
                    "(not connected — --no-connect)".to_string()
                },
                tools: 0,
                target: target_line(&cfg.mcp.servers[name]),
            })
            .collect()
    } else {
        registry
            .entries()
            .into_iter()
            .map(|e| {
                let tools = match &e.status {
                    McpStatus::Connected { tool_count } => *tool_count,
                    _ => 0,
                };
                Row {
                    name: e.name.clone(),
                    transport: cfg.mcp.servers[&e.name].transport(),
                    status: e.status.label(),
                    tools,
                    target: target_line(&cfg.mcp.servers[&e.name]),
                }
            })
            .collect()
    };
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    if args.json {
        let json: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "transport": r.transport,
                    "status": r.status,
                    "tools": r.tools,
                    "target": r.target,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        print_table(&rows);
    }
    registry.shutdown().await;
    Ok(())
}

async fn cmd_get(name: String) -> Result<()> {
    let cfg = crate::config::load_config()?;
    let server_cfg = cfg
        .mcp
        .servers
        .get(&name)
        .ok_or_else(|| anyhow!("MCP server '{name}' not configured"))?
        .clone();
    let disabled: HashSet<String> = state::load_state()
        .disabled_mcp_servers
        .into_iter()
        .collect();
    let is_disabled = disabled.contains(&name);

    println!("name: {name}");
    println!("transport: {}", server_cfg.transport());
    match server_cfg.transport() {
        "stdio" => {
            println!("command: {}", target_line(&server_cfg));
            if !server_cfg.env.is_empty() {
                println!("env:");
                for (k, v) in &server_cfg.env {
                    println!("  {k}={v}");
                }
            }
            if let Some(cwd) = &server_cfg.cwd {
                println!("cwd: {}", cwd.display());
            }
        }
        "http" => {
            println!("url: {}", server_cfg.url.as_deref().unwrap_or(""));
            if let Some(ev) = &server_cfg.bearer_token_env_var {
                println!("bearer_token_env_var: {ev}");
            }
            if !server_cfg.headers.is_empty() {
                println!("headers:");
                // Print keys only; never print values (secrets-in-terminal hazard).
                let mut keys: Vec<&String> = server_cfg.headers.keys().collect();
                keys.sort();
                for k in keys {
                    println!("  {k}: <set>");
                }
            }
        }
        _ => unreachable!("transport() returns only stdio/http"),
    }
    println!("startup_timeout_secs: {}", server_cfg.startup_timeout_secs);
    println!("tool_timeout_secs: {}", server_cfg.tool_timeout_secs);

    if is_disabled {
        println!("\nstatus: disabled (toggle with `ignis mcp enable {name}`)");
        return Ok(());
    }

    println!("\nconnecting…");
    let mut servers = HashMap::new();
    servers.insert(name.clone(), server_cfg);
    let registry = McpRegistry::spawn_all(&servers, HashSet::new()).await;
    let entry = registry
        .entries()
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| anyhow!("internal: '{name}' missing from registry"))?;
    println!("status: {}", entry.status.label());
    // Tools + instructions only meaningful when connected.
    if let McpStatus::Connected { .. } = &entry.status {
        if let Some(instr) = registry.instructions_block() {
            println!("\ninstructions:");
            println!("{instr}");
        }
        // The wrappers' qualified names are what the model sees.
        let wrappers = registry.wrappers();
        if !wrappers.is_empty() {
            println!("\ntools:");
            for w in wrappers {
                let qname = w.qualified_name();
                let desc = w.description();
                if desc.is_empty() {
                    println!("  {qname}");
                } else {
                    println!("  {qname}");
                    for line in desc.lines() {
                        println!("    {line}");
                    }
                }
            }
        }
    }
    registry.shutdown().await;
    Ok(())
}

struct Row {
    name: String,
    transport: &'static str,
    status: String,
    tools: usize,
    target: String,
}

/// Render the connection target for table display: command + args for stdio,
/// the URL for HTTP. Validation has already ensured exactly one is set.
fn target_line(cfg: &McpServerConfig) -> String {
    if let Some(url) = &cfg.url {
        return url.clone();
    }
    let cmd = cfg.command.as_deref().unwrap_or("");
    if cfg.args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, cfg.args.join(" "))
    }
}

fn print_table(rows: &[Row]) {
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    let tx_w = rows
        .iter()
        .map(|r| r.transport.len())
        .max()
        .unwrap_or(9)
        .max(9);
    let status_w = rows
        .iter()
        .map(|r| r.status.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let tools_w = rows
        .iter()
        .map(|r| r.tools.to_string().len())
        .max()
        .unwrap_or(5)
        .max(5);
    println!(
        "{:<name_w$}  {:<tx_w$}  {:<status_w$}  {:>tools_w$}  TARGET",
        "NAME",
        "TRANSPORT",
        "STATUS",
        "TOOLS",
        name_w = name_w,
        tx_w = tx_w,
        status_w = status_w,
        tools_w = tools_w,
    );
    for r in rows {
        println!(
            "{:<name_w$}  {:<tx_w$}  {:<status_w$}  {:>tools_w$}  {}",
            r.name,
            r.transport,
            r.status,
            r.tools,
            r.target,
            name_w = name_w,
            tx_w = tx_w,
            status_w = status_w,
            tools_w = tools_w,
        );
    }
}

/// Get-or-create the `[mcp.servers]` table inside the document, preserving any
/// surrounding content. Returns `Err` (not panic) if the user has put
/// something at `mcp` or `mcp.servers` that isn't a table — they get a clean
/// CLI error instead of a stack trace.
fn ensure_servers_table(doc: &mut DocumentMut) -> Result<&mut Table> {
    if !doc.as_table().contains_key("mcp") {
        let mut t = Table::new();
        t.set_implicit(true);
        doc["mcp"] = Item::Table(t);
    }
    let mcp = doc["mcp"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("`mcp` in config.toml is not a table"))?;
    if !mcp.contains_key("servers") {
        let mut t = Table::new();
        t.set_implicit(true);
        mcp["servers"] = Item::Table(t);
    }
    mcp["servers"]
        .as_table_mut()
        .ok_or_else(|| anyhow!("`mcp.servers` in config.toml is not a table"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_val_splits_first_equals() {
        assert_eq!(parse_key_val("FOO=bar"), Ok(("FOO".into(), "bar".into())));
        assert_eq!(
            parse_key_val("URL=postgres://a=b@h/db"),
            Ok(("URL".into(), "postgres://a=b@h/db".into()))
        );
        assert!(parse_key_val("FOO").is_err());
    }

    #[test]
    fn add_emits_a_well_formed_block_via_toml_edit() {
        // Use a stand-in DocumentMut so we don't need to touch ~/.ignis.
        let pre =
            "model = \"x/y\"\n# preserve me\n[providers.x]\napi_key = \"k\"\nmodels = [\"y\"]\n";
        let mut doc: DocumentMut = pre.parse().unwrap();
        let servers = ensure_servers_table(&mut doc).unwrap();
        let mut block = Table::new();
        block["command"] = value("gh");
        let mut arr = Array::new();
        arr.push("mcp".to_string());
        block["args"] = value(arr);
        servers.insert("github", Item::Table(block));
        let out = doc.to_string();
        // Pre-existing content preserved exactly.
        assert!(out.contains("model = \"x/y\""));
        assert!(out.contains("# preserve me"));
        assert!(out.contains("[providers.x]"));
        // New block exists.
        assert!(out.contains("[mcp.servers.github]"));
        assert!(out.contains("command = \"gh\""));
        assert!(out.contains("args = [\"mcp\"]"));
    }

    #[test]
    fn ensure_servers_table_returns_err_when_mcp_is_not_a_table() {
        // User typo: `mcp = "x"` or `mcp.servers = []` should produce a clean
        // CLI error, not panic.
        for bad in ["mcp = \"x\"\n", "[mcp]\nservers = []\n"] {
            let mut doc: DocumentMut = bad.parse().unwrap();
            let err = ensure_servers_table(&mut doc).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("not a table"),
                "expected `not a table` message for `{bad}`, got: {msg}"
            );
        }
    }

    #[test]
    fn ensure_servers_table_on_empty_doc_creates_block() {
        let mut doc: DocumentMut = "".parse().unwrap();
        let servers = ensure_servers_table(&mut doc).unwrap();
        servers.insert(
            "foo",
            Item::Table({
                let mut t = Table::new();
                t["command"] = value("bar");
                t
            }),
        );
        let out = doc.to_string();
        assert!(out.contains("[mcp.servers.foo]"));
        assert!(out.contains("command = \"bar\""));
    }
}
