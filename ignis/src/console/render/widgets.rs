//! Shared bottom-band helpers — sizing math and the adaptive hint for the
//! queued strip, plus the selection-window math used by the slash suggestions
//! and every picker. The band panels themselves now render via their own view
//! components: `loading`, `queued`, `slash`, `composer`, `footer`.
use crate::console::app::{App, Mode};

/// Max queued rows shown before collapsing to a "+N more" row.
pub(crate) const MAX_QUEUE_ROWS: usize = 5;
/// Max slash-suggestion rows shown at once; the list scrolls to keep the
/// selected entry visible when there are more (e.g. many skills + `/skills`).
pub(crate) const MAX_SLASH_ROWS: u16 = 8;

/// Adaptive hint shown above the input while busy (None = no hint row).
pub(crate) fn queued_hint(app: &App) -> Option<String> {
    if app.mode == Mode::Idle {
        return None;
    }
    let has_queue = !app.queue.is_empty();
    let typing = !app.composer.input.is_empty();
    if !has_queue && !typing {
        return None;
    }
    Some(if has_queue {
        "↑ edit last · Enter queue · Ctrl+S send now".to_string()
    } else {
        "Enter queue · Ctrl+S send now".to_string()
    })
}

/// Height of the queued-rows + hint region between the status line and input.
pub(crate) fn queued_region_height(app: &App) -> u16 {
    if app.mode == Mode::Idle {
        return 0;
    }
    let shown = app.queue.len().min(MAX_QUEUE_ROWS) as u16;
    let overflow = if app.queue.len() > MAX_QUEUE_ROWS {
        1
    } else {
        0
    };
    let rows = if shown > 0 { 1 + shown + overflow } else { 0 }; // leading blank
    let hint = if queued_hint(app).is_some() { 1 } else { 0 };
    rows + hint
}

/// First index of the visible slash-suggestion window so that `sel` stays in
/// view: `[start, start+visible)` always contains `sel`.
pub(crate) fn slash_window_start(sel: usize, visible: usize, len: usize) -> usize {
    let visible = visible.max(1);
    let sel = sel.min(len.saturating_sub(1));
    if sel >= visible {
        sel - visible + 1
    } else {
        0
    }
}

/// Window `[start, end)` over a list of `len` items so that `sel` stays in
/// view in at most `visible` rows. Same rule as `slash_window_start` but
/// returns both ends — callers building a paginated picker can slice
/// directly and emit `↑/↓ N more` hints from the bounds.
pub(crate) fn picker_window(sel: usize, visible: usize, len: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let start = slash_window_start(sel, visible, len);
    let end = (start + visible.max(1)).min(len);
    (start, end)
}
