# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
