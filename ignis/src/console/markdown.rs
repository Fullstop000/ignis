use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{
    sanitize, ACCENT, BORDER, CODE_BG, GREEN, LAVENDER, MAUVE, PEACH, SUBTEXT, TEAL, TEXT, YELLOW,
};

/// Simple inline markdown spans: **bold**, `code`, *italic*
pub(crate) fn render_md_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut buf = String::new();

    while let Some((i, c)) = chars.next() {
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
        let _ = i; // suppress unused warning
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, base_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled("", base_style));
    }
    spans
}

/// Render a full assistant text block as Lines with basic markdown awareness
pub(crate) fn render_md_block(text: &str, is_streaming: bool) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let base = Style::default().fg(TEXT);
    let mut in_code_block = false;
    let mut code_lang = String::new();

    for raw_line in text.lines() {
        // Expand tabs / strip control chars so they can't desync the layout.
        let raw_line = sanitize(raw_line);
        let raw_line = raw_line.as_str();
        if raw_line.starts_with("```") {
            if in_code_block {
                // End code block
                lines.push(Line::from(Span::styled(
                    "  └────",
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
                    Span::styled("  ┌────", Style::default().fg(BORDER)),
                    Span::styled(
                        label,
                        Style::default().fg(TEAL).add_modifier(Modifier::BOLD),
                    ),
                ]));
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(BORDER)),
                Span::styled(raw_line.to_string(), Style::default().fg(GREEN)),
            ]));
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

// ==========================================
