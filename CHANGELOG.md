# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- hooks: v2 protocol extends the surface from 2 events to 9 — `SessionStart`, `SystemPromptCompose`, `PreToolUse`, `PostToolUse`, `PreCompact`, `PostCompact`, and `Stop` join `UserPromptSubmit` + `AssistantMessageRender`. Each event gets typed per-event envelope fields (`tool_name`, `tool_input`, `tool_response`, `system_prompt`, `model`, `trigger`, `summary`, `source`, `transcript_path`, `triggered_at`). Hooks can rewrite (`updatedInput` / `updatedOutput` / `updatedSystemPrompt`), block (`decision: "block"` + `reason`), or inject context for the next LLM call (`additionalContext`, surfaced as a labelled `<system-reminder>`). v1 hook configs parse unchanged.
- hooks: `matcher` field on `PreToolUse` / `PostToolUse` entries — regex on `tool_name`, compiled at parse so a malformed pattern is a startup error. Non-matching tools skip the spawn entirely. Declaring `matcher` on a non-tool event logs a `[warn]` at load.
- hooks: `Stop` event honours the Claude Code inversion — a `decision: "block"` on `Stop` keeps the loop alive and surfaces the reason as a `<system-reminder>` framed `"stopped continuation: <reason>"`. Lets users wire "don't stop until tests pass" guardrails without a new event.
- hooks: `SystemPromptCompose` fires once per LLM call (not per session) — hooks can prune or rewrite the assembled system prompt (which includes git status/diff, AGENTS.md, the skills catalog, and MCP instructions) to A/B test prompt density for token-efficiency research.
- hooks: three new example hooks — `examples/hooks/bash-deny-rm-rf/` (PreToolUse block), `examples/hooks/auto-test/` (PostToolUse `additionalContext`), `examples/hooks/system-prompt-trim/` (SystemPromptCompose rewrite).

## [0.35.0] - 2026-06-06

### Added
- TUI — `/hooks` (and the alias `/hooks list`) now prints the in-memory hook chains — one block per event, each entry showing the program path, its argv tail, and the per-hook timeout — and still re-reads `~/.ignis/hooks.json` from disk under `/hooks reload`. An empty registry prints a single `[info] no hooks registered` line pointing at the file and the reload action. ([#127](https://github.com/Fullstop000/ignis/pull/127))

### Changed
- TUI renders inline in the terminal's normal buffer — the conversation lives in native scrollback, so terminal copy/scroll and tmux detach-reattach work, and assistant text streams in as it settles. ([#131](https://github.com/Fullstop000/ignis/pull/131))

### Fixed
- Interrupting a turn with `Ctrl+C` no longer breaks the next message — an interrupted tool call is closed out so the conversation continues instead of being rejected by the provider. ([#129](https://github.com/Fullstop000/ignis/pull/129))
- TUI — a long `ask_user` question (including CJK and other space-less text) no longer overruns its row and hides the newest transcript lines behind the status bar. ([#130](https://github.com/Fullstop000/ignis/pull/130))

## [0.34.0] - 2026-06-05

### Added
- TUI — scroll the transcript with the mouse wheel. ([#125](https://github.com/Fullstop000/ignis/pull/125))
- TUI — pasting a multi-line block (≥ 4 lines) now shows a compact `[ pasted-text#N M lines ]` chip in the composer instead of dumping the whole text inline; the chip expands back to the full content when you send. ([#124](https://github.com/Fullstop000/ignis/pull/124))
- Benchmarks — bench pipeline records per-trial tool-call success rate (OK / total / err) sourced from ignis's own session JSONL, surfaced as a new "Tool OK rate" headline card, sortable "Tool OK%" column, and per-trial drill-down line. Harness-portable: no dependency on harbor stdout markers. ([#120](https://github.com/Fullstop000/ignis/pull/120))
- `ask_user` shows a review-and-submit screen for multi-question batches — confirm every answer, or step back (Left / Shift-Tab) to revise one before submitting. ([#115](https://github.com/Fullstop000/ignis/pull/115))
- Footer shows the current git branch (oh-my-zsh `git:(branch)` style) when the working directory is a git repo. ([#115](https://github.com/Fullstop000/ignis/pull/115))

### Changed
- Agent — outbound history trim before every model call. Prior-turn reasoning is dropped on assistant messages that did NOT call a tool (mirrors DeepSeek's documented non-tool-turn contract and strips inline `<think>...</think>` for MiniMax-M3); tool-calling turns are preserved verbatim. Cuts input-token bloat from replayed chain-of-thought without changing tool-call linkage or per-provider invariants. Toggle with `[settings] strip-think = true|false` in `~/.ignis/config.toml`; `IGNIS_HISTORY_TRIM=off` disables it at runtime. ([#123](https://github.com/Fullstop000/ignis/pull/123))
- TUI — the live "Thinking…" timer now reads `5s` / `1m 05s` / `1h 02m 05s` instead of raw seconds. ([#125](https://github.com/Fullstop000/ignis/pull/125))
- TUI — removed the `↑/↓ N more lines` transcript scroll hints. ([#125](https://github.com/Fullstop000/ignis/pull/125))
- Tool-initiated pickers (`ask_user`, `permission`, `connect`, `afk`) now anchor to the bottom of the body above the input band — transcript stays visible above — matching the Claude Code / Codex convention. ([#107](https://github.com/Fullstop000/ignis/pull/107))
- Internal — the TUI slash-command dispatcher (`submit_text`) is now a `match` on the command token instead of a 13-arm `if/else` ladder, with the no-provider gate hoisted to a single early return and the skill-command lookup deduplicated. No user-visible change. ([#111](https://github.com/Fullstop000/ignis/pull/111))
- Internal — native tools now share one static metadata and call adapter; the agent loop's stream-retry and hook-block paths are factored into named helpers. No user-visible change. ([#119](https://github.com/Fullstop000/ignis/pull/119))
- Internal — renamed agent-loop events so a `Turn` is the whole user exchange and a `Run` is one LLM round (`AgentStart`/`AgentEnd` → `TurnStart`/`TurnEnd`; old `TurnStart`/`TurnEnd` → `RunStart`/`RunEnd`). No user-visible change. ([#121](https://github.com/Fullstop000/ignis/pull/121))
- Internal — `Agent::run` is now a stable control-flow skeleton over named lifecycle moments (`before_llm_call`, `call_llm`, `after_llm_call`, `emit_fatal`); telemetry, hooks, and message assembly moved out of the loop body. No user-visible change. ([#122](https://github.com/Fullstop000/ignis/pull/122))
- Internal — dropped the experimental `IGNIS_HISTORY_TRIM` modes (`mask-only`, `strip-wide`, `both`) and the unused tool-result mask path. The only knob is now `strip-think` (TOML) / `IGNIS_HISTORY_TRIM=off|<anything>` (env). No user-visible change for the shipped default. ([#126](https://github.com/Fullstop000/ignis/pull/126))

### Fixed
- TUI message queue now routes queued slash commands through the same dispatcher Enter uses, so `/compact`, `/model`, and other commands typed while the agent is busy actually run on drain instead of being sent to the LLM as literal user messages. Slash-command autocomplete is also surfaced while busy so the queued line can be completed from the dropdown. ([#106](https://github.com/Fullstop000/ignis/pull/106))
- TUI: `End` now actually follows the transcript to the bottom — plain `End` (with empty input) scrolls and the renderer reserves room for the top hint so the last line is no longer clipped and no stale `↓ N more lines below` hint lingers on a followed view. ([#108](https://github.com/Fullstop000/ignis/pull/108))
- Ctrl+C between Enter and the model's first reply no longer wipes the just-typed user prompt from disk; the next prompt's session re-loads it instead of starting a blank conversation. ([#108](https://github.com/Fullstop000/ignis/pull/108))

## [0.33.1] - 2026-06-03

### Fixed
- `MiniMax-M3` now declares its 1M-token context window so `/model` and the context bar show the correct size instead of falling back to the default. ([#103](https://github.com/Fullstop000/ignis/pull/103))
- Tool-block headers and permission prompts on Anthropic-compatible streams (Anthropic, MiniMax `/anthropic`) showed the tool name duplicated — `bashbashbash("…")` — because the SSE parser re-sent `name` on every `input_json_delta` chunk; now emitted only on `content_block_start`. ([#104](https://github.com/Fullstop000/ignis/pull/104))

## [0.33.0] - 2026-06-02

### Added
- Auto-retry on streaming model-response drops; shows a `↻ Reconnecting (n/3)` notice instead of failing the turn (exponential backoff 500 ms → 1 s → 2 s, capped at 10 s). ([#97](https://github.com/Fullstop000/ignis/pull/97))

## [0.32.0] - 2026-06-01

### Added
- hooks: external subprocess hooks for `UserPromptSubmit` and `AssistantMessageRender` with first-class prompt mutation; declare in `~/.ignis/hooks.json`. ([#102](https://github.com/Fullstop000/ignis/pull/102))
- MiniMax Token Plan: `MiniMax-M3` model now in the catalog (Anthropic-first, same dual-endpoint shape as M2.7). ([#94](https://github.com/Fullstop000/ignis/pull/94))
- MCP HTTP transport (Streamable HTTP). `[mcp.servers.X] url = "https://…"` or `ignis mcp add X --url … [--bearer-token-env-var ENV] [--header "K: V"]`; `/mcp` picker shows transport tag and per-server tool list. ([#86](https://github.com/Fullstop000/ignis/pull/86))

### Changed
- `/sessions` opens an interactive picker with a per-turn timing waterfall (token usage, tool rollup, start-offset bars, per-turn user-prompt preview); `/resume` removed — use `/sessions`. ([#84](https://github.com/Fullstop000/ignis/pull/84))
- TUI runs fullscreen in the alternate-screen buffer; the input band is permanently pinned at the bottom and transcript history lives in an in-app scrollable buffer (`PgUp`/`PgDn`, `Ctrl+Home`/`Ctrl+End`, auto-follow when at the bottom). ([#89](https://github.com/Fullstop000/ignis/pull/89))
- Reasoning content now renders as its own dim `✻ Thinking` block instead of a `💭`-prefixed reply, and keeps streaming even when reasoning arrives after text starts. ([#83](https://github.com/Fullstop000/ignis/pull/83))

### Fixed
- `ignis -v` / `--version` now print the version instead of erroring; `-V` is no longer accepted. ([#95](https://github.com/Fullstop000/ignis/pull/95))
- `/sessions`, `/model`, and `ask_user` pickers now window the option list around the selection so long lists don't scroll the highlight off-screen; `↑N more` / `↓N more` markers appear at the window edges. ([#89](https://github.com/Fullstop000/ignis/pull/89))
- `/telemetry` and `/afk` pickers now print a confirmation notice after selection. ([#89](https://github.com/Fullstop000/ignis/pull/89))
- `ask_user` picker: full-width divider + placeholder gating. ([#82](https://github.com/Fullstop000/ignis/pull/82))

### Breaking
- `ignis upgrade --version <TAG>` renamed to `ignis upgrade --tag <TAG>` so it no longer collides with the root `--version`. ([#95](https://github.com/Fullstop000/ignis/pull/95))

## [0.31.0] - 2026-05-31

### Added
- Auto-update check — TUI footer shows `● new version available — run \`ignis upgrade\`` in yellow when a newer GitHub release exists; cached 24h, opt out with `IGNIS_NO_UPDATE_NOTIFIER=1`. ([#81](https://github.com/Fullstop000/ignis/pull/81))

### Changed
- README reorganized into Usage / Configure sections. ([#78](https://github.com/Fullstop000/ignis/pull/78))

### Fixed
- TUI `edit_file`/`create_file` headers now show only the `file_path` instead of dumping `old_string`/`new_string`/`content` into the header. ([#76](https://github.com/Fullstop000/ignis/issues/76))
- `ask_user` picker now sits behind a horizontal divider that spans the full terminal width, and the `Other` row's placeholder disappears once you start typing instead of rendering next to the user's text. ([#66](https://github.com/Fullstop000/ignis/issues/66))

### Security
- `~/.ignis/config.toml` is now chmod-ed to `0600` on Unix (write + one-time load-time migration) so other local UIDs can't read the API keys it carries. ([#74](https://github.com/Fullstop000/ignis/issues/74))

## [0.30.0] - 2026-05-31

### Added
- `/connect` — interactive 3-step setup to pick a provider, paste an API key, and pick a model; first launch with no `config.toml` now opens straight into it. ([#75](https://github.com/Fullstop000/ignis/pull/75))
- `/telemetry` — TUI picker to toggle OpenTelemetry export on/off. ([#65](https://github.com/Fullstop000/ignis/pull/65))

## [0.29.0] - 2026-05-30

### Added
- MiniMax Token Plan provider — same models over OpenAI- and Anthropic-compatible endpoints, Anthropic auto-selected.
- `custom` provider for any OpenAI-compatible endpoint.

### Changed
- Providers are now built in — pick one and supply `api_key`; `config.toml` is overrides-only.

### Fixed
- Anthropic-compatible requests now send the required `max_tokens` field — without it MiniMax (default Anthropic endpoint) and real Anthropic both 400 before streaming. ([#61](https://github.com/Fullstop000/ignis/pull/61))
- `AGENTS.md` symlink now points at the project `CLAUDE.md` instead of an absolute path on the author's machine. ([#61](https://github.com/Fullstop000/ignis/pull/61))

### Breaking
- Provider id `Moonshot Platform CN` is now `moonshot-platform-cn`. ([#61](https://github.com/Fullstop000/ignis/pull/61))
- Removed the `gemini` provider — configs with `[providers.gemini]` will fail to load. ([#61](https://github.com/Fullstop000/ignis/pull/61))

## [0.28.0] - 2026-05-30

- `AGENTS.md` — ignis reads project (`./AGENTS.md`) and global (`~/.ignis/AGENTS.md`) instructions; the project file overrides the global one.

## [0.27.0] - 2026-05-30

### Added
- `/sessions` — marks the current session with a `▸` glyph in the table and surfaces it above the table with a `You are here · sess-… · started … · X msgs / Y turns` line that's always visible (even when the row falls off the visible top-5).

## [0.26.1] - 2026-05-29

### Fixed
- Streaming transport errors (e.g. connection reset) now end the turn instead of looping — a broken stream could previously spin until the agent timeout, emitting the same error millions of times.
- `bash` tool no longer panics when truncating binary or multibyte output at its size cap (it backs off to a UTF-8 char boundary).
- TB2 adapter passes the prompt after `--`, so task instructions that begin with `-` no longer abort the run as an unknown flag.

### Changed
- Agent system prompt nudges verifying the task's exact required output path/format and cleaning up build artifacts before finishing.

## [0.26.0] - 2026-05-29

### Added
- `/sessions` — slash command that shows a compact session-stats block inline in the TUI for the current project.

### Fixed
- `ignis sessions export --html` — session timestamps now render correctly (the v0.21.0 report read milliseconds as seconds and produced year-58371 dates).

### Changed
- Internal — fixed a stale `ignis upgrade` doc comment (it described the JSON API the code no longer uses) and de-duplicated the request User-Agent string. No user-visible change.

## [0.25.3] - 2026-05-29

### Changed
- Internal — the ~600-line `Agent::run` is decomposed into focused helpers (`consume_turn_stream`, `execute_tool_calls`, `execute_single_tool`, `push_with_hook`); the loop body and nested closures are gone. No user-visible change.

## [0.25.2] - 2026-05-29

### Fixed
- `ignis upgrade` on Linux — downloads the musl release asset instead of a nonexistent gnu one (was failing with a 404).
- `install.sh` and `ignis upgrade` — retry the download when GitHub's release CDN resets the connection, instead of failing on the first transient error.

## [0.25.1] - 2026-05-29

### Changed
- Internal — OpenAI-compatible providers (OpenAI, DeepSeek, Kimi, Moonshot) now share one `chat_stream` implementation and SSE parser instead of copy-pasted logic. No user-visible change.

## [0.25.0] - 2026-05-29

### Added
- Markdown tables now render as aligned box-drawing grids in the TUI (display-width columns, `:--`/`--:`/`:-:` alignment) instead of raw pipe-delimited text.

## [0.24.0] - 2026-05-29

### Added
- TB2 adapter — `-m provider/model@<effort>` suffix forces a reasoning level for the in-sandbox run.

## [0.23.1] - 2026-05-29

### Fixed
- TB2 HTML report — caps each embedded agent log to a 512 KiB tail so a runaway run can't bloat the report to gigabytes.

## [0.23.0] - 2026-05-29

### Added
- `[permissions]` config rules — pre-declare `allow`/`ask`/`deny` lists of `Tool(pattern)` rules (e.g. `bash(git *)`, `edit_file(src/**)`, `read_file(.env)`) so common tool calls stop prompting; evaluated deny > ask > allow, beneath the safety floor.
- Permission picker "Always allow" — saves an arity-trimmed rule (e.g. `bash(git status *)`) to `state.json` so matching calls run silently in future sessions.

## [0.22.0] - 2026-05-29

### Added
- `benchmarks/terminal-bench/scripts/` — stdlib-only TB2 result aggregator and single-file HTML report generator.

## [0.21.0] - 2026-05-29

### Added
- `ignis sessions export --html` — self-contained sortable HTML report of per-session stats.

## [0.20.0] - 2026-05-28

### Added
- TUI footer mode badge — peach `HANDS-FREE` or red `AFK` shown left of the model name whenever you're in an auto-approve mode, so it's obvious at a glance that tool calls aren't being prompted.

## [0.19.0] - 2026-05-28

### Added
- Agent permission control — every tool call now passes through a permission gate; sensitive tools (`bash`, `edit_file`, `create_file`, `web_fetch`, `agent`, MCP) prompt with a 3-option picker (Approve once / Approve session / Deny). Built-in safety floor (`rm -rf /` family, edits to `.git/**` / `.ignis/**` / shell init) always prompts and is never bypassed.
- `/afk` slash command — opens a picker to enter *Hands-free* (auto-approve tools, still answer `ask_user`) or *Fully unattended* (auto-approve everything, dismiss `ask_user`, hard-deny safety floor). Disabling fires immediately; enabling confirms. Mode persists in `state.json`.
- `--afk` CLI flag and implicit one-shot AFK — both pin Fully unattended so headless runs never hang on a missing TTY.
- `docs/permissions.md` — user-facing doc covering modes, picker, safety floor, and the roadmap.

## [0.18.0] - 2026-05-28

### Added
- TB2 adapter — `kimi-code/kimi-for-coding` is now a supported `-m` provider.
- TB2 adapter — per-trial `n_input_tokens` / `n_output_tokens` / `n_cache_tokens` now populate harbor's result.json instead of staying null.

## [0.17.0] - 2026-05-28

### Added
- Opt-in OpenTelemetry export of session traces and token-usage metrics over OTLP — enable with `IGNIS_ENABLE_TELEMETRY=1`.
- `/telemetry` — show OpenTelemetry export status.

## [0.16.0] - 2026-05-28

### Added
- `benchmarks/terminal-bench/` — Harbor adapter that runs ignis as a Terminal-Bench 2 agent.

### Changed
- Linux release binary is now statically linked against musl — runs on any modern x86_64 Linux without distro-glibc coupling.

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
