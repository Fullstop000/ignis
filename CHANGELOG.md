# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.41.3] - 2026-06-21

### Added
- TUI — a "Compacting context…" spinner now shows while the agent summarizes old history, and disappears when compaction finishes. ([#218](https://github.com/Fullstop000/ignis/pull/218))
- TUI — after each compaction, a report block shows the token reduction and the full summary text. ([#218](https://github.com/Fullstop000/ignis/pull/218))

### Changed
- TUI — the Ink UI now exits on a double Ctrl-D instead of Ctrl+C, matching the native TUI. ([#219](https://github.com/Fullstop000/ignis/pull/219))

### Fixed
- TUI — `/compact` now cancels on Ctrl+C instead of ignoring the interrupt. ([#218](https://github.com/Fullstop000/ignis/pull/218))
- TUI — compaction now clears the old conversation from the screen, leaving only the summary report. ([#218](https://github.com/Fullstop000/ignis/pull/218))

## [0.41.2] - 2026-06-21

### Added
- TUI — the Ink `edit_file` diff view now syntax-highlights the code over the green/red `+`/`-` background tints. ([#213](https://github.com/Fullstop000/ignis/pull/213))

### Fixed
- TUI — multi-line pastes in the Ink composer now collapse to a `[paste #N]` chip instead of submitting line-by-line or fragmenting. ([#214](https://github.com/Fullstop000/ignis/pull/214))
- TUI — the Ink task-list panel now paints immediately on resume, not only after the next prompt. ([#212](https://github.com/Fullstop000/ignis/pull/212))
- Ink frontend — engine spawn and crash failures now surface a blocking error overlay, and clean exits send an explicit shutdown frame before closing stdin. ([#198](https://github.com/Fullstop000/ignis/pull/198))

## [0.41.1] - 2026-06-19

### Added
- TUI — the Ink task-list panel (`todo_write`) caps at 8 visible rows; on long lists the window anchors on the `in_progress` row so the active task stays visible. ([#211](https://github.com/Fullstop000/ignis/pull/211))

### Fixed
- TUI — the Ink frontend no longer flickers when the rolling output exceeds the terminal height. ([#210](https://github.com/Fullstop000/ignis/pull/210))
- Ink frontend — `ignis --resume <id>` now restores the prior conversation instead of starting an empty session. ([#208](https://github.com/Fullstop000/ignis/pull/208))
- Sessions — the last turn before exit is no longer dropped, so resumed sessions keep their most recent messages. ([#208](https://github.com/Fullstop000/ignis/pull/208))

## [0.41.0] - 2026-06-19

### Added
- TUI — the Ink `edit_file` diff view is now a reusable `<DiffView>` component; it highlights changed words and fills `+`/`-` rows with full-width green/red background bars. ([#201](https://github.com/Fullstop000/ignis/pull/201))
- Tools — `edit_file` now returns a real unified diff (`@@ -a,b +c,d @@` hunks with context) and the Ink frontend renders it as a line-numbered `◆ Edited <path> (+a -d)` view with `⋮` separators between non-contiguous hunks. ([#200](https://github.com/Fullstop000/ignis/pull/200))
- Tools — `todo_write` lets the model keep a live task checklist; the Ink frontend renders it as a ✓/◐/◻ panel that persists across `/resume`. ([#202](https://github.com/Fullstop000/ignis/pull/202))
- Tools — `bash` can run a command in the background (`run_in_background`); read its streaming output with `bash_output` and stop it with `kill_shell`, and the Ink footer shows `⚙ N bg` while any are live. ([#203](https://github.com/Fullstop000/ignis/pull/203))
- Tools — the `agent` tool accepts an `agent_type` (`general`/`explore`/`review`); `explore` and `review` run with a read-only toolset (no edits, shell, or network). ([#204](https://github.com/Fullstop000/ignis/pull/204))
- TUI — the model can end a reply with 2–4 suggested follow-up prompts (a `<follow_ups>` block, stripped from the transcript); the Ink frontend renders them as a Tab-pickable strip. ([#205](https://github.com/Fullstop000/ignis/pull/205))
- Hooks — `PreToolUse` and `PostToolUse` events let a configured hook block or rewrite a tool call before it runs and rewrite its result after, with an optional tool-name `matcher`. ([#206](https://github.com/Fullstop000/ignis/pull/206))

### Changed
- TUI — slash-command suggestions now render below the input bar (was above), capped at 8 visible rows; both Ink and ratatui frontends are now flush with no blank rows above or below the input. ([#197](https://github.com/Fullstop000/ignis/pull/197))

### Fixed
- TUI — the Ink frontend now pins picker boxes to the terminal width so resizing the window no longer draws stale or overflowing rounded borders. ([#199](https://github.com/Fullstop000/ignis/pull/199))

### Security
- Tools — in the unattended permission modes, auto-run `bash` commands are confined by a Landlock/Seatbelt write sandbox to the working directory and temp dirs (plus any configured `[permissions] sandbox_write_paths`); reads stay broad. ([#207](https://github.com/Fullstop000/ignis/pull/207))

## [0.40.2] - 2026-06-18

### Added
- Providers — Ark Coding Plan now includes `glm-5.2` with a 1M context window. ([#194](https://github.com/Fullstop000/ignis/pull/194))

### Fixed
- TUI — the Ink frontend now renders `edit_file` results as red/green diffs instead of generic tool output. ([#194](https://github.com/Fullstop000/ignis/pull/194))
- TUI — the Ink frontend no longer flickers on transcripts taller than the terminal window. ([#195](https://github.com/Fullstop000/ignis/pull/195))

## [0.40.1] - 2026-06-18

### Fixed
- Ink frontend — messages typed while the agent is busy now wait in a queue and send one per turn instead of submitting immediately, matching the built-in TUI. ([#193](https://github.com/Fullstop000/ignis/pull/193))

## [0.40.0] - 2026-06-17

### Changed
- TUI — releases now bundle the Ink frontend and `ignis` uses it by default when Node >=18 is available, falling back to the built-in `ratatui` TUI otherwise (`IGNIS_FRONTEND=native` forces it). ([#192](https://github.com/Fullstop000/ignis/pull/192))

## [0.39.0] - 2026-06-16

### Added
- TUI — an experimental opt-in Ink frontend (set `IGNIS_FRONTEND=ink`, requires Node) renders at parity with the native terminal UI and falls back to it when Node is unavailable. ([#174](https://github.com/Fullstop000/ignis/pull/174))
- Tools — `edit_file` accepts an optional `global_replace=true` parameter to replace every occurrence of `old_text` instead of only the first. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- Tools — `web_fetch` and `web_search` now retry transient failures (timeouts, connection drops, and 5xx responses) with exponential backoff. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- Hooks — hook executables can now be bare command names resolved on `$PATH`, and invalid env-var names are rejected at config load time. ([#187](https://github.com/Fullstop000/ignis/pull/187))

### Changed
- Telemetry — OpenTelemetry export is now **off** by default; opt in with `[telemetry] enabled = true` or `IGNIS_ENABLE_TELEMETRY=1`. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- Sessions — project session directories now use a cleaner slug (no leading dash from the root separator) and legacy directories are migrated automatically while keeping each `.usage.json` sidecar with its session file. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- LLM — removed the speculative `IGNIS_HISTORY_TRIM` runtime A/B env knob. Outbound history trimming is now controlled only by `[settings] strip-think` in `~/.ignis/config.toml`; the default remains `true`. ([#189](https://github.com/Fullstop000/ignis/pull/189))
- Tools — tool output now uses one consistent `... [truncated]` marker everywhere, and `read_file`/`list_dir` return explicit `(empty file)`/`(empty directory)` instead of a blank result. ([#191](https://github.com/Fullstop000/ignis/pull/191))

### Fixed
- TUI — resizing or splitting a terminal pane no longer leaves stale copies of the input bar in native scrollback; Ignis coalesces each resize burst into one settled purge, then replays its welcome, resumed/mid-session conversation, and active stream rows at the new width. ([#173](https://github.com/Fullstop000/ignis/pull/173))
- LLM — provider requests now time out after 120 s waiting for the first byte, and retries are restricted to transient failures (timeouts, connection errors, 5xx) instead of burning budget on 4xx or malformed responses. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- Reliability — permission/MCP/skill lock poisoning no longer panics; the runtime recovers safely. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- MCP — shutdown escalates from SIGTERM to SIGKILL only after confirming the process group is still alive. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- Tools — `bash` now rejects a missing or non-directory `cwd` before spawning the shell. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- Security — `web_fetch` `domain:` permission rules now parse the URL with the same `reqwest` URL parser used for the actual request, closing bypasses where percent-encoded dots (`evil%2ecom`) or uppercase hosts (`EVIL.com`) slipped past a `domain:evil.com` deny rule. ([#188](https://github.com/Fullstop000/ignis/pull/188))
- Tests — the flaky pty-timing integration test `inline_resize_replays_stable_rows_from_an_active_stream` is now `#[ignore]`d under parallel load; run it in isolation with `--ignored`. ([#187](https://github.com/Fullstop000/ignis/pull/187))
- Sessions — a corrupt legacy `.json` transcript (partial write, hand-edit, truncation) no longer aborts startup for that directory; it degrades to empty history with a warning so you can still reach the TUI. ([#190](https://github.com/Fullstop000/ignis/pull/190))
- Sessions — `/sessions`, `--resume`, and auto-resume no longer panic when a session's first message starts with a CJK/emoji character that straddles the preview cut-off; truncation is now character-based. ([#190](https://github.com/Fullstop000/ignis/pull/190))
- Sessions — compaction no longer silently keeps a truncated summary when the summarization request drops mid-stream; the error now surfaces and your real history is left intact. ([#190](https://github.com/Fullstop000/ignis/pull/190))
- LLM — the Anthropic protocol now reports token usage, so the context-% meter, `/settings` stats, and cost telemetry are no longer stuck at zero on Claude and the MiniMax default endpoint. ([#190](https://github.com/Fullstop000/ignis/pull/190))
- Tools — `bash` kills a timed-out command's process instead of leaving it running to race the next bash call. ([#190](https://github.com/Fullstop000/ignis/pull/190))
- Tools — `edit_file` now errors when `old_text` matches more than one place in the file instead of silently editing the first; add surrounding context or set `global_replace=true`. ([#190](https://github.com/Fullstop000/ignis/pull/190))
- Tools — `read_file` no longer prints `... [truncated]` on a file it read completely (off-by-one), and hints when an `offset` lands past the end of the file. ([#190](https://github.com/Fullstop000/ignis/pull/190))
- Sessions — the cumulative-token sidecar (`.usage.json`) is now written atomically, so a crash mid-write can no longer reset your token counters to zero. ([#190](https://github.com/Fullstop000/ignis/pull/190))

## [0.38.1] - 2026-06-12

### Changed
- Internal — the status footer and the composer input box are extracted into self-contained `FooterProps`/`ComposerProps` view components (props struct + ratatui `Widget` + one `From<&App>`), making each panel's rendering unit-testable in isolation. No user-visible change. ([#166](https://github.com/Fullstop000/ignis/pull/166))
- Internal — the loading line, queued-prompts strip, and slash-suggestion list are extracted into `LoadingProps`/`QueuedProps`/`SlashProps` view components, completing the bottom-band decomposition. No user-visible change. ([#167](https://github.com/Fullstop000/ignis/pull/167))
- Internal — the `/settings` panel's live stats are decoupled from the `App` struct into a `SettingsData` view model, making the Stats and Statusline tabs unit-testable in isolation. No user-visible change. ([#168](https://github.com/Fullstop000/ignis/pull/168))
- Docs — the TB 2.1 leaderboard now lists the `deepseek-v4-pro@max` (Ark) row and clarifies the cache column. ([#169](https://github.com/Fullstop000/ignis/pull/169))

### Fixed
- TUI — `ignis --resume <id>` now paints your prior conversation on launch instead of leaving the chat history blank. ([#165](https://github.com/Fullstop000/ignis/pull/165))
- Benchmarks — `bundle_traces.py` and `generate_report.py` now redact any value behind `api_key = "…"` and any `Authorization: Bearer …` header in dumped configs and agent logs, closing the leak that exposed an unprefixed Ark API token during the `deepseek-v4-pro@max` run. ([#170](https://github.com/Fullstop000/ignis/pull/170))

## [0.38.0] - 2026-06-10

### Added
- TUI — the model's thinking now collapses to a rolling 3-line preview while it streams, finalizing to a one-line summary; press `Ctrl+O` to expand the full chain-of-thought. ([#156](https://github.com/Fullstop000/ignis/pull/156))
- TUI — press `r` in the `/skills` picker to reload skills from disk, picking up newly added, edited, or removed skills without restarting. ([#157](https://github.com/Fullstop000/ignis/pull/157))
- TUI — exiting with `Ctrl+D` prints a copy-pasteable `ignis --resume <id>` hint so you can pick the session back up. ([#158](https://github.com/Fullstop000/ignis/pull/158))

### Changed
- Internal — the inline TUI render loop is restructured for testability: the re-anchor/commit state machine (screen-clear episodes, resize settle, commit row budget) moves into a pure `render::anchor` module whose historical bugs (#138, #140, #154, #155) are now table-driven regression tests, and the frame loop itself becomes a `ConsoleLoop` lifecycle skeleton mirroring `Agent::run`. No user-visible change. ([#163](https://github.com/Fullstop000/ignis/pull/163))

### Fixed
- TUI — on WSL2/conpty, the conversation no longer stays blank after you send a message while the agent keeps working; inline rendering recovers instead of only repainting on resume. ([#154](https://github.com/Fullstop000/ignis/pull/154))
- TUI — `/sessions` no longer crashes when resuming a long transcript; the history is now committed to scrollback in bounded chunks instead of one oversized buffer that overflowed ratatui's cell limit. ([#155](https://github.com/Fullstop000/ignis/pull/155))
- TUI — the footer's context % now tracks the active model's context window after switching models via `/connect`, instead of measuring against the previously-selected model's window. ([#160](https://github.com/Fullstop000/ignis/pull/160))
- TUI — wide markdown tables now wrap to fit the terminal instead of sprawling past the screen as a garbled box. ([#161](https://github.com/Fullstop000/ignis/pull/161))

### Security
- External hooks now run sandboxed by default: an env-var allowlist keeps secrets like `ANTHROPIC_API_KEY` out of hook subprocesses, and a filesystem sandbox (Linux Landlock / macOS Seatbelt) confines them to a small set of allowed paths. ([#109](https://github.com/Fullstop000/ignis/pull/109))

## [0.37.1] - 2026-06-09

### Added
- Providers — Ark Coding Plan (`ark-coding`) — Volcengine's flat-fee subscription aggregating 10 models (`doubao-seed-*`, `minimax-m{2.7,3}`, `glm-5.1`, `deepseek-v4-{flash,pro}`, `kimi-k2.6`). Set `ARK_CODING_PLAN_TOKEN` and pick `ark-coding/<model>` via `/model`. ([#149](https://github.com/Fullstop000/ignis/pull/149))
- TUI — `/settings` opens a control panel: a live **Stats** tab (context %, tokens, turns, tools, uptime) and a **Statusline** tab to show/hide individual status-bar segments. ([#151](https://github.com/Fullstop000/ignis/pull/151))

### Changed
- TUI — tool calls, your messages, and boxes restyled toward Claude Code's look: gutter-style tool blocks (a status-colored `●` bullet + `╰` result), a mauve rail down your turns, and rounded composer/code-fence corners. ([#153](https://github.com/Fullstop000/ignis/pull/153))

### Fixed
- TUI — no longer crashes at startup in a git repository whose working diff contains a multibyte character near the truncation point. ([#151](https://github.com/Fullstop000/ignis/pull/151))

## [0.37.0] - 2026-06-09

### Added
- TUI — `/sessions` shows a per-row title (from the session's first message) and hides the session you're already in. ([#144](https://github.com/Fullstop000/ignis/pull/144))
- Providers — Zhipu GLM (BigModel open platform, China) is now a built-in OpenAI-compatible provider; configure with `[providers.zhipu]` and `model = "zhipu/glm-5.1"`. ([#143](https://github.com/Fullstop000/ignis/pull/143))

### Changed
- TUI — `/connect`'s final step now imports the provider's whole model list into `/model` and just picks which one is active, offering a "Keep current model" row so rotating a key needn't switch models. ([#143](https://github.com/Fullstop000/ignis/pull/143))

### Fixed
- TUI — after `/connect`, the newly-connected provider's models now appear in `/model` in the same session (the picker list was only built at startup). ([#143](https://github.com/Fullstop000/ignis/pull/143))

## [0.36.2] - 2026-06-08

### Fixed
- TUI — resizing the terminal (e.g. dragging between monitors) no longer leaves duplicate input bars stacked on screen. ([#138](https://github.com/Fullstop000/ignis/pull/138))
- TUI — resuming a session via `/sessions` reliably repaints the conversation history instead of sometimes leaving a blank screen with only the input bar. ([#140](https://github.com/Fullstop000/ignis/pull/140))

## [0.36.1] - 2026-06-08

### Fixed
- TUI — `/model` picker anchors above the input (replacing it, CC-style) instead of taking over the whole body. The conversation in native scrollback above the TUI stays visible while the picker is open. ([#134](https://github.com/Fullstop000/ignis/pull/134))

## [0.36.0] - 2026-06-08

### Added
- TUI — the input bar shows a `❯` prompt at its left edge. ([#133](https://github.com/Fullstop000/ignis/pull/133))

### Changed
- TUI — invoking a skill (`/skill-name` or the `skill` tool) shows a compact line in the transcript instead of the full skill body. ([#136](https://github.com/Fullstop000/ignis/pull/136))

### Fixed
- TUI — a momentarily unresponsive terminal no longer crashes the session mid-render; the inline view rides out the hiccup and recovers. ([#135](https://github.com/Fullstop000/ignis/pull/135))

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
