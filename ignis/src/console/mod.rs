//! Console layer — TUI runner + per-frame draw + key dispatch + slash
//! commands. The legacy single-file `console::mod` grew large; the concerns
//! now split into focused submodules. `pub use` re-exports below preserve
//! every path the rest of the crate (and external callers) actually use;
//! internal-only items that were previously hoisted as crate-visible
//! constants live on their submodules and are imported directly via
//! `crate::console::<submodule>::*`.

pub mod app;
pub mod clipboard;
pub(crate) mod colors;
pub(crate) mod composer;
pub(crate) mod connect;
pub(crate) mod format;
pub mod frontend;
pub(crate) mod git;
pub mod highlight;
pub(crate) mod inline_picker;
pub(crate) mod keys;
pub mod markdown;
pub(crate) mod picker;
pub(crate) mod pickers_state;
pub mod render;
pub(crate) mod render_diag;
pub(crate) mod runner;
pub(crate) mod slash;
pub(crate) mod transcript;

// Re-exports for paths the rest of the crate already uses.
pub use runner::{run_console, run_engine};

pub(crate) use colors::{
    ACCENT, BG, BORDER, BORDER_ACTIVE, CODE_BG, DIFF_ADD_BG, DIFF_DEL_BG, GREEN, LAVENDER, MAUVE,
    PEACH, RED, SPINNERS, SUBTEXT, SURFACE, SURFACE_2, TEAL, TEXT, TEXT_DIM, THINKING_VERBS,
    YELLOW,
};
pub(crate) use format::{
    format_context, format_duration, format_elapsed, format_tokens, next_selection, sanitize,
    truncate, SelectionDirection,
};
pub(crate) use slash::{slash_suggestions, SlashCommand};
