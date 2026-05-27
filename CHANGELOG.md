# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.15.3] - 2026-05-28

### Fixed
- Reasoning streams live as a 💭-prefixed block instead of appearing all at once at turn end.
- API keys (OpenAI `sk-…` / Anthropic `sk-ant-…`) in provider error messages are now actually redacted; the previous code matched a regex literal as plain text and never replaced anything.
- Parallel tool execution no longer silently drops results whose `tool_call_id` doesn't match an announced tool call.

## [0.15.2] - 2026-05-27

### Fixed
- `install.sh` and `ignis upgrade` no longer fail with HTTP 403 on shared-IP networks (WSL/corp NAT/CI) — both now resolve the latest tag via the `/releases/latest` HTML redirect instead of the rate-limited JSON API.

## [0.15.1] - 2026-05-27

### Changed
- Internal — unified the top-level CLI on clap; `ignis --help` is now clap-generated and stays in lockstep with the actual flags.
- Typo'd top-level flags (e.g. `ignis --resme`) now error with a suggested correction instead of being silently sent as one-shot prompt text. A literal `-`-leading prompt now needs `--`, e.g. `ignis -- "--debug fix"`.

## [0.15.0] - 2026-05-27

### Added
- `install.sh` — one-liner installer for Linux/macOS that drops the latest release binary into `~/.ignis/bin` (override with `IGNIS_INSTALL_DIR`).
- `ignis upgrade` (alias `ignis update`) — self-update to the latest release; supports `--check`, `--force`, and `--version v0.X.Y`.
- `ignis --version` / `-V` — print the binary's version and exit.
- `ignis --help` / `-h` — print top-level usage with the available flags and subcommands.

### Removed
- `ignis --tui` / `ignis tui` — redundant; the no-arg invocation already launches the TUI.

## [0.14.1] - 2026-05-27

### Changed
- Internal — split `console/render.rs` and `console/mod.rs` into focused submodules and gave the slash-picker types their own behavior. No user-visible change.

## [0.14.0] - 2026-05-26

### Added
- `ask_user` — model-initiated interactive picker; the model can pause mid-turn and ask the user a structured 2-4 option question (single- or multi-select, with a free-text "Other" row).

## [0.13.0] - 2026-05-26

### Added
- MCP — connect to user-configured stdio Model Context Protocol servers and expose their tools to the model as `mcp__<server>__<tool>`.
- `/mcp` — slash command to manage MCP servers (enable/disable).
- `ignis mcp add | list | get | remove | enable | disable` — CLI to manage MCP servers from outside the TUI.

## [0.12.0] - 2026-05-26

### Added
- Skills — define reusable `SKILL.md` instruction sets the model loads on demand; force-load with `/skill-name` and manage with `/skills`.

## [0.11.0] - 2026-05-25

### Added
- Queued messages — while the agent is busy, `Enter` queues your prompt to send after the current turn.
- Message steering — `Ctrl+S` sends a message into the running turn; `↑` recalls the last queued message to edit.

### Fixed
- Stray space rendered after CJK / wide characters in scrollback.

## [0.10.0] - 2026-05-25

### Added
- `/model` picker shows each model's context window (config override, else models.dev).

### Changed
- **BREAKING:** per-model `reasoning`/`context` move into `models` entries (a bare name or an inline table); the `[providers.X.reasoning]` / `[providers.X.context]` sub-tables are removed.

## [0.9.0] - 2026-05-25

### Changed
- TUI renders inline in the normal buffer, so conversation history lives in terminal scrollback (tmux/native scroll now works).
- While generating, the reply appears in full when complete instead of streaming token-by-token.

### Removed
- In-app scroll keybindings and mouse capture — the terminal handles scrolling and text selection now.

## [0.8.0] - 2026-05-25

### Removed
- **BREAKING:** the YAML plugin/extension system.

## [0.7.0] - 2026-05-25

### Changed
- `edit_file` diff view: solid red/green line backgrounds and syntax-highlighted code.

## [0.6.0] - 2026-05-25

### Added
- `/model` — switch provider/model and reasoning effort at runtime, remembered across restarts.

### Changed
- **BREAKING:** config — active selection is now top-level `model = "provider/model"`; each provider declares a `models` catalog (replaces `active_provider` + per-provider `model`).

## [0.5.0] - 2026-05-25

### Added
- `/copy` — copy the last assistant reply to the system clipboard.

## [0.4.1] - 2026-05-24

### Fixed
- Streaming loader shows input (`↑`) and output (`↓`) tokens, not just the output estimate.

## [0.4.0] - 2026-05-24

### Added
- Real token usage: OpenAI-compatible providers (kimi, DeepSeek) report actual `usage` (incl. DeepSeek's `prompt_cache_hit_tokens`); captured via `LlmResponseDelta::Usage` / `AgentEvent::Usage`.
- Token usage is persisted per session (`<id>.usage.json`) and reloaded on open.
- Footer shows real context tokens (e.g. `1.6k tok (1%)`), falling back to a chars/4 estimate when a provider doesn't report usage.

### Changed
- Loader is livelier: rotating whimsical verbs (Thinking → Pondering → Nebulizing → …) plus live output tokens & tok/s while streaming.

## [0.3.0] - 2026-05-24

### Added
- `grep` tool — regex content search across the project, gitignore-aware (ripgrep's `ignore` + `regex`).
- `glob` tool — find files by glob pattern (`**/*.rs`), gitignore-aware.
- `web_fetch` tool — fetch a URL and return its readable text (HTML stripped); pairs with `web_search`.
- `agent` tool — delegate a self-contained task to a one-level sub-agent that has the base toolset and returns its final answer.

### Changed
- Tool-call headers show argument values only, never parameter names (e.g. `grep("fn main")`).
- `edit_file` returns a git-style diff; the console renders removed lines red and added lines green.

## [0.2.1] - 2026-05-24

### Changed
- Tool headers show path args bare and relative to the working dir (e.g. `read_file(src/main.rs)` instead of `read_file(path="…/src/main.rs")`).
- Internal layout: `tool.rs` → `tools/tool.rs`, `storage.rs` → `session/storage.rs` (crate-root paths preserved via re-exports).
- CI/release actions bumped off deprecated Node 20 (`checkout` v5, `action-gh-release` v3).

### Removed
- `scratch/`, `docs/`, and the bundled sample `.ignis/extensions` plugin.

## [0.2.0] - 2026-05-24

### Added
- `Session` core model wrapping a stateless `Agent`, exposing `prompt()` and `compact()`.
- Context compaction: token-budget range, auto-trigger threshold, 9-section summary prompt; `/compact` command and `[compaction]` config.

### Changed
- Renamed `repl` → `console`; `agent`/`session`/`cli`/`console` are now directory modules.
- TUI: frame-capped coalesced rendering, borderless Claude Code-style layout, status footer (dir · model · ctx%), loading status above input, mouse-wheel scroll, Ctrl/Cmd+J newline.
- Replaced the `You`/`Ignis` turn labels with a 👤 user-prompt prefix; replies render as plain markdown.
- Merged `/new` into `/clear` (single session-reset command).

### Fixed
- Multi-byte (CJK) input no longer panics; cursor stays on UTF-8 char boundaries and uses display-width columns.
- Tool output and markdown no longer garble the screen — tabs expand to spaces and control chars are stripped before rendering.
- `truncate()` is char-safe (was byte-slicing and could panic on multi-byte previews).
- Chat no longer hides its last lines behind the input box — scroll bounds count wrapped rows, not logical lines.
- Resumed sessions render tool calls as proper blocks instead of raw `{"result":…}` JSON; the resume picker shows a clean screen without the prior conversation.

## [0.1.0] - 2026-05-24

### Added
- Switchable `web_search` backend with Brave and Tavily providers, selected via
  `[web_search]` in `config.toml` (`provider` + `api_key`).
- `docs/web-search.md` tutorial for configuring and using web search.
- Project documentation: `README.md`, `CONTRIBUTING.md`, this changelog, and an
  Apache-2.0 `LICENSE`.
- CI workflow (`fmt` + `clippy -D warnings` + tests on Linux/macOS) and a
  cross-platform release workflow triggered by `v*` tags.
- `config.example.toml` template.
- `/ship` skill (`.claude/skills/ship/`) — the release runbook for this repo.

### Changed
- Configuration format migrated from YAML to TOML; config is loaded from
  `~/.ignis/config.toml`.

### Removed
- Dead DuckDuckGo HTML-scraping implementation of `web_search` (it had stopped
  returning results due to anti-bot challenges).

### Security
- Removed a real API key that was present in a local (unpushed) commit and
  git-ignored local config files so secrets can no longer be committed.
