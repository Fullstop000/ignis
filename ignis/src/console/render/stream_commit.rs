//! Streaming row committer for inline rendering.
//!
//! As the in-progress assistant/reasoning block streams in, we want its
//! *finalized* visual rows to flow into the terminal's native scrollback (via
//! `insert_before`) while the unstable tail stays live in the band. The only
//! thing that makes this non-trivial is markdown retroactivity: a row already
//! pushed to scrollback can never be rewritten, so we must commit only text
//! whose rendering can no longer change as more text arrives.
//!
//! `render_md_block` (see `markdown.rs`) makes this tractable — the *only*
//! retroactive construct is a GFM table (its header flips to a box once the
//! `|---|` separator arrives, and column widths re-align as body rows stream).
//! Code fences are append-only (committed `│ code` rows never change; only a
//! closing `└────` is appended), and setext headings aren't supported. So the
//! rule is: commit every completed line, *except* a trailing run of table-row
//! lines outside a code fence (held until a non-table line proves the table is
//! done) and the incomplete final line (no newline yet).

use std::path::Path;

use ratatui::text::Line;

use crate::console::app::UIBlock;
use crate::console::markdown::is_table_row;
use crate::console::render::blocks::block_lines;

/// The prefix of `text` that is safe to commit to scrollback now: it renders
/// identically no matter what text arrives later.
///
/// Two things are held back: the incomplete final line (no newline yet), and a
/// trailing run of table-row lines that sit *outside* any code fence — those
/// might still become a table or have their columns re-aligned by rows yet to
/// arrive. Over-holding (a prose line that merely contains `|`) only delays a
/// commit by a line; under-committing a real table row would corrupt
/// scrollback, so we bias to holding.
pub(crate) fn stable_prefix(text: &str) -> &str {
    // Only complete (newline-terminated) lines are commit candidates.
    let complete_end = match text.rfind('\n') {
        Some(i) => i + 1,
        None => return "",
    };

    // Record, per complete line, its start offset and whether it's a held
    // table row (outside a fence). Code-fence delimiters toggle fence state;
    // content inside a fence is append-only and never held.
    let mut in_fence = false;
    let mut start = 0usize;
    let mut lines: Vec<(usize, bool)> = Vec::new();
    for line in text[..complete_end].split_inclusive('\n') {
        // Match render_md_block's detection exactly (it checks the line head
        // with no trim): an *indented* ``` is not a fence delimiter there, so
        // we must not treat it as one either, or committed rows could diverge.
        let is_fence_delim = line.starts_with("```");
        let held = !in_fence && !is_fence_delim && is_table_row(line);
        lines.push((start, held));
        if is_fence_delim {
            in_fence = !in_fence;
        }
        start += line.len();
    }

    // Cut at the start of the maximal trailing run of held lines.
    let mut cut = complete_end;
    for &(line_start, held) in lines.iter().rev() {
        if !held {
            break;
        }
        cut = line_start;
    }
    &text[..cut]
}

/// Visual rows of `block` that are safe to push to scrollback now — its stable
/// text prefix, rendered exactly as the final block will render it. Returns an
/// empty Vec for block kinds that aren't streamed incrementally (tool calls,
/// user prompts): the caller commits those whole when they finalize.
pub(crate) fn stable_rows(
    block: &UIBlock,
    tick: u64,
    cwd: &Path,
    width: u16,
) -> Vec<Line<'static>> {
    let stable = match block {
        UIBlock::Assistant(t) => UIBlock::Assistant(stable_prefix(t).to_string()),
        UIBlock::Reasoning(t) => UIBlock::Reasoning(stable_prefix(t).to_string()),
        _ => return Vec::new(),
    };
    block_lines(&stable, tick, cwd, width)
}

/// Whether `block` is streamed row-by-row (assistant / reasoning text). Tool
/// and user blocks are committed whole on finalize.
pub(crate) fn is_streamed(block: &UIBlock) -> bool {
    matches!(block, UIBlock::Assistant(_) | UIBlock::Reasoning(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::app::UIBlock;
    use crate::console::render::blocks::block_lines;
    use ratatui::text::Line;
    use std::path::Path;

    /// Flatten rendered lines to their concatenated span text — enough to
    /// assert "a committed row never changes" (style is a visual concern).
    fn flat(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn rows(text: &str, width: u16) -> Vec<String> {
        flat(&block_lines(
            &UIBlock::Assistant(text.to_string()),
            0,
            Path::new("/"),
            width,
        ))
    }

    #[test]
    fn incomplete_trailing_line_is_held() {
        // "world" has no newline yet — its row is still being formed.
        assert_eq!(stable_prefix("hello\nworld"), "hello\n");
    }

    #[test]
    fn trailing_table_rows_are_held() {
        // A completed table-row line still renders as plain text until a
        // separator follows, and its columns re-align as body rows arrive.
        // Hold the whole trailing table run; "intro" before it is stable.
        assert_eq!(stable_prefix("intro\n| a | b |\n|---|---|\n"), "intro\n");
    }

    #[test]
    fn table_released_once_a_non_table_line_follows() {
        // Once a non-table line ends the table, the whole table is final.
        let t = "| a | b |\n|---|---|\n| 1 | 2 |\nafter\n";
        assert_eq!(stable_prefix(t), t);
    }

    #[test]
    fn pipe_lines_inside_a_code_fence_are_not_held() {
        // `| x |` inside a fence is code (append-only), not a table row.
        let t = "```\n| x |\n";
        assert_eq!(stable_prefix(t), t);
    }

    #[test]
    fn indented_backticks_are_not_treated_as_a_fence() {
        // render_md_block only treats a line-leading ``` as a fence, so an
        // indented ``` must NOT open one here — otherwise a following table
        // would be committed as "code" while the renderer boxes it (divergence).
        let t = "  ```\n| a | b |\n|---|---|\n";
        assert_eq!(stable_prefix(t), "  ```\n");
    }

    #[test]
    fn nothing_stable_until_first_newline() {
        assert_eq!(stable_prefix("partial line so far"), "");
    }

    /// The core guarantee: for EVERY streamed prefix, the rows we would commit
    /// are an exact prefix of the final one-shot render — so a row pushed to
    /// scrollback can never need to change. Streams byte-by-byte through a doc
    /// exercising headings, lists, an open/closed code fence, and a table.
    #[test]
    fn streamed_commits_are_always_a_prefix_of_the_final_render() {
        let full = "# Title\nintro paragraph\n\n- one\n- two\n\n```rust\nlet x = 1;\nlet y = 2;\n```\n\n| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |\n\ndone.\n";
        for width in [40u16, 80] {
            let final_rows = rows(full, width);
            let mut prev_len = 0usize;
            for end in 0..=full.len() {
                if !full.is_char_boundary(end) {
                    continue;
                }
                let committed = rows(stable_prefix(&full[..end]), width);
                assert!(
                    final_rows.starts_with(&committed),
                    "committed rows diverged from final at byte {end} (w={width}):\n  committed={committed:?}\n  final={final_rows:?}"
                );
                assert!(
                    committed.len() >= prev_len,
                    "committed rows shrank at byte {end} (w={width})"
                );
                prev_len = committed.len();
            }
        }
    }
}
