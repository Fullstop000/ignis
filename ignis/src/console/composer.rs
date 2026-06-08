//! The text composer — the input buffer, cursor, paste chips, and input
//! history. It owns the UTF-8 cursor/input invariant (`cursor` indexes `input`
//! by byte and must land on a char boundary) so callers never poke the raw
//! fields and risk a mid-character slice panic. Cross-cutting side effects
//! (the exit-hint, slash-autocomplete reset) stay in `App`, which wraps these
//! primitives — this struct is pure composer state with no knowledge of modes,
//! transcript blocks, or pickers.

/// A pasted block collapsed into a chip. The `placeholder` text lives inline in
/// the composer buffer; `expand_pastes` swaps it back for `content` at commit so
/// the agent never sees the chip. Placeholders are unique (monotonic counter).
pub(crate) struct PendingPaste {
    pub(crate) placeholder: String,
    pub(crate) content: String,
}

#[derive(Default)]
pub(crate) struct Composer {
    pub(crate) input: String,
    pub(crate) cursor: usize,
    /// Collapsed paste blocks (chip ↔ full content) currently in the buffer.
    pub(crate) pending_pastes: Vec<PendingPaste>,
    /// Monotonic `#N` counter for paste chips. Never reset, so numbering keeps
    /// climbing across messages and every placeholder string stays unique.
    paste_counter: usize,
    /// Submitted prompts, recalled with the Up arrow. Written via `push_history`.
    pub(crate) history: Vec<String>,
    history_idx: Option<usize>,
    saved_input: String, // saved input when browsing history
}

impl Composer {
    /// Byte offset of the char boundary one character left of the cursor.
    /// `cursor` indexes `input` by byte, so movement must step whole UTF-8
    /// chars — a naive `cursor -= 1` lands mid-character and panics on slice.
    fn prev_char_boundary(&self) -> usize {
        self.input[..self.cursor]
            .chars()
            .next_back()
            .map_or(0, |c| self.cursor - c.len_utf8())
    }

    /// Byte offset of the char boundary one character right of the cursor.
    fn next_char_boundary(&self) -> usize {
        self.input[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    pub(crate) fn insert_char(&mut self, c: char) {
        self.snap_out_of_chip();
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn insert_str(&mut self, s: &str) {
        self.snap_out_of_chip();
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_char_boundary();
        if let Some((start, end, i)) = self.chip_overlapping(prev, self.cursor) {
            self.remove_chip(start, end, i);
            return;
        }
        self.input.remove(prev);
        self.cursor = prev;
    }

    pub(crate) fn delete_forward(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = self.next_char_boundary();
        if let Some((start, end, i)) = self.chip_overlapping(self.cursor, next) {
            self.remove_chip(start, end, i);
            return;
        }
        self.input.remove(self.cursor);
    }

    /// Ctrl+W: delete the word before the cursor. A chip in the way is removed
    /// whole rather than half-eaten into a fragment.
    pub(crate) fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.input[..self.cursor];
        let trimmed = before.trim_end();
        let new_end = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        if let Some((start, end, i)) = self.chip_overlapping(new_end, self.cursor) {
            self.remove_chip(start, end, i);
            return;
        }
        self.input.replace_range(new_end..self.cursor, "");
        self.cursor = new_end;
    }

    /// A paste chip is atomic. If the buffer byte range `[lo, hi)` overlaps a
    /// chip, return its full span `[start, end)` and `pending_pastes` index, so
    /// an edit can remove the whole chip instead of leaving a corrupted
    /// placeholder fragment — which would both silently drop the pasted content
    /// and leak the partial chip text to the agent.
    fn chip_overlapping(&self, lo: usize, hi: usize) -> Option<(usize, usize, usize)> {
        self.pending_pastes.iter().enumerate().find_map(|(i, p)| {
            let start = self.input.find(&p.placeholder)?;
            let end = start + p.placeholder.len();
            (start < hi && lo < end).then_some((start, end, i))
        })
    }

    /// Remove the chip spanning `[start, end)` (pending index `i`); park the
    /// cursor where the chip began.
    fn remove_chip(&mut self, start: usize, end: usize, i: usize) {
        self.input.replace_range(start..end, "");
        self.cursor = start;
        self.pending_pastes.remove(i);
    }

    /// If the cursor sits strictly inside a chip, move it to the chip's end so
    /// an insert lands beside the chip rather than splitting the placeholder.
    fn snap_out_of_chip(&mut self) {
        if let Some((_, end, _)) = self.chip_overlapping(self.cursor, self.cursor) {
            self.cursor = end;
        }
    }

    /// Number of lines at or above which a paste collapses into a chip.
    const PASTE_COLLAPSE_MIN_LINES: usize = 4;

    /// Insert pasted text at the cursor. A multi-line block (>= 4 lines)
    /// collapses into a `[ pasted-text#N M lines ]` chip whose full content is
    /// stashed in `pending_pastes` and restored at commit; smaller pastes go in
    /// inline like ordinary typing.
    pub(crate) fn paste(&mut self, pasted: String) {
        // Normalize line endings so the chip's line count and the agent message
        // are clean regardless of where the text was copied from.
        let pasted = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let lines = pasted.split('\n').count();
        if lines >= Self::PASTE_COLLAPSE_MIN_LINES {
            self.paste_counter += 1;
            let placeholder = format!("[ pasted-text#{} {} lines ]", self.paste_counter, lines);
            self.insert_str(&placeholder);
            self.pending_pastes.push(PendingPaste {
                placeholder,
                content: pasted,
            });
        } else {
            self.insert_str(&pasted);
        }
    }

    /// Swap every paste chip back for its full content. Placeholders are unique,
    /// so a plain replace is unambiguous; a chip the user deleted is simply
    /// absent and replaces nothing.
    pub(crate) fn expand_pastes(&self, mut text: String) -> String {
        for p in &self.pending_pastes {
            text = text.replace(&p.placeholder, &p.content);
        }
        text
    }

    pub(crate) fn move_left(&mut self) {
        self.cursor = self.prev_char_boundary();
    }

    pub(crate) fn move_right(&mut self) {
        self.cursor = self.next_char_boundary();
    }

    /// Ctrl+A / Home: jump to the start of the buffer.
    pub(crate) fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// Ctrl+E / End: jump to the end of the buffer.
    pub(crate) fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// Ctrl+U / post-submit reset: empty the buffer, drop chips, park cursor.
    pub(crate) fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.pending_pastes.clear();
    }

    /// Replace the buffer wholesale (history recall, queued-prompt recall, slash
    /// suggestion commit) and park the cursor at the end.
    pub(crate) fn set_text(&mut self, text: String) {
        self.input = text;
        self.cursor = self.input.len();
    }

    /// Take the trimmed, paste-expanded buffer for submission and clear it.
    /// Returns `None` (leaving the buffer untouched) when it's blank — the agent
    /// must see the expanded content, never the `[ pasted-text#N ]` placeholder.
    pub(crate) fn take_submit(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let text = self.expand_pastes(text);
        self.clear();
        Some(text)
    }

    /// Record a submitted prompt into the recall history and reset the browse
    /// cursor so the next Up arrow starts from the newest entry.
    pub(crate) fn push_history(&mut self, text: String) {
        self.history.push(text);
        self.history_idx = None;
    }

    /// Stop browsing input history (drop the browse pointer) — called on session
    /// reset so a stale index can't linger into the next session.
    pub(crate) fn reset_history_browse(&mut self) {
        self.history_idx = None;
    }

    /// Up arrow: walk back through input history, stashing the in-progress
    /// buffer on the first step so Down can restore it.
    pub(crate) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.saved_input = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.set_text(self.history[idx].clone());
    }

    /// Down arrow: walk forward through input history; past the newest entry,
    /// restore the buffer stashed when history browsing began.
    pub(crate) fn history_next(&mut self) {
        let idx = match self.history_idx {
            None => return,
            Some(i) => i,
        };
        if idx + 1 >= self.history.len() {
            self.history_idx = None;
            self.set_text(self.saved_input.clone());
        } else {
            self.history_idx = Some(idx + 1);
            self.set_text(self.history[idx + 1].clone());
        }
    }
}
