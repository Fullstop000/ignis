//! Tool-initiated interactive picker — shared types used by both the TUI
//! console and non-UI callers such as the `ask_user` tool and permission
//! checker.
//!
//! Keeping these types in a module independent of `console::picker` means
//! headless tools, tests, and permission logic can construct and receive
//! picker requests without depending on the TUI layer.
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// A single selectable option within a question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerOption {
    /// 1-5 word display text shown in the list.
    pub label: String,
    /// Longer text explaining the choice; shown below/with the label.
    pub description: String,
    /// Optional multi-line preview (code or ASCII) shown when this row has
    /// focus.
    pub preview: Option<String>,
}

/// One question with its options. The console renders these one at a time,
/// advancing on ENTER, finishing the request on the last question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerQuestion {
    /// The complete question text.
    pub question: String,
    /// Picker-kind label rendered before the header chip (e.g. "ask_user",
    /// "permission", "afk"). Tells the user *which* subsystem opened the
    /// picker. The channel is shared so this can't be inferred otherwise.
    pub kind: String,
    /// Short chip label (≤12 chars) shown in the header strip.
    pub header: String,
    /// `true` enables space-to-toggle multi-select; `false` is single-select.
    pub multi_select: bool,
    /// 2-4 options. The console appends an `"Other (type custom)…"` row on
    /// top of these for free-text answers when `allow_other` is true.
    pub options: Vec<PickerOption>,
    /// Show the `"Other (type custom)…"` free-text row? `true` for `ask_user`
    /// (the model invites the user to free-text anything). `false` for
    /// permission/AFK prompts where the option set is closed by design.
    pub allow_other: bool,
    /// Render as a single text-input row instead of a list of options. When
    /// true, `options` MUST be empty and the picker captures keystrokes into
    /// a free-text buffer; Enter returns `PickerAnswer::Single(<typed text>)`.
    /// Used by `/connect`'s API-key step.
    pub text_input: bool,
    /// When `text_input` is true, render the typed characters as `●` so the
    /// API key isn't shoulder-surfed. Has no effect when `text_input` is
    /// false.
    pub mask: bool,
}

/// A request from a tool to the console asking the user to pick. The `reply`
/// channel must be drained exactly once (Answered or Cancelled).
#[derive(Debug)]
pub struct PickerRequest {
    pub questions: Vec<PickerQuestion>,
    pub reply: oneshot::Sender<PickerResponse>,
}

/// What the console returns to the tool when the picker closes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PickerResponse {
    /// One answer per question, in the same order as `questions`.
    Answered(Vec<PickerAnswer>),
    /// User pressed ESC.
    Cancelled,
}

/// One question's answer. Shape mirrors what the tool returns to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PickerAnswer {
    /// Single-select pick (the option's label, or the user's text for "Other").
    Single(String),
    /// Multi-select picks in selection order (each is an option label, or the
    /// user's text for "Other"). Always non-empty — the console enforces it.
    Multi(Vec<String>),
}
