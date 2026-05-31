<div align="center">

# 🔥 Ignis

**Your AI coding agent, right in the terminal.**

[![CI](https://github.com/Fullstop000/ignis/actions/workflows/ci.yml/badge.svg)](https://github.com/Fullstop000/ignis/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Fullstop000/ignis?sort=semver)](https://github.com/Fullstop000/ignis/releases)
[![codecov](https://codecov.io/gh/Fullstop000/ignis/graph/badge.svg)](https://codecov.io/gh/Fullstop000/ignis)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

<img src="assets/demo.png" alt="Ignis running in the terminal" width="820">

</div>

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/Fullstop000/ignis/master/install.sh | sh
```

Drops the binary in `~/.ignis/bin`. Already installed? Update in place with
`ignis upgrade`.

<details>
<summary>Other ways to install</summary>

```bash
# Pin a version or install dir
curl -fsSL …/install.sh | IGNIS_VERSION=v0.14.1 IGNIS_INSTALL_DIR=/usr/local/bin sh

# Self-update
ignis upgrade                     # download + replace the running binary
ignis upgrade --check             # report whether an update is available
ignis upgrade --version v0.14.1   # pin to a specific tag

# From source (stable Rust toolchain)
git clone https://github.com/Fullstop000/ignis.git
cd ignis && cargo build --release   # → target/release/ignis
```

Prebuilt binaries for Linux, macOS, and Windows are attached to every
[GitHub Release](https://github.com/Fullstop000/ignis/releases).

</details>

## Quickstart

```bash
# 1. Point ignis at a provider and key
mkdir -p ~/.ignis && cat > ~/.ignis/config.toml <<'TOML'
model = "deepseek/deepseek-v4-flash"

[providers.deepseek]
api_key = "sk-your-deepseek-key"
TOML

# 2. Launch the TUI…
ignis

# …or run one-shot from the shell
ignis "fix the failing test in src/parser.rs"
```

See [Configure](#configure) for more providers and per-model options.

## Features

- **TUI + CLI** — a native terminal TUI (`ratatui` + `crossterm`) and a one-shot
  CLI from the same binary.
- **Bring your own model** — OpenAI, Anthropic, DeepSeek, Kimi, MiniMax,
  Moonshot, Ollama, and any OpenAI-compatible endpoint (the `custom` provider).
  Providers are built in — drop in an API key and go. Switch model and reasoning
  effort at runtime with `/model`.
- **Streaming agent loop** — incremental text and reasoning, parallel or
  sequential tool execution, and lifecycle hooks.
- **Built-in tools** — read, write, and edit files; `grep`, `glob`, `list_dir`;
  `bash`; `web_fetch` and `web_search`; `ask_user`; and `agent` to delegate a
  subtask to a one-level sub-agent.
- **[MCP servers](docs/configure/mcp.md)** — connect external stdio or
  HTTP [MCP](https://modelcontextprotocol.io) servers; their tools appear
  alongside the built-ins.
- **[Skills](docs/configure/skills.md)** — load reusable `SKILL.md` instruction
  sets on demand, sharable across Claude Code, Codex, OpenCode, and Kimi.
- **[Permissions](docs/configure/permissions.md)** — every tool call passes a
  gate, with a built-in safety floor and user-declarable allow/ask/deny rules.
- **Sessions** — project-scoped history with `--resume`, auto-resume, and
  context compaction; export per-session stats with `ignis sessions export`.
- **Single binary** — no external runtime dependencies, with built-in
  self-update.

## Configure

Ignis reads `~/.ignis/config.toml`. Each provider — its endpoint(s), model list,
context windows, and reasoning levels — is **built in**, so you normally just
pick one and supply an `api_key`:

```toml
model = "minimax-token-plan/MiniMax-M2.7"

[providers.minimax-token-plan]
api_key = "sk-cp-your-key"
# protocol = "openai"   # MiniMax serves both protocols; ignis defaults to Anthropic
```

`config.toml` is overrides-only: set `api_url` to point at a different endpoint,
or add a `models` list to extend the catalog or override a model's `reasoning`
(effort levels) / `context` (window, else looked up from
[models.dev](https://models.dev)). For any other OpenAI-compatible endpoint, use
the built-in `custom` provider (supply `api_url` + `models`). `/model` switches
the active selection at runtime, saving it to `~/.ignis/state.json` — your
`config.toml` is never auto-edited. See
[`config.example.toml`](config.example.toml) for every provider and optional
`web_search` / `compaction` settings.

> Your `~/.ignis/config.toml` holds secrets and is never committed. The
> repo-level `config.toml` is git-ignored on purpose — commit
> `config.example.toml` only.

## Usage

| Command | What it does |
| --- | --- |
| `ignis` | Interactive TUI (default) |
| `ignis "<prompt>"` | One-shot to stdout |
| `ignis --resume [id] [prompt]` | Resume the latest (or given) session |
| `ignis --afk` | Fully unattended: auto-approve tools, dismiss `ask_user` |
| `ignis mcp …` | Manage MCP servers (`add`, `list`, `get`, `remove`, `enable`, `disable`) |
| `ignis sessions export --html` | Export an HTML report of session stats |
| `ignis upgrade` | Update to the latest release |
| `ignis --help` | Full flag and subcommand list |

In the TUI: `Enter` sends, `↑/↓` walk history, `Ctrl+D` exits. Output renders
inline in the normal buffer, so scroll with your terminal/tmux as usual. Type
`/` for slash-command suggestions — see
[`docs/usage/commands.md`](docs/usage/commands.md) for the full reference.

## Docs

Deep references live in [`docs/`](docs/README.md) — commands, permissions,
skills, MCP servers, telemetry.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the workflow and
[CLAUDE.md](CLAUDE.md) for coding guidelines.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
