//! Streaming suppression + parsing of the optional `<follow_ups>` block.
//!
//! The model may end a reply with:
//! ```text
//! <follow_ups>
//! - First suggested next prompt
//! - Second suggested next prompt
//! </follow_ups>
//! ```
//! The user should never SEE that block in the streamed reply, and it must not
//! be stored in history — it surfaces only as `AgentEvent::FollowUps` (rendered
//! as a strip in the Ink frontend). [`FollowUpStream`] does the streaming
//! suppression: callers feed it raw text deltas and emit only what it returns as
//! visible, then [`FollowUpStream::finish`] yields the parsed suggestions.
//!
//! Invariant: when no `<follow_ups>` marker appears, the concatenation of every
//! `push` return plus `finish().flush` is byte-identical to the raw input — the
//! common case is exactly today's behavior (only the chunk boundaries differ,
//! by at most a held-back partial-marker prefix).

const OPEN: &str = "<follow_ups>";
const CLOSE: &str = "</follow_ups>";
/// Cap on surfaced suggestions — a runaway list is noise, not help.
const MAX_ITEMS: usize = 4;

/// Streaming stripper for the trailing `<follow_ups>` block.
pub struct FollowUpStream {
    raw: String,
    /// Bytes of `raw` already returned as visible text.
    emitted: usize,
    /// Byte index where the `<follow_ups>` marker began, once seen. After this
    /// point all text is suppressed (the block runs to end-of-message).
    open_at: Option<usize>,
}

/// The result of finishing a [`FollowUpStream`].
pub struct FollowUpResult {
    /// Any held-back visible tail to emit as one final delta (empty once the
    /// marker is seen, or when nothing was held back).
    pub flush: String,
    /// Parsed suggestions (may be empty). Capped at [`MAX_ITEMS`].
    pub items: Vec<String>,
    /// Whether a `<follow_ups>` marker appeared. When true the caller trims
    /// trailing whitespace the model left before the (suppressed) block, so the
    /// reply doesn't end in dangling blank lines.
    pub had_block: bool,
}

impl FollowUpStream {
    pub fn new() -> Self {
        Self {
            raw: String::new(),
            emitted: 0,
            open_at: None,
        }
    }

    /// Feed a raw text delta; returns the substring safe to show now. Holds back
    /// a trailing partial that could still become `<follow_ups>` (so a partial
    /// marker is never shown and then un-showable).
    pub fn push(&mut self, delta: &str) -> String {
        self.raw.push_str(delta);
        if self.open_at.is_some() {
            return String::new(); // suppressing everything after the marker
        }
        if let Some(rel) = self.raw[self.emitted..].find(OPEN) {
            let abs = self.emitted + rel;
            self.open_at = Some(abs);
            let visible = self.raw[self.emitted..abs].to_string();
            self.emitted = abs;
            return visible;
        }
        let end = self.safe_end();
        let visible = self.raw[self.emitted..end].to_string();
        self.emitted = end;
        visible
    }

    /// Largest byte index `>= emitted` up to which it is safe to emit without
    /// showing a proper prefix of `OPEN` (which might still complete into the
    /// real marker on the next delta). `OPEN` is ASCII, so any matching suffix
    /// is ASCII and the returned index is always a char boundary.
    fn safe_end(&self) -> usize {
        let tail = self.raw.as_bytes();
        let start = self.emitted;
        let open = OPEN.as_bytes();
        let max_k = (open.len() - 1).min(tail.len() - start);
        for k in (1..=max_k).rev() {
            if tail[tail.len() - k..] == open[..k] {
                return tail.len() - k;
            }
        }
        self.raw.len()
    }

    /// Flush any held-back visible tail WITHOUT ending the stream (it may
    /// continue). Use at a text→reasoning boundary: a held partial marker there
    /// cannot be a real `<follow_ups>` (the block never spans a reasoning
    /// interruption — it is always the very end of the reply), so it is safe to
    /// release. Returns "" once the marker has been seen (already suppressing).
    pub fn flush_held(&mut self) -> String {
        if self.open_at.is_some() {
            return String::new();
        }
        let v = self.raw[self.emitted..].to_string();
        self.emitted = self.raw.len();
        v
    }

    /// Finish the stream: flush any safely-held tail (when no marker
    /// materialized) and parse the suggestions from the suppressed block.
    pub fn finish(mut self) -> FollowUpResult {
        let flush = if self.open_at.is_none() {
            let v = self.raw[self.emitted..].to_string();
            self.emitted = self.raw.len();
            v
        } else {
            String::new()
        };
        let items = match self.open_at {
            Some(p) => parse_items(&self.raw[p..]),
            None => Vec::new(),
        };
        FollowUpResult {
            flush,
            items,
            had_block: self.open_at.is_some(),
        }
    }
}

impl Default for FollowUpStream {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the suggestions out of a `<follow_ups> … </follow_ups>` block. Tolerant
/// of a missing close tag (truncated stream). Each non-empty line inside the
/// block, with a leading `-`/`*`/`•`/number bullet stripped, becomes one item.
fn parse_items(block: &str) -> Vec<String> {
    let inner = block
        .strip_prefix(OPEN)
        .unwrap_or(block)
        .split(CLOSE)
        .next()
        .unwrap_or("");
    let mut items = Vec::new();
    for line in inner.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let t = t
            .trim_start_matches(['-', '*', '•'])
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')')
            .trim();
        if !t.is_empty() {
            items.push(t.to_string());
        }
        if items.len() >= MAX_ITEMS {
            break;
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a stripper with a list of deltas; return (visible_text, items).
    fn run(deltas: &[&str]) -> (String, Vec<String>) {
        let mut s = FollowUpStream::new();
        let mut visible = String::new();
        for d in deltas {
            visible.push_str(&s.push(d));
        }
        let fin = s.finish();
        visible.push_str(&fin.flush);
        (visible, fin.items)
    }

    #[test]
    fn no_marker_is_byte_identical_to_input() {
        // The common case: whatever the chunking, the visible output equals the
        // raw input exactly (incl. trailing text that looks marker-ish but isn't).
        for deltas in [
            vec!["hello world"],
            vec!["hel", "lo ", "wor", "ld"],
            vec!["a < b and c > d"],
            vec!["talking about <follow", "ers> not the marker"],
            vec!["ends with <foll"], // partial prefix, never completed
        ] {
            let (visible, items) = run(&deltas);
            assert_eq!(visible, deltas.concat(), "deltas={deltas:?}");
            assert!(items.is_empty());
        }
    }

    #[test]
    fn suppresses_block_and_parses_items() {
        let (visible, items) =
            run(&["Here is the answer.\n\n<follow_ups>\n- Do X next\n- Then do Y\n</follow_ups>"]);
        assert_eq!(visible, "Here is the answer.\n\n");
        assert_eq!(items, vec!["Do X next", "Then do Y"]);
    }

    #[test]
    fn suppresses_across_delta_boundaries_with_split_marker() {
        // The marker is split across deltas — the partial must be held back, not
        // shown then retracted (deltas are append-only on the frontend).
        let (visible, items) = run(&["answer <foll", "ow_ups>\n- a\n- b\n</follow", "_ups>"]);
        assert_eq!(visible, "answer ", "partial marker must never leak");
        assert_eq!(items, vec!["a", "b"]);
    }

    #[test]
    fn tolerates_missing_close_tag() {
        let (visible, items) = run(&["done<follow_ups>\n- only one\n"]);
        assert_eq!(visible, "done");
        assert_eq!(items, vec!["only one"]);
    }

    #[test]
    fn strips_varied_bullets_and_caps_items() {
        let (_v, items) =
            run(&["<follow_ups>\n1. first\n* second\n• third\n- fourth\n- fifth\n</follow_ups>"]);
        assert_eq!(items, vec!["first", "second", "third", "fourth"]); // capped at 4
    }

    #[test]
    fn flush_held_releases_a_pending_partial_marker() {
        // At a text→reasoning boundary the held partial must be released so the
        // closing snapshot is faithful (and not lost / migrated to a later block).
        let mut s = FollowUpStream::new();
        let v1 = s.push("answer <foll"); // "<foll" is held back
        assert_eq!(v1, "answer ");
        let held = s.flush_held();
        assert_eq!(held, "<foll", "held partial is released");
        // Stream continues; no marker ever completes → byte-identical overall.
        let v2 = s.push("ow text");
        let fin = s.finish();
        assert_eq!(
            format!("{v1}{held}{v2}{}", fin.flush),
            "answer <follow text"
        );
        assert!(fin.items.is_empty());
    }

    #[test]
    fn empty_block_yields_no_items_and_no_visible_leak() {
        let (visible, items) = run(&["text<follow_ups></follow_ups>"]);
        assert_eq!(visible, "text");
        assert!(items.is_empty());
    }
}
