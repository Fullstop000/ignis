//! Minimal syntax highlighting for the diff view, via `syntect` (pure-Rust
//! `fancy-regex`, syntaxes + themes embedded — keeps ignis a single binary).

use ratatui::style::Color;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn highlighter() -> &'static Highlighter {
    static H: OnceLock<Highlighter> = OnceLock::new();
    H.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults().themes["base16-ocean.dark"].clone();
        Highlighter { syntaxes, theme }
    })
}

/// Highlight a single line of code for the given file extension, returning
/// `(foreground, text)` spans. Unknown extensions (or any failure) fall back to
/// one uncolored span so callers can always render something.
pub(crate) fn highlight_line(line: &str, ext: &str) -> Vec<(Color, String)> {
    let h = highlighter();
    let syntax = h
        .syntaxes
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| h.syntaxes.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, &h.theme);
    match hl.highlight_line(line, &h.syntaxes) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, text)| {
                let c = style.foreground;
                (Color::Rgb(c.r, c.g, c.b), text.to_string())
            })
            .collect(),
        Err(_) => vec![(Color::Reset, line.to_string())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_keyword_distinctly() {
        let spans = highlight_line("fn main() {}", "rs");
        // Real highlighting yields multiple differently-colored spans.
        assert!(spans.len() > 1, "expected multiple colored spans");
        let colors: std::collections::HashSet<_> = spans.iter().map(|(c, _)| *c).collect();
        assert!(colors.len() > 1, "expected more than one color");
        // Round-trips the text.
        let joined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined.trim_end(), "fn main() {}");
    }

    #[test]
    fn unknown_extension_falls_back_without_panic() {
        let spans = highlight_line("some text", "no-such-ext");
        let joined: String = spans.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined.trim_end(), "some text");
    }
}
