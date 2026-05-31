# MCP servers

Ignis can spawn external [Model Context Protocol](https://modelcontextprotocol.io)
servers over stdio at startup and surface every tool they advertise to the
model alongside the built-ins. Today the integration is **stdio + tools only**
— HTTP/SSE transports, resources, prompts, and per-tool approval gates are not
in v1.

## Adding a server

The fastest path is the CLI — it edits `~/.ignis/config.toml` in place
(comments and surrounding formatting preserved):

```bash
ignis mcp add github -- gh mcp
ignis mcp add fs -e ROOT=/srv/data -- npx -y @modelcontextprotocol/server-filesystem
```

Everything after `--` is the command and its arguments. Flags:

| Flag | Purpose |
|---|---|
| `-e KEY=VALUE` (repeatable) | Set an environment variable for the child. |
| `--cwd <path>` | Working directory for the child process. |
| `--startup-timeout-secs <n>` | Bound on the `initialize` handshake (default `30`). |
| `--tool-timeout-secs <n>` | Bound on each `tools/call` (default `120`). |
| `--force` | Overwrite an existing entry of the same name. |

Equivalent hand-written TOML:

```toml
[mcp.servers.github]
command = "gh"
args    = ["mcp"]

[mcp.servers.fs]
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem"]
env     = { ROOT = "/srv/data" }
startup_timeout_secs = 30
tool_timeout_secs    = 120
```

**Server names** must match `[a-zA-Z0-9_-]{1,40}` — the 40-char cap leaves
room for the `mcp__<server>__<tool>` qualified name to stay under the 64-char
OpenAI tool-name limit.

## How tools surface to the model

Each MCP tool is wrapped and presented as `mcp__<server>__<tool>`. Tool names
beyond the 64-char OpenAI limit are sanitized (truncated + hashed); use
`ignis mcp get <server>` to see what the model actually sees.

If a server returns a non-empty `instructions` field at `initialize`, ignis
folds the text of every connected server into a single block and injects it
into the system prompt — so the model gets the server's own usage notes
without you wiring anything up.

## Inspecting + managing

```bash
ignis mcp list              # one-row-per-server: status, tool count, command
ignis mcp list --json       # machine-readable
ignis mcp list --no-connect # config view only — don't spawn anything

ignis mcp get <name>        # config + connect probe + tool list (and instructions)
ignis mcp remove <name>     # drops from config.toml + clears the disabled flag

ignis mcp enable  <name>    # clear the runtime disable flag
ignis mcp disable <name>    # keep the config entry, don't connect next run
```

`enable` / `disable` only edit `~/.ignis/state.json` — the config entry stays
put, so toggling is reversible and cheap.

## Runtime control: `/mcp`

Inside the TUI, the **`/mcp`** picker is the equivalent of
`ignis mcp enable/disable` — toggle any configured server on or off. Disabled
servers don't spawn at startup, their tools don't appear in the catalog, and
their `instructions` block is omitted. The state persists to
`~/.ignis/state.json`.

## Lifecycle + errors

- **Eager startup.** All enabled servers spawn during ignis startup; the
  `initialize` handshake is bounded by `startup_timeout_secs`.
- **A server that fails to start does not block ignis.** Its row in
  `ignis mcp list` shows the failure; other servers and the built-in tools
  keep working.
- **Tool calls** are bounded by `tool_timeout_secs`. A timeout returns an
  error to the model but does not tear the connection down.

## What's intentionally not in v1

These were dropped during design (per the MCP design memo) so the integration
ships small and predictable:

- **HTTP / SSE transports** — stdio only.
- **MCP resources and prompts** — only `tools/*` surfaces are wired.
- **Per-tool permission rules** — the existing
  [permissions](permissions.md) system gates the wrappers as a group; per-tool
  allow/deny is not yet supported.
- **OAuth / interactive auth flows** — provide credentials via `-e KEY=VALUE`
  or the server's own config file.
