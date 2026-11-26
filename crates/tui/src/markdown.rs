//! Rich-text GFM Markdown rendering and pure-Rust lexical syntax highlighting
//! for `tars-tui`.
//!
//! All untrusted text (prose, code blocks, fence language tags, link URLs,
//! image metadata, and table cells) is sanitized before entering Ratatui
//! [`Line`](ratatui::text::Line) / [`Span`](ratatui::text::Span) output so raw
//! terminal escape sequences, control characters, BiDi overrides, and Unicode
//! line/paragraph separators never reach the terminal.

mod highlight;
mod render;
#[cfg(test)]
mod tests;

#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use highlight::{MAX_HIGHLIGHT_BYTES, highlight_code, highlight_diff_line};
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use render::{render_markdown, wrap_styled_line};
