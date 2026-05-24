# CLAUDE.md - Ignis Development Guide

This guide contains commands, patterns, and style rules for developers and AI assistants working on the `ignis` codebase.

## Build and Run Commands

*   **Build all workspace crates:** `cargo build`
*   **Run interactive TUI (default):** `cargo run`
*   **Run interactive TUI (explicit):** `cargo run -- --tui`
*   **Run one-shot CLI:** `cargo run -- <prompt>`
*   **Clippy/Lints check:** `cargo clippy --workspace --all-targets -- -D warnings`
*   **Rust Formatter:** `cargo fmt --all -- --check`

## Testing Commands

*   **Run all unit tests:** `cargo test --workspace`
*   **Run specific test file or pattern:** `cargo test <test_name>`

---

## Coding Guidelines & Style Rules

### R1. Simplicity & Scope
*   Follow the **YAGNI** (You Aren't Gonna Need It) principle strictly.
*   Only add the minimum required dependencies to `Cargo.toml`.
*   Avoid adding speculative abstractions, compatibility shims, or unrelated code cleanups.
*   Keep changes tightly scoped to the current active goal.

### R2. TUI Design
*   The primary UI is a **native terminal TUI** built with `ratatui` + `crossterm`.
*   Keep the TUI responsive by processing `AgentEvent` updates at ~30fps.
*   Use the established dark color palette (defined in `src/tui.rs`).
*   Tool call blocks use color-coded borders: yellow=pending, green=success, red=error.
*   Ignis ships as a **single binary** — no external runtime dependencies.

### R3. Quality & Warning Gate
*   Maintain **zero compiler warnings and clippy errors** in the Rust crates.
*   Ensure all unit tests pass before finishing a task.

