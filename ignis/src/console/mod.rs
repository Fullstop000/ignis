//! Console layer — TUI runner + per-frame draw + key dispatch + slash
//! commands. The legacy single-file `console::mod` grew large; the concerns
//! now split into focused submodules. `pub use` re-exports below preserve
//! every `crate::console::*` path the rest of the crate already uses, so
//! the split is a pure organizational change.

pub mod app;
pub mod clipboard;
pub(crate) mod colors;
pub(crate) mod format;
pub mod highlight;
pub(crate) mod inline_picker;
pub(crate) mod keys;
pub mod markdown;
pub(crate) mod picker;
pub mod render;
pub(crate) mod runner;
pub(crate) mod slash;

// Re-exports for paths the rest of the crate already uses.
pub use runner::run_console;

pub(crate) use colors::{
    ACCENT, BG, BORDER, BORDER_ACTIVE, CODE_BG, DIFF_ADD_BG, DIFF_DEL_BG, GREEN, LAVENDER, MAUVE,
    PEACH, RED, SPINNERS, SUBTEXT, SURFACE, SURFACE_2, TEAL, TEXT, TEXT_DIM, THINKING_VERBS,
    YELLOW,
};
pub(crate) use format::{
    format_context, format_duration, format_tokens, next_selection, sanitize, truncate,
    SelectionDirection,
};
pub(crate) use slash::{slash_suggestions, SlashCommand};
