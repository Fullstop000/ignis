# MCP servers

Ignis can connect to external [Model Context Protocol](https://modelcontextprotocol.io)
servers at startup and surface every tool they advertise to the model
alongside the built-ins. Two transports are supported:

- **stdio** — ignis spawns a child process and talks JSON-RPC over its
  stdin/stdout. Best for local servers (`npx @modelcontextprotocol/server-*`,
  `gh mcp`, …).
- **HTTP** — Streamable HTTP (MCP spec 2025-06-18), a single endpoint that
  multiplexes JSON-RPC POSTs and a server→client SSE channel. Best for
  remote SaaS servers (e.g. `https://mcp.stripe.com`). **Legacy HTTP+SSE
  (2024-11-05) and OAuth are not supported in this release** — bearer-token
  auth covers the common remote case.

## Adding a server

The fastest path is the CLI — it edits `~/.ignis/config.toml` in place
(comments and surrounding formatting preserved). Pass either `-- <command>
[args...]` for a stdio server, or `--url <URL>` for an HTTP server:

```bash
# stdio
ignis mcp add github -- gh mcp
ignis mcp add fs -e ROOT=/srv/data -- npx -y @modelcontextprotocol/server-filesystem

# HTTP, with a bearer token from the env (`STRIPE_API_KEY` must be set
# when ignis starts; read at connect time and sent as `Authorization: Bearer …`)
ignis mcp add stripe --url https://mcp.stripe.com \
                     --bearer-token-env-var STRIPE_API_KEY

# HTTP, with literal non-secret headers (repeat `--header`)
ignis mcp add corp --url https://mcp.corp.example.com \
                   --header "X-Tenant: acme" \
                   --header "X-Region: us-east-1"
```

Shared flags:

| Flag | Purpose |
|---|---|
| `--startup-timeout-secs <n>` | Bound on the `initialize` handshake (default `30`). |
| `--tool-timeout-secs <n>` | Bound on each `tools/call` (default `120`). |
| `--force` | Overwrite an existing entry of the same name. |

stdio-only flags:

| Flag | Purpose |
|---|---|
| `-e KEY=VALUE` (repeatable) | Set an environment variable for the child. |
| `--cwd <path>` | Working directory for the child process. |

HTTP-only flags:

| Flag | Purpose |
|---|---|
| `--url <URL>` | Server endpoint, must be `http://` or `https://`. |
| `--header "K: V"` (repeatable) | Non-secret HTTP header. Duplicate names are rejected. |
| `--bearer-token-env-var <ENV>` | Read this env var at connect time and send `Authorization: Bearer <value>`. Mutually exclusive with `--header Authorization: …`. |

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

[mcp.servers.stripe]
url = "https://mcp.stripe.com"
bearer_token_env_var = "STRIPE_API_KEY"

[mcp.servers.corp]
url     = "https://mcp.corp.example.com"
headers = { X-Tenant = "acme", X-Region = "us-east-1" }
```

**Transport discriminator: presence-based.** Set `command` *or* `url`, never
both. Setting both, or mixing stdio-only fields (`args`, `env`, `cwd`) with
HTTP-only fields (`headers`, `bearer_token_env_var`), fails config load with a
clear error — and `ignis mcp add` runs the same check before writing the TOML,
so a typo at the CLI never produces a config that breaks the next launch.

**Server names** must match `[a-zA-Z0-9_-]{1,40}` — the 40-char cap leaves
room for the `mcp__<server>__<tool>` qualified name to stay under the 64-char
OpenAI tool-name limit.

**Secrets in headers.** Only use `--bearer-token-env-var` (or the equivalent
TOML field) for secrets. Literal `headers` entries are for non-secret values
like `X-Tenant` or `X-Region`; the value is stored in `~/.ignis/config.toml`
in plaintext. There is no string-interpolation (`${VAR}`) in `headers` in this
release — if you need a secret in a non-Authorization header, wait for a
follow-up release or use the stdio transport with a wrapper script.

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
ignis mcp list              # one row per server: name, transport, status, tool count, target
ignis mcp list --json       # machine-readable
ignis mcp list --no-connect # config view only — don't spawn anything

ignis mcp get <name>        # config + connect probe + tool list (and instructions);
                            # HTTP entries print header keys only — values are never shown.
ignis mcp remove <name>     # drops from config.toml + clears the disabled flag

ignis mcp enable  <name>    # clear the runtime disable flag
ignis mcp disable <name>    # keep the config entry, don't connect next run
```

`enable` / `disable` only edit `~/.ignis/state.json` — the config entry stays
put, so toggling is reversible and cheap.

## Runtime control: `/mcp`

Inside the TUI, the **`/mcp`** picker is the equivalent of
`ignis mcp enable/disable` — toggle any configured server on or off. Each row
shows the transport tag (`(stdio)` / `(http)`), connection status, and the
tools the server is exposing (capped, with a `+N more` overflow line
pointing at `ignis mcp get <name>` for the full list). Disabled servers don't
connect at startup, their tools don't appear in the catalog, and their
`instructions` block is omitted. The state persists to `~/.ignis/state.json`.

## Lifecycle + errors

- **Eager startup.** All enabled servers spawn during ignis startup; the
  `initialize` handshake is bounded by `startup_timeout_secs`.
- **A server that fails to start does not block ignis.** Its row in
  `ignis mcp list` shows the failure; other servers and the built-in tools
  keep working.
- **Tool calls** are bounded by `tool_timeout_secs`. A timeout returns an
  error to the model but does not tear the connection down.

## What's intentionally not supported

These were dropped during design so the integration ships small and
predictable:

- **Legacy HTTP+SSE transport** (MCP spec 2024-11-05, the two-endpoint
  `GET /sse` + `POST /messages` shape). Only Streamable HTTP is supported.
  Most active MCP servers either already speak it or are migrating.
- **OAuth / interactive auth flows.** For HTTP, use `--bearer-token-env-var`
  (or `bearer_token_env_var = "…"` in TOML). For stdio, pass credentials via
  `-e KEY=VALUE` or the server's own config file.
- **MCP resources and prompts** — only `tools/*` surfaces are wired.
- **Per-tool permission rules** — the existing
  [permissions](permissions.md) system gates the wrappers as a group; per-tool
  allow/deny is not yet supported.
- **`${VAR}` interpolation in `headers`** — secrets belong in
  `bearer_token_env_var`; literal headers stay literal.
- **Mid-session reconnect** on a hard transport drop. rmcp handles
  session-id expiry transparently (re-runs `initialize`), but a TCP-level
  loss marks the server failed until the next ignis launch — same as stdio.
