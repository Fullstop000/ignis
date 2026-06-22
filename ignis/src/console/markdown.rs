use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    sanitize, ACCENT, BORDER, CODE_BG, GREEN, LAVENDER, MAUVE, PEACH, SUBTEXT, TEAL, TEXT, YELLOW,
};

/// Simple inline markdown spans: **bold**, `code`, *italic*
pub(crate) fn render_md_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut buf = String::new();

    while let Some((_, c)) = chars.next() {
        match c {
            '`' => {
                // Inline code
                if !buf.is_empty() {
                    spans.push(Span::styled(buf.clone(), base_style));
                    buf.clear();
                }
                let mut code = String::new();
                let mut found_end = false;
                for (_, cc) in chars.by_ref() {
                    if cc == '`' {
                        found_end = true;
                        break;
                    }
                    code.push(cc);
                }
                if found_end {
                    spans.push(Span::styled(
                        format!(" {} ", code),
                        Style::default().fg(PEACH).bg(CODE_BG),
                    ));
                } else {
                    buf.push('`');
                    buf.push_str(&code);
                }
            }
            '*' => {
                // Check for **bold**
                if chars.peek().map(|(_, c)| *c) == Some('*') {
                    chars.next(); // consume second *
                    if !buf.is_empty() {
                        spans.push(Span::styled(buf.clone(), base_style));
                        buf.clear();
                    }
                    let mut bold = String::new();
                    let mut found_end = false;
                    while let Some((_, bc)) = chars.next() {
                        if bc == '*' && chars.peek().map(|(_, c)| *c) == Some('*') {
                            chars.next();
                            found_end = true;
                            break;
                        }
                        bold.push(bc);
                    }
                    if found_end {
                        spans.push(Span::styled(bold, base_style.add_modifier(Modifier::BOLD)));
                    } else {
                        buf.push_str("**");
                        buf.push_str(&bold);
                    }
                } else {
                    // *italic* (simplified)
                    if !buf.is_empty() {
                        spans.push(Span::styled(buf.clone(), base_style));
                        buf.clear();
                    }
                    let mut italic = String::new();
                    let mut found_end = false;
                    for (_, ic) in chars.by_ref() {
                        if ic == '*' {
                            found_end = true;
                            break;
                        }
                        italic.push(ic);
                    }
                    if found_end && !italic.is_empty() {
                        spans.push(Span::styled(
                            italic,
                            base_style.add_modifier(Modifier::ITALIC),
                        ));
                    } else {
                        buf.push('*');
                        buf.push_str(&italic);
                    }
                }
            }
            _ => buf.push(c),
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, base_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled("", base_style));
    }
    spans
}

/// Render a full assistant text block as Lines with basic markdown awareness.
/// `width` is the terminal column count; it bounds table layout so the box
/// never sprawls past the screen (downstream `wrap_line` can't repair a
/// pre-rendered border without garbling it).
pub(crate) fn render_md_block(text: &str, is_streaming: bool, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let base = Style::default().fg(TEXT);
    let mut in_code_block = false;
    let mut code_lang = String::new();

    // Expand tabs / strip control chars up front so they can't desync the
    // layout, and so the table detector can look ahead one line cheaply.
    let src: Vec<String> = text.lines().map(sanitize).collect();
    let mut i = 0;
    while i < src.len() {
        let raw_line = src[i].as_str();
        if raw_line.starts_with("```") {
            if in_code_block {
                // End code block
                lines.push(Line::from(Span::styled(
                    "  ╰────",
                    Style::default().fg(BORDER),
                )));
                in_code_block = false;
                code_lang.clear();
            } else {
                // Start code block
                code_lang = raw_line.trim_start_matches('`').to_string();
                let label = if code_lang.is_empty() {
                    " code ".to_string()
                } else {
                    format!(" {} ", code_lang)
                };
                lines.push(Line::from(vec![
                    Span::styled("  ╭────", Style::default().fg(BORDER)),
                    Span::styled(
                        label,
                        Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
                    ),
                ]));
                in_code_block = true;
            }
            i += 1;
            continue;
        }

        if in_code_block {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(BORDER)),
                Span::styled(raw_line.to_string(), Style::default().fg(GREEN)),
            ]));
            i += 1;
            continue;
        }

        // Table: a row followed by a `|---|`-style separator on the next line.
        if is_table_row(raw_line) && src.get(i + 1).map(|n| is_separator_row(n)).unwrap_or(false) {
            let (table_lines, consumed) = render_table(&src[i..], width);
            lines.extend(table_lines);
            i += consumed;
            continue;
        }

        // Headers
        if let Some(h3) = raw_line.strip_prefix("### ") {
            lines.push(Line::from(Span::styled(
                format!("  {}", h3),
                Style::default().fg(MAUVE).add_modifier(Modifier::BOLD),
            )));
        } else if let Some(h2) = raw_line.strip_prefix("## ") {
            lines.push(Line::from(Span::styled(
                format!("  {}", h2),
                Style::default()
                    .fg(LAVENDER)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
        } else if let Some(h1) = raw_line.strip_prefix("# ") {
            lines.push(Line::from(Span::styled(
                format!("  {}", h1),
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
        } else if let Some(bullet) = raw_line
            .strip_prefix("- ")
            .or_else(|| raw_line.strip_prefix("* "))
        {
            // Bullet points
            let mut spans = vec![Span::styled("  • ", Style::default().fg(ACCENT))];
            spans.extend(render_md_spans(bullet, base));
            lines.push(Line::from(spans));
        } else if let Some(quote) = raw_line.strip_prefix("> ") {
            // Blockquote
            lines.push(Line::from(vec![
                Span::styled("  ▍ ", Style::default().fg(YELLOW)),
                Span::styled(quote.to_string(), Style::default().fg(SUBTEXT)),
            ]));
        } else if raw_line.trim().is_empty() {
            lines.push(Line::from(""));
        } else {
            let mut spans = vec![Span::styled("  ", base)];
            spans.extend(render_md_spans(raw_line, base));
            lines.push(Line::from(spans));
        }
        i += 1;
    }

    // Streaming cursor
    if is_streaming {
        if let Some(last) = lines.last_mut() {
            last.spans
                .push(Span::styled("▌", Style::default().fg(ACCENT)));
        }
    }

    lines
}

// ---- Markdown tables -----------------------------------------------------

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

/// A line that could be a table row: non-empty and contains a pipe.
pub(crate) fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.contains('|')
}

/// A GitHub-style separator/alignment row: every cell is `:?-+:?`.
pub(crate) fn is_separator_row(line: &str) -> bool {
    let cells = split_cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim();
            let body = c.strip_prefix(':').unwrap_or(c);
            let body = body.strip_suffix(':').unwrap_or(body);
            !body.is_empty() && body.bytes().all(|b| b == b'-')
        })
}

/// Split a `| a | b |` row into trimmed cells, dropping the optional outer pipes.
fn split_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn alignment_of(sep_cell: &str) -> Align {
    let c = sep_cell.trim();
    match (c.starts_with(':'), c.ends_with(':')) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

fn cell_at(row: &[String], c: usize) -> &str {
    row.get(c).map(|s| s.as_str()).unwrap_or("")
}

/// Pad `cell` to `width` display columns under `align` (display-width aware, so
/// CJK / wide glyphs line up — see [[insert-before-wide-char-spaces]]).
fn pad_cell(cell: &str, width: usize, align: Align) -> String {
    let pad = width.saturating_sub(UnicodeWidthStr::width(cell));
    match align {
        Align::Left => format!("{cell}{}", " ".repeat(pad)),
        Align::Right => format!("{}{cell}", " ".repeat(pad)),
        Align::Center => {
            let lp = pad / 2;
            format!("{}{cell}{}", " ".repeat(lp), " ".repeat(pad - lp))
        }
    }
}

/// Shrink natural column widths so the whole box fits `width` columns, or return
/// `None` when no box can: the overhead is `3 + 3*ncols` (the leading `  │` plus
/// ` … │` per column), leaving a content budget of `width - overhead`. A cell can
/// hold a double-width glyph (CJK/emoji) that can't be split, so a content column
/// needs at least 2 columns; if the budget can't give every column that
/// (`budget < 2*ncols`), a box would overflow and garble — the caller falls back
/// to plain rows. Otherwise: if the natural widths already fit they're returned
/// as-is, else we water-fill narrowest-first — each column takes the smaller of
/// its natural width and an even share of the remaining budget, so slim columns
/// keep their size and the surplus flows to wide ones (which then wrap, see
/// [[wrap_cell]]). `budget >= 2*ncols` keeps every per-step share >= 2, so no
/// content column is squeezed below the width of a single wide glyph.
fn fit_column_widths(natural: &[usize], width: u16) -> Option<Vec<usize>> {
    let ncols = natural.len();
    if ncols == 0 {
        return Some(Vec::new());
    }
    let budget = (width as usize).saturating_sub(3 + 3 * ncols);
    if budget < 2 * ncols {
        return None;
    }
    if natural.iter().sum::<usize>() <= budget {
        return Some(natural.to_vec());
    }
    let mut order: Vec<usize> = (0..ncols).collect();
    order.sort_by_key(|&c| natural[c]);
    let mut widths = vec![0usize; ncols];
    let mut remaining = budget;
    let mut left = ncols;
    for &c in &order {
        let share = remaining / left; // >= 2 here, since remaining >= 2*left throughout
        let w = natural[c].min(share);
        widths[c] = w;
        remaining -= w;
        left -= 1;
    }
    Some(widths)
}

/// Word-wrap a single cell to `width` display columns, breaking over-long runs
/// (a path, a `snake_case` token) at the column boundary. Returns one entry per
/// visual sub-row; an empty cell yields a single empty row so the column keeps
/// its slot in the grid.
fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in cell.split(' ') {
        let ww = UnicodeWidthStr::width(word);
        if ww > width {
            // Longer than a whole row: flush, then hard-break by columns.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + cw > width && !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += cw;
            }
        } else {
            let sep = usize::from(!cur.is_empty());
            if cur_w + sep + ww > width && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += ww;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Render a markdown table given a slice whose first line is the header and
/// second is the separator; consumes body rows until a non-row line. Returns
/// the rendered lines and how many source lines were consumed.
fn render_table(block: &[String], width: u16) -> (Vec<Line<'static>>, usize) {
    let header = split_cells(&block[0]);
    let aligns_src = split_cells(&block[1]);
    let mut body: Vec<Vec<String>> = Vec::new();
    let mut consumed = 2;
    for line in &block[2..] {
        if !is_table_row(line) {
            break;
        }
        body.push(split_cells(line));
        consumed += 1;
    }

    let ncols = header
        .len()
        .max(aligns_src.len())
        .max(body.iter().map(|r| r.len()).max().unwrap_or(0));
    let aligns: Vec<Align> = (0..ncols)
        .map(|c| {
            aligns_src
                .get(c)
                .map(|s| alignment_of(s))
                .unwrap_or(Align::Left)
        })
        .collect();
    let mut natural = vec![0usize; ncols];
    for (c, w) in natural.iter_mut().enumerate() {
        *w = UnicodeWidthStr::width(cell_at(&header, c));
        for row in &body {
            *w = (*w).max(UnicodeWidthStr::width(cell_at(row, c)));
        }
    }
    // Bound the box to the terminal; over-wide columns wrap instead of sprawling.
    // When too many columns leave no room for a box, fall back to plain rows so
    // we never emit an over-wide border for the terminal to garble.
    let Some(widths) = fit_column_widths(&natural, width) else {
        let plain = |cells: &[String]| {
            Line::from(Span::styled(
                format!("  {}", cells.join(" | ")),
                Style::default().fg(TEXT),
            ))
        };
        let mut out = vec![plain(&header)];
        out.extend(body.iter().map(|r| plain(r)));
        return (out, consumed);
    };

    let border = Style::default().fg(BORDER);
    let head_style = Style::default().fg(TEAL).add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(TEXT);

    let rule = |left: char, mid: char, right: char| -> Line<'static> {
        let mut s = format!("  {left}");
        for (c, w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push(if c + 1 == ncols { right } else { mid });
        }
        Line::from(Span::styled(s, border))
    };
    // A logical row is as tall as its tallest wrapped cell; each visual sub-row
    // redraws the vertical borders so the grid stays closed.
    let data_row = |cells: &[String], style: Style| -> Vec<Line<'static>> {
        let wrapped: Vec<Vec<String>> = (0..ncols)
            .map(|c| wrap_cell(cell_at(cells, c), widths[c]))
            .collect();
        let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);
        (0..height)
            .map(|r| {
                let mut spans: Vec<Span<'static>> = vec![Span::styled("  │", border)];
                for (c, w) in widths.iter().enumerate() {
                    let sub = wrapped[c].get(r).map(String::as_str).unwrap_or("");
                    let padded = pad_cell(sub, *w, aligns[c]);
                    spans.push(Span::styled(format!(" {padded} "), style));
                    spans.push(Span::styled("│", border));
                }
                Line::from(spans)
            })
            .collect()
    };

    let mut out = vec![rule('┌', '┬', '┐')];
    out.extend(data_row(&header, head_style));
    out.push(rule('├', '┼', '┤'));
    for row in &body {
        out.extend(data_row(row, cell_style));
    }
    out.push(rule('└', '┴', '┘'));
    (out, consumed)
}

#[cfg(test)]
mod table_tests {
    use super::*;

    /// Flatten each rendered Line to its concatenated span text — enough to
    /// assert structure and alignment (colors are a visual/dogfood concern).
    fn flat(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_full_grid_box_matching_spec() {
        let md = "\
| Name | Age | City |
|------|-----|------|
| Alice | 30 | Beijing |
| Bob | 25 | Shanghai |";
        let out = flat(&render_md_block(md, false, 80));
        assert_eq!(
            out,
            vec![
                "  ┌───────┬─────┬──────────┐",
                "  │ Name  │ Age │ City     │",
                "  ├───────┼─────┼──────────┤",
                "  │ Alice │ 30  │ Beijing  │",
                "  │ Bob   │ 25  │ Shanghai │",
                "  └───────┴─────┴──────────┘",
            ]
        );
    }

    #[test]
    fn right_align_marker_right_pads_left() {
        let md = "\
| Item | Qty |
|------|----:|
| Pen | 3 |
| Notebook | 12 |";
        let out = flat(&render_md_block(md, false, 80));
        // Qty column width 3 (header "Qty"); values right-aligned.
        assert!(out.iter().any(|l| l.contains("│   3 │")), "rows: {out:?}");
        assert!(out.iter().any(|l| l.contains("│  12 │")), "rows: {out:?}");
    }

    #[test]
    fn center_align_marker_centers() {
        let md = "\
| K | V |
|:-:|---|
| ab | x |
| c | y |";
        let out = flat(&render_md_block(md, false, 80));
        // K column width 2 ("ab"); "c" centered → " c". Cell is ` ` + content + ` `.
        assert!(
            out.iter().any(|l| l.starts_with("  │ c  │")),
            "rows: {out:?}"
        );
    }

    #[test]
    fn cjk_columns_use_display_width() {
        // "中文" has display width 4; the column border segment must be 4+2=6.
        let md = "\
| x |
|---|
| 中文 |";
        let out = flat(&render_md_block(md, false, 80));
        assert_eq!(out[0], "  ┌──────┐", "top border: {out:?}");
        assert_eq!(out[3], "  │ 中文 │", "cjk row: {out:?}");
    }

    #[test]
    fn pipe_block_without_separator_falls_back_to_plaintext() {
        let md = "| a | b |\n| c | d |";
        let out = flat(&render_md_block(md, false, 80));
        assert!(
            out.iter().all(|l| !l.contains('┌') && !l.contains('│')),
            "should not render a box without a separator row: {out:?}"
        );
        // Rendered as ordinary text lines (indented).
        assert!(out.iter().any(|l| l.contains("| a | b |")), "out: {out:?}");
    }

    #[test]
    fn table_surrounded_by_prose_renders_both() {
        let md = "before\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\nafter";
        let out = flat(&render_md_block(md, false, 80));
        assert!(out.iter().any(|l| l.contains("before")));
        assert!(out.iter().any(|l| l.starts_with("  ┌")));
        assert!(out.iter().any(|l| l.contains("after")));
    }

    /// Display width of a flattened row.
    fn w(s: &str) -> usize {
        UnicodeWidthStr::width(s)
    }

    #[test]
    fn wide_table_fits_terminal_width() {
        // A table whose natural content width far exceeds the terminal. Cells
        // must wrap inside their columns so the box fits `width`; otherwise the
        // pre-rendered border sprawls past the screen and downstream wrap_line
        // hard-breaks it into a garbled mess (the reported bug).
        let md = "\
| # | Layer | Detail |
|---|-------|--------|
| 1 | Env allowlist | UNIVERSAL_ENV_ALLOWLIST equals PATH HOME USER LANG, then Command::env_clear() plus explicit per-name cmd.env so the hook sees only universal plus declared names |
| 2 | Filesystem sandbox | Linux Landlock ABI V2 and macOS sandbox_init Seatbelt, per-hook sandbox bool default true reading the hook folder and system cert and lib paths |";
        let width = 60u16;
        let out = flat(&render_md_block(md, false, width));

        for line in &out {
            assert!(
                w(line) <= width as usize,
                "row exceeds width {width} (w={}): {line:?}",
                w(line)
            );
        }
        // The box stays intact: one top border and one bottom border, each
        // spanning corner-to-corner (not split across wrapped rows).
        assert!(
            out.first()
                .is_some_and(|l| l.starts_with("  ┌") && l.ends_with('┐')),
            "top border: {:?}",
            out.first()
        );
        assert!(
            out.last()
                .is_some_and(|l| l.starts_with("  └") && l.ends_with('┘')),
            "bottom border: {:?}",
            out.last()
        );
        // Content is wrapped, not truncated away — a distinctive token from the
        // long cell still appears somewhere in the rendered table.
        assert!(
            out.iter().any(|l| l.contains("env_clear")),
            "long-cell content was lost: {out:?}"
        );
    }

    #[test]
    fn table_refits_when_width_changes() {
        // Resize: the live band re-renders each block with the current terminal
        // width every frame, so the same table must lay out cleanly at any
        // width. (Already-committed scrollback is frozen by the terminal — that
        // is a property of inline rendering, not of table layout.)
        let md = "\
| Key | Value |
|-----|-------|
| sandbox | Linux Landlock ABI V2 and macOS sandbox_init Seatbelt, default true |
| grace | SIGTERM then a one second grace window then SIGKILL via libc::kill |";
        for width in [30u16, 50, 80, 120] {
            let out = flat(&render_md_block(md, false, width));
            for line in &out {
                assert!(
                    w(line) <= width as usize,
                    "width {width}: row exceeds it (w={}): {line:?}",
                    w(line)
                );
            }
            assert!(
                out.first()
                    .is_some_and(|l| l.starts_with("  ┌") && l.ends_with('┐')),
                "width {width}: broken top border {:?}",
                out.first()
            );
        }
        // Narrowing wraps more, so it produces at least as many rows as widening.
        let narrow = flat(&render_md_block(md, false, 30));
        let wide = flat(&render_md_block(md, false, 120));
        assert!(
            narrow.len() >= wide.len(),
            "narrower terminal should wrap into more rows: {} vs {}",
            narrow.len(),
            wide.len()
        );
    }
}
