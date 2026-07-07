# CLAUDE.md - Ignis Development Guide

This guide contains commands, patterns, and style rules for developers and AI assistants working on the `ignis` codebase.

## Build and Run Commands

*   **Build all workspace crates:** `cargo build` (workspace = `ignis` + `ignis-macros`; `ignis-tui/` is a separate Node/Ink project, not a Cargo crate).
*   **Run interactive TUI (default):** `cargo run` — no-arg launches the TUI. The default UI is the **Ink frontend** (`ignis-tui/`, requires Node ≥18); `IGNIS_FRONTEND=native` forces the built-in `ratatui` TUI, which is also the automatic fallback when Node is missing. (The `--tui` flag was removed in v0.15.0 — no-arg *is* the TUI.)
*   **Run one-shot CLI:** `cargo run -- <prompt>` — streams a single turn to stdout and exits.
*   **Subcommands:** `ignis mcp`, `ignis upgrade` (alias `update`), `ignis sessions`.
*   **Other flags:** `-r, --resume [ID]`, `--afk` (fully unattended), `-v, --version`.
*   **Clippy/Lints check:** `cargo clippy --workspace --all-targets -- -D warnings`
*   **Rust Formatter:** `cargo fmt --all -- --check`

## Testing Commands

*   **Run all unit tests:** `cargo test --workspace`
*   **Run specific test file or pattern:** `cargo test <test_name>`

## Architecture

The Rust core lives in `ignis/src/` as directory modules sharing one `Session`-centric loop — entry/UI at the edges, engine in the middle:

*   `agent/` — stateless turn-execution engine; runs the tool-dispatch loop and streams `AgentEvent`s (message deltas, tool start/end, turn end).
*   `session/` — core conversational model; owns message `history` + persistence and wraps an `Agent`, advancing the conversation via `Session::prompt` / `compact`.
*   `llm/` — LLM domain: model catalog, provider-brand declarations, and wire protocols (Anthropic / OpenAI-compatible).
*   `tools/` — built-in tool registry (`bash`, `read_file`, `edit_file`, `grep`, `glob`, `agent` sub-agent, `ask_user`, `todo_write`, web, worktree, …) plus the `#[tool]` trait machinery.
*   `permissions/` — the tool-call gate; a single 3-state `Mode` (`Off` / `HandsFree` / `FullyUnattended`) + rule set, enforced via a `PermissionChecker` `ToolHooks` impl.
*   `hooks/` — external subprocess hooks on `UserPromptSubmit`, `PreToolUse`/`PostToolUse`, and `AssistantMessageRender`; failures degrade to "use original + warn" and never kill a turn.
*   `sandbox/` — policy-free process-confinement primitive (Landlock on Linux, Seatbelt on macOS) shared by hook and bash subprocesses.
*   `mcp/` — Model Context Protocol client; spawns configured stdio/HTTP MCP servers and exposes their tools as `mcp__<server>__<tool>`.
*   `skills/` — user `SKILL.md` instruction sets discovered from disk, advertised to the model, loaded on demand, toggleable at runtime.
*   `console/` — the TUI layer: `runner` (event loop + ~30fps frame tick), `app` (state), `render/` (draw), `keys`/`slash`/`composer`/`pickers`, and `frontend/` (the headless `--engine` NDJSON protocol the Ink host drives).
*   `cli/` — the clap CLI surface (flags + `mcp`/`upgrade`/`sessions` subcommands) and the Ink-frontend resolver.

Cross-cutting top-level files: `main.rs` (routing — `--engine` headless vs TUI vs one-shot), `config.rs` (TOML config + provider/model resolution), `state.rs` (persisted `state.json`: mode, grants, disabled skills/MCP), `telemetry.rs` (opt-in OpenTelemetry).

---

## Coding Guidelines & Style Rules

### R1. Simplicity & Scope
*   Follow the **YAGNI** (You Aren't Gonna Need It) principle strictly.
*   Only add the minimum required dependencies to `Cargo.toml`.
*   Avoid adding speculative abstractions, compatibility shims, or unrelated code cleanups.
*   Keep changes tightly scoped to the current active goal.

### R2. TUI Design
*   Two frontends share one Rust core: the **Ink frontend** (`ignis-tui/`, Node/React-Ink — default when Node ≥18 is present, since v0.40.0) and the **built-in `ratatui` TUI** (`crossterm` backend, fallback). The Ink host owns the terminal and spawns the Rust binary as a headless `--engine` over an NDJSON stdin/stdout protocol; `IGNIS_FRONTEND=native` forces the built-in.
*   The built-in TUI renders at a ~30fps frame tick (`FRAME = 33ms` in `console/runner.rs`); `AgentEvent`s stream in between frames and are coalesced into the next draw.
*   Use the established dark color palette (Catppuccin Mocha, defined in `src/console/colors.rs`).
*   Tool call blocks encode status via a color-coded **bullet** (`●`): yellow=pending, green=success, red=error (`console/render/tool_block.rs`).
*   The Rust core ships as a **single binary**; the default Ink frontend additionally requires Node ≥18 (the `ratatui` TUI has no external runtime deps).

### R3. Quality & Warning Gate
*   Maintain **zero compiler warnings and clippy errors** in the Rust crates.
*   Ensure all unit tests pass before finishing a task.

