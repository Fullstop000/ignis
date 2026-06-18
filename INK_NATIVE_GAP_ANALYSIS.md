# Gap Analysis: Native (Rust) Runner vs Ink (Terminal-UI) Frontend

**Scope:** Compare the Ignis native terminal runner (`ignis/src/console/runner.rs`, `app.rs`, `tui.rs`) with the Ink-based TypeScript TUI (`ignis-tui/src/`) to identify features, event handling, CLI/config interactions, and rendering behaviors that would need to be replicated, adapted, or reconciled when building a native TUI.

**Date:** 2026-06-18

---

## 1. Architectural Mismatch

| Aspect | Native Runner (Rust) | Ink TUI (TypeScript) | Gap |
|--------|----------------------|----------------------|-----|
| **Mode** | One-shot CLI by default; `--tui` launches an embedded native TUI (`ratatui` + `crossterm`) but currently it is a thin placeholder. | Full-screen interactive React/Ink terminal UI. | Native TUI is incomplete; the Ink frontend is the current "real" interactive UI. |
| **Process boundary** | Single binary; TUI runs in-process. | Separate Node.js process spawned by Rust (`src/console/tui_bridge.rs`), communicates over stdin/stdout via JSON-RPC-like `ClientCommand`/`Outbound` protocol. | Native TUI must absorb the protocol surface area that currently crosses the process boundary. |
| **Rendering model** | Frame-based `ratatui` terminal UI, no React lifecycle. | React/Ink component lifecycle, state diffing, effects. | Some Ink features (suspense-ish loading states, focus management, exit hooks) need imperative equivalents. |

---

## 2. Console / TUI Surface (`src/console/`)

### 2.1 `app.rs` — Application State Machine

The native runner already maintains an in-memory `App` state machine. Key fields that must map to TUI behavior:

- **`messages: Vec<Message>`** — Conversation history. Ink's `messages` state is equivalent.
- **`scrollback: Vec<ScrollbackItem>`** — Rendered lines including tool blocks, notices, warnings, reconnects, and errors. Ink builds a `Message` array with variants `user`, `assistant`, `tool`, `error`, `notice`.
- **`pending_request: Option<PendingRequest>`** — Stores the current streaming request for snapshot/resume. Ink mirrors this in `reduceOutbound` when receiving a `snapshot`.
- **`config: Config`** — Resolved configuration. Must be available to native TUI.
- **`session_id` / `session_name`** — Session identity for `/save`, `/load`, `/fork`. Ink displays these in the status bar.
- **`context_files: Vec<ContextFile>`** — Files attached via `/add` or drag-and-drop. Ink shows an attached-files panel.
- **`usage`** — Aggregated token usage. Ink shows it in footer.
- **`status: AppStatus`** — `Idle`, `Running`, `WaitingForTool`, `Paused`. Native must render these.
- **`error`** — Fatal or non-fatal error to display. Ink renders a blocking error overlay.
- **`quit` / `interactive`** — Exit conditions.

#### Gaps
1. **Input method / history** — Native uses `crossterm` for raw input; must re-implement:
   - Multiline prompt editing.
   - Up/down history navigation (native already has `history` in one-shot mode, TUI does not expose it).
   - Tab completion for slash commands (`/add`, `/save`, `/model`, etc.).
   - Clipboard / OSC 52 integration.
2. **Completions overlay** — Ink has a `Completions` component for slash-command autocomplete; native has none.
3. **Focused UI areas** — Ink splits focus between `InputBox`, `Messages`, `Sidebar`, `ErrorDisplay`. Native `App` has `Focus` enum but TUI rendering is minimal.
4. **Theme / color palette** — `tui.rs` defines a dark palette. Ink has its own `Theme` object with `colors` and `styles`. Need to align or translate.

### 2.2 `runner.rs` — Driver Loop

The driver loop processes `ClientCommand`s from the frontend and emits `Outbound::Event(AgentEvent)` or `Outbound::Snapshot`.

#### Commands the native TUI must send (same as Ink)
- `Submit { text }` — Send user prompt.
- `Interrupt` — Stop generation.
- `ToolConfirm { tool_call_id, confirmed }` — Approve/deny a tool.
- `AddContext { path }` — Attach file.
- `RemoveContext { index }` — Detach file.
- `LoadSession { path_or_name }`, `SaveSession { path_or_name }`, `ForkSession { path_or_name }` — Session ops.
- `SetModel { model }` — Switch model.
- `Shutdown` — Graceful exit.

#### Gaps
1. **Shutdown path** — Ink's `cleanExit()` ends the engine stdin; native must explicitly send `ClientCommand::Shutdown` or rely on EOF. Runner already handles `Shutdown` and EOF correctly.
2. **Snapshot hydration** — `Outbound::Snapshot { app, pending_request }` is sent on connect/resume. Ink's `reduceOutbound` reconstructs `request` from `pending_request`. Native TUI must do equivalent hydration of `App` state.
3. **Pending tool confirmation** — When model emits a tool needing confirmation, driver waits on a channel. Native must render a confirmation prompt and send `ToolConfirm`. Ink currently uses `useToolConfirmation` hook; native needs a blocking or event-driven prompt.
4. **Interruption during tool execution** — `Interrupt` while a tool is running cancels the tool future. Native TUI must surface this.
5. **Hook output streaming** — `UserPromptCommitted` and `Notice` events carry hook-mutated text. Native must render them (already done in one-shot CLI; TUI must mirror).

### 2.3 Event Handling

All `AgentEvent` variants are already defined in `agent/mod.rs`. Ink's `protocol.js` reduces every variant except `run_start`/`run_end`.

| Event | Native `App::apply_event` | Ink `reduceEvent` | Gap |
|-------|---------------------------|-------------------|-----|
| `TurnStart` | ✅ | ✅ | None |
| `RunStart` | ✅ (logs) | ignored | Minor — no visible gap. |
| `MessageStart` | ✅ | ✅ | None |
| `MessageUpdate` | ✅ | ✅ | None |
| `MessageEnd` | ✅ | ✅ | None |
| `ToolExecutionStart` | ✅ | ✅ | None |
| `ToolExecutionEnd` | ✅ | ✅ | None |
| `RunEnd` | ✅ | ignored | Minor. |
| `Usage` | ✅ | ✅ | None |
| `TurnEnd` | ✅ | ✅ | None |
| `UserInjected` | ✅ | ✅ | None |
| `Reconnecting` | ✅ | ✅ | None |
| `UserPromptCommitted` | ✅ | ✅ | None |
| `Warning` | ✅ | ✅ | None |
| `Notice` | ✅ | ✅ | None |

**Finding:** The native event model already supports everything Ink supports. The gap is *rendering*, not protocol coverage.

---

## 3. CLI / Config Interactions

### 3.1 Startup Arguments

Native binary accepts:
- `--tui` — launch native TUI.
- `--session <name>` — resume session.
- `--model <name>` — override model.
- `--config <path>` — config file.
- `--no-config` — skip config.
- Positional prompt for one-shot mode.

Ink currently is launched by Rust with a fixed command. To reach parity, native TUI must:
1. Parse all CLI flags currently passed to the engine and expose them as initial state.
2. Support `--session` and `--model` initial overrides in the TUI boot path.

### 3.2 Config File (`config.toml`)

| Section | Native Usage | Ink Exposure | Gap |
|---------|--------------|--------------|-----|
| `model` | `default`, `provider` | Status bar, `/model` command | Native TUI must display and allow override. |
| `ui` | `theme`, `show_usage`, `compact` | `Theme` object, usage footer | Need to load `ui` settings and apply palette. |
| `context` | `auto_add_patterns` | Auto-attach matching files | Native must implement. |
| `keybindings` | Not present in native; Ink has hard-coded keys. | Hard-coded | Opportunity to make keybindings configurable in native. |
| `hooks` | Hook chains | `/hooks` status | Native TUI should surface active hooks. |
| `tools` | `enabled`, per-tool config | Tool settings panel | Ink has a tool-settings UI; native does not. |
| `sessions` | `save_dir`, `auto_save` | Save/load dialogs | Native must implement save/load/fork UI. |

### 3.3 Slash Commands

Native `handle_command` in `runner.rs` already implements slash commands. Ink exposes these via the input box parser. Native TUI must provide:
- `/add <path>` — file picker or path completion.
- `/drop <index>` — context list management.
- `/model <name>` — model switch with validation.
- `/save [name]` / `/load <name>` / `/fork [name]` — session dialogs.
- `/clear` — clear conversation.
- `/exit` or `/quit` — exit.
- `/help` — command help overlay.

**Gap:** Native one-shot runner already handles all slash commands; native TUI needs the *interactive UI* for each.

---

## 4. Rendering Gaps in Native TUI (`tui.rs`)

Current `tui.rs` is a skeleton. To match Ink it must implement:

1. **Message list with variants:**
   - User prompt blocks.
   - Assistant markdown/code blocks.
   - Tool call blocks with color-coded borders (yellow pending, green success, red error).
   - Error blocks.
   - Notice / warning lines.
   - Reconnecting spinner.
2. **Input box:**
   - Multi-line textarea.
   - Placeholder text.
   - Character counter / token estimate.
   - Submit on Enter, newline on Shift+Enter.
3. **Status bar / footer:**
   - Model name, provider, session name.
   - Token usage (if enabled).
   - Current status / spinner.
4. **Sidebar / panels:**
   - Attached context files list.
   - Tool settings.
   - Session list.
5. **Overlays / modals:**
   - Error overlay.
   - Tool confirmation dialog.
   - Save/load/fork dialogs.
   - Help screen.
   - Completions menu.
6. **Scrolling:**
   - Auto-scroll during streaming.
   - Manual scroll override.
   - Scroll-to-bottom indicator.

### 4.1 Tool Block Rendering Detail

Ink renders tool calls as collapsible blocks with:
- Header: tool name + call ID (truncated).
- Arguments: syntax-highlighted JSON.
- Result: success/error with timing.

Native must produce equivalent blocks. `ToolResult` in Rust contains `content` and `is_error`; formatting must handle:
- Long results with scroll/copy.
- Error tool results in red.
- Pending tool results with spinner.

### 4.2 Markdown / Code

Ink uses a custom message renderer for markdown and code blocks. Native TUI can use `ratatui`'s `Paragraph` with `Line`/Span` plus a lightweight markdown-to-spans converter (e.g. `pulldown-cmark` + custom styling) or keep it plain-text initially.

---

## 5. Key Handling Gaps

Ink hard-codes:
- `Ctrl+C` / `Esc` — interrupt or exit.
- `Enter` — submit, `Shift+Enter` — newline.
- `Tab` / `Shift+Tab` — focus switching.
- `/` — start slash command.
- `Up` / `Down` — history or scroll.
- `Ctrl+L` — clear screen.
- `Ctrl+S` — save session.

Native `crossterm` key handling must be wired to equivalent `App` actions. There is currently no keybinding layer.

---

## 6. Lifecycle / Error Handling Gaps

| Scenario | Native | Ink | Gap |
|----------|--------|-----|-----|
| Startup failure (bad config, no API key) | Returns error to shell, no TUI. | Renders error overlay and exits. | Native TUI should show a blocking error and wait for keypress. |
| Provider stream error | Emits `Warning`/`Notice`, may reconnect. | Shows inline error + reconnect spinner. | Need inline error/reconnect UI. |
| Tool confirmation timeout | Driver has no timeout today. | Shows prompt until user acts. | Define timeout policy or keep indefinite. |
| Session save failure | Returns error event. | Shows error overlay. | Native TUI must surface save errors. |
| Terminal resize | Handled by `ratatui` automatically. | Handled by Ink. | None — both handle resize. |

---

## 7. Recommendations

1. **Reuse the existing driver loop.** `runner.rs` and `app.rs` already implement the full protocol and state machine. The native TUI should be a new frontend that sends `ClientCommand`s and renders `Outbound` events, just like the Ink engine.
2. **Keep the Ink bridge alive during transition.** Do not delete `tui_bridge.rs` or `ignis-tui/` until native TUI reaches feature parity and is dogfooded.
3. **Build components in priority order:**
   1. Input box + scrollback renderer.
   2. Message streaming (assistant text + tool blocks).
   3. Interrupt + tool confirmation.
   4. Context files sidebar.
   5. Session save/load/fork dialogs.
   6. Slash-command completion + help.
   7. Settings/config UI.
4. **Use existing `AgentEvent`/`Outbound` types as the contract.** No changes needed to the core protocol.
5. **Add snapshot hydration tests** to ensure a resumed native TUI reconstructs the same visible state as a fresh Ink session.
6. **Document keybindings** in a way that can later become configurable.

---

## 8. Summary Table: High-Priority Gaps

| # | Gap | Effort | Risk |
|---|-----|--------|------|
| 1 | Full-screen input box with history, multiline, and submit behavior | Medium | Low |
| 2 | Scrollback renderer for all message/tool/notice/error variants | Medium | Medium |
| 3 | Tool execution confirmation UI | Low | Low |
| 4 | Context files sidebar and `/add`/`/drop` interactions | Medium | Low |
| 5 | Session save/load/fork dialogs | Medium | Medium |
| 6 | Slash-command autocomplete and help overlay | Low | Low |
| 7 | Error overlay and startup-failure handling | Low | Low |
| 8 | Theme/config-driven palette and UI toggles | Medium | Low |
| 9 | Configurable keybindings | Low | Low |
| 10 | Snapshot hydration / resume parity tests | Low | Medium |

The protocol and state-management foundations are already in place. The bulk of the work is implementing the interactive terminal UI surface on top of the existing Rust runtime.
