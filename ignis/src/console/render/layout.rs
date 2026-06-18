//! Inline-viewport geometry — the band/viewport row math the runner uses to
//! size (and decide when to rebuild) the `Terminal`, and that `draw` uses to lay
//! out the same region. Pulled out of `mod.rs` so the layout chokepoint every
//! new picker has to touch lives on its own, away from the frame orchestrator.

use crate::console::app::App;

use super::blocks::REASONING_PREVIEW_LINES;
use super::widgets::{queued_region_height, MAX_SLASH_ROWS};

/// Rows reserved above the band for the live reasoning preview: the header +
/// the rolling window. Zero unless a thought is streaming in collapsed mode.
pub(crate) fn reasoning_preview_height(app: &App) -> u16 {
    if app.live_reasoning().is_some() {
        1 + REASONING_PREVIEW_LINES as u16
    } else {
        0
    }
}

/// Height (rows) of the bottom band — status line + queued strip + slash
/// suggestions + input box + footer. Independent of the transcript above it.
pub(crate) fn band_height(app: &App, term_rows: u16) -> u16 {
    let cap = term_rows.saturating_sub(1).max(3);
    // While a picker is open the user is answering, not typing — the band
    // collapses to status + footer and the picker takes the room above
    // (it replaces the input region rather than sitting beside an empty box).
    if picker_open(app) {
        return 2.min(cap);
    }
    let input_h = input_height(app, cap);
    let sugg = app.slash_suggestions();
    let sugg_h = if !sugg.is_empty() {
        (sugg.len() as u16).min(MAX_SLASH_ROWS)
    } else {
        0
    };
    let queued_h = queued_region_height(app);
    (1 + queued_h + sugg_h + input_h + 1).min(cap)
}

/// Input box height (incl. borders), growing with newline-separated lines.
pub(crate) fn input_height(app: &App, cap: u16) -> u16 {
    let lines = app.composer.input.split('\n').count().max(1) as u16;
    (lines + 2).clamp(3, cap.saturating_sub(2).max(3))
}

/// Whether any picker is currently open (tool-initiated `ask_user` or a
/// slash-command picker). When one is, the inline viewport grows to give it
/// room above the band.
pub(crate) fn picker_open(app: &App) -> bool {
    app.inline_picker.is_some()
        || app.model_picker.is_some()
        || app.session_picker.is_some()
        || app.skill_picker.is_some()
        || app.mcp_picker.is_some()
        || app.settings_panel.is_some()
}

/// Height (rows) of the inline viewport. The runner rebuilds the `Terminal`
/// whenever this changes — which, with a fixed band, is only on picker
/// open/close or multi-line input growth.
///
/// - `ask_user` (a tool-initiated `inline_picker`): just the picker + the
///   status/footer band, so the conversation stays visible in native scrollback
///   *above* the picker (it replaces the input region, CC-style).
/// - `/model`: just the picker + band, anchored above the input so the
///   conversation remains visible in scrollback.
/// - slash-command pickers (`/sessions`, `/skills`, `/mcp`): the whole terminal,
///   since they're entered intentionally and benefit from the room.
/// - otherwise: just the band.
pub(crate) fn viewport_height(app: &App, term_cols: u16, term_rows: u16) -> u16 {
    let cap = term_rows.saturating_sub(1).max(3);
    if let Some(p) = &app.inline_picker {
        let picker_h = crate::console::inline_picker::picker_height(p, term_cols);
        return (picker_h + 2).min(cap); // picker + status + footer
    }
    if let Some(picker) = &app.model_picker {
        let ph = model_picker_height(picker, &app.model_options);
        let bh = band_height(app, term_rows);
        return (ph + bh).min(cap);
    }
    if app.session_picker.is_some()
        || app.skill_picker.is_some()
        || app.mcp_picker.is_some()
        || app.settings_panel.is_some()
    {
        return cap;
    }
    // No picker: the band fills the viewport, plus the reasoning preview region
    // (if a thought is streaming collapsed) sits above it.
    (band_height(app, term_rows) + reasoning_preview_height(app)).min(cap)
}

/// Natural height (rows) of the `/model` picker content. Used by
/// `viewport_height` so the picker only takes the room it needs instead
/// of consuming the whole terminal.
///
/// **Selection-stable** — the height is computed from the option set, not
/// from `picker.selected`. Otherwise a single `↓` over the scroll-window
/// boundary (or across a reasoning↔non-reasoning model) would change the
/// viewport height and trigger a full terminal rebuild + flicker. The
/// actual rendered height is at most the worst case; any leftover rows
/// in `body_area` simply render blank.
pub(crate) const MODEL_PICKER_MAX_OPTION_ROWS: usize = 15;

pub(crate) fn model_picker_height(
    _picker: &crate::console::app::ModelPicker,
    options: &[crate::llm::ModelOption],
) -> u16 {
    if options.is_empty() {
        return 3; // blank + header + "No models configured."
    }
    // Worst case across all selections:
    // - effort row is shown iff any option declares levels
    // - `↑ N more` / `↓ N more` appear iff the list overflows the cap
    let any_has_effort = options.iter().any(|o| !o.effort_levels.is_empty());
    let overflows = options.len() > MODEL_PICKER_MAX_OPTION_ROWS;
    let visible = options.len().min(MODEL_PICKER_MAX_OPTION_ROWS) as u16;
    let has_above_below = overflows as u16;
    // blank + header + effort? + above? + visible + below? + footer
    2 + any_has_effort as u16 + has_above_below + visible + has_above_below + 1
}
