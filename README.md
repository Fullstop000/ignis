# 🔥 Ignis

[![CI](https://github.com/Fullstop000/ignis/actions/workflows/ci.yml/badge.svg)](https://github.com/Fullstop000/ignis/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A single-binary, multi-provider AI coding agent for your terminal — an interactive
TUI and a one-shot CLI, with built-in tools and a simple plugin system.

## Features

- **Two modes** — a native terminal TUI (`ratatui` + `crossterm`) and a one-shot CLI.
- **Multiple LLM providers** — OpenAI, DeepSeek, Kimi, Anthropic, Gemini, Ollama
  (anything OpenAI-compatible), selected in config.
- **Streaming agent loop** — incremental text + reasoning, with parallel or
  sequential tool execution and lifecycle hooks.
- **Built-in tools** — `read_file`, `create_file`, `edit_file`, `list_dir`,
  `bash`, and `web_search` (switchable Brave / Tavily backends).
- **Plugins** — add external tools with a small YAML manifest; no recompile.
- **Sessions** — project-scoped JSONL history, `--resume`, auto-resume, and
  slash commands (`/resume`, `/clear`, `/compact`).
- **Single binary** — no external runtime dependencies.

## Install

Requires a stable Rust toolchain.

```bash
git clone https://github.com/Fullstop000/ignis.git
cd ignis
cargo build --release
# binary at target/release/ignis
```

Prebuilt binaries for Linux, macOS, and Windows are attached to each
[GitHub Release](https://github.com/Fullstop000/ignis/releases).

## Configure

Ignis reads its configuration from `~/.ignis/config.toml`. Start from the example:

```bash
mkdir -p ~/.ignis
cp config.example.toml ~/.ignis/config.toml
# then edit ~/.ignis/config.toml and add your API key(s)
```

Minimal `config.toml`:

```toml
active_provider = "kimi-code"

[providers."kimi-code"]
api_key = "sk-your-kimi-key"
api_url = "https://api.kimi.com/coding/v1"
model   = "kimi-for-coding"
```

For web search, see **[docs/web-search.md](docs/web-search.md)**.

> Your `~/.ignis/config.toml` holds secrets. The repo-level `config.toml` /
> `config.yaml` are git-ignored on purpose — commit `config.example.toml` only.

## Usage

```bash
cargo run                 # interactive TUI (default)
cargo run -- --tui        # interactive TUI (explicit)
cargo run -- "fix the failing test in foo.rs"   # one-shot CLI
cargo run -- --resume     # resume the latest session (TUI)
cargo run -- --resume <id> "follow-up prompt"   # resume a session, one-shot
```

In the TUI: `Enter` sends, `↑/↓` history, `Shift+↑/↓` scroll, `Ctrl+D` exit.
Type `/` for slash-command suggestions.

## Tools & plugins

Native tools are registered automatically. To add an external tool, drop a YAML
manifest into `~/.ignis/extensions/` or `./.ignis/extensions/`:

```yaml
name: "hello_plugin"
description: "Greets the user or a target."
parameters:
  type: object
  properties:
    name: { type: string, description: "Who to greet" }
  required: []
command: "python3 hello.py"     # receives the tool args as JSON on stdin
execution_mode: "parallel"       # or "sequential"
```

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution workflow and
[CLAUDE.md](CLAUDE.md) for the coding guidelines.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
