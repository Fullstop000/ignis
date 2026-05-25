# 🔥 Ignis

[![CI](https://github.com/Fullstop000/ignis/actions/workflows/ci.yml/badge.svg)](https://github.com/Fullstop000/ignis/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/Fullstop000/ignis/graph/badge.svg)](https://codecov.io/gh/Fullstop000/ignis)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A single-binary, multi-provider AI coding agent for your terminal — an interactive
TUI and a one-shot CLI, with built-in tools.

## Features

- **Two modes** — a native terminal TUI (`ratatui` + `crossterm`) and a one-shot CLI.
- **Multiple LLM providers** — OpenAI, DeepSeek, Kimi, Anthropic, Gemini, Ollama
  (anything OpenAI-compatible), selected in config.
- **Streaming agent loop** — incremental text + reasoning, with parallel or
  sequential tool execution and lifecycle hooks.
- **Built-in tools** — `read_file`, `create_file`, `edit_file`, `list_dir`,
  `grep`, `glob`, `bash`, `web_fetch`, `web_search` (switchable Brave / Tavily
  backends), and `agent` (delegate a subtask to a one-level sub-agent).
- **Sessions** — project-scoped JSONL history, `--resume`, auto-resume, and
  slash commands (`/resume`, `/clear`, `/compact`, `/copy`, `/model`).
- **Runtime model switching** — `/model` picks the provider/model and (for
  reasoning models) the effort level, and saves it back to your config.
- **Single binary** — no external runtime dependencies.

> `/copy` (copy the last reply to the clipboard) uses a platform clipboard tool:
> `pbcopy` (macOS), `clip` (Windows), `clip.exe` (WSL) — all built in. On a Linux
> desktop, install `wl-clipboard` (`wl-copy`) or `xclip`.

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

Minimal `config.toml` — the top-level `model` is the active selection
(`provider/model`, an optional default); each provider lists the models it offers:

```toml
model = "kimi-code/kimi-for-coding"

[providers."kimi-code"]
api_key = "sk-your-kimi-key"
api_url = "https://api.kimi.com/coding/v1"
models  = ["kimi-for-coding"]
```

`/model` switches the active selection at runtime. It writes the choice to
`~/.ignis/state.json` (which takes priority over the config default) — your
`config.toml` is never auto-edited. Reasoning-effort levels differ by model, so
declare them per model — the picker shows the effort control only for models
that have levels:

```toml
model = "deepseek/deepseek-v4-flash"

[providers.deepseek]
api_key = "sk-your-deepseek-key"
models  = ["deepseek-v4-flash", "deepseek-v4-pro"]

[providers.deepseek.reasoning]
deepseek-v4-pro = ["high", "max"]
```

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

In the TUI: `Enter` sends, `↑/↓` history, `Ctrl+D` exit. Output renders inline in
the normal buffer, so scroll with your terminal/tmux as usual. Type `/` for
slash-command suggestions.

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
