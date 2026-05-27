//! Color palette + animation tickers used across the console renderer.
//! Names mirror the Catppuccin Mocha base palette so anyone reading the
//! ratatui styling code recognizes them.
use ratatui::style::Color;

pub(crate) const BG: Color = Color::Rgb(17, 17, 27);
pub(crate) const SURFACE: Color = Color::Rgb(24, 24, 37);
pub(crate) const SURFACE_2: Color = Color::Rgb(30, 30, 46);
pub(crate) const BORDER: Color = Color::Rgb(49, 50, 68);
pub(crate) const BORDER_ACTIVE: Color = Color::Rgb(137, 180, 250);
pub(crate) const TEXT: Color = Color::Rgb(205, 214, 244);
pub(crate) const TEXT_DIM: Color = Color::Rgb(108, 112, 134);
pub(crate) const SUBTEXT: Color = Color::Rgb(147, 153, 178);
pub(crate) const ACCENT: Color = Color::Rgb(137, 180, 250); // blue
pub(crate) const LAVENDER: Color = Color::Rgb(180, 190, 254);
pub(crate) const GREEN: Color = Color::Rgb(166, 227, 161);
pub(crate) const RED: Color = Color::Rgb(243, 139, 168);
pub(crate) const YELLOW: Color = Color::Rgb(249, 226, 175);
pub(crate) const PEACH: Color = Color::Rgb(250, 179, 135);
pub(crate) const TEAL: Color = Color::Rgb(148, 226, 213);
pub(crate) const MAUVE: Color = Color::Rgb(203, 166, 247);
pub(crate) const CODE_BG: Color = Color::Rgb(30, 30, 46);
// Solid diff backgrounds (added / removed lines), dark tints of green / red.
pub(crate) const DIFF_ADD_BG: Color = Color::Rgb(25, 46, 36);
pub(crate) const DIFF_DEL_BG: Color = Color::Rgb(51, 29, 37);

pub(crate) const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Playful status verbs cycled while the model is generating (Claude Code
/// style), for a livelier "Thinking" indicator. First entry is the default at t=0.
pub(crate) const THINKING_VERBS: &[&str] = &[
    "Thinking",
    "Pondering",
    "Noodling",
    "Cogitating",
    "Ruminating",
    "Marinating",
    "Percolating",
    "Nebulizing",
    "Conjuring",
    "Brewing",
    "Simmering",
    "Tinkering",
    "Scheming",
    "Synthesizing",
    "Incubating",
    "Galaxy-braining",
];
