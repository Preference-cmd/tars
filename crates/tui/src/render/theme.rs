//! Centralized Grok Build minimalist color palette and geometric glyph tokens.

use ratatui::style::Color;

/// Primary UI accent color (dark- and light-terminal friendly).
pub(crate) const ACCENT: Color = Color::Cyan;
/// Muted secondary metadata and rail color.
pub(crate) const MUTED: Color = Color::DarkGray;
/// Success / addition color.
pub(crate) const OK: Color = Color::Green;
/// Error / denied / deletion color.
pub(crate) const ERR: Color = Color::Red;
/// Warning / unknown-outcome / unpaired color.
pub(crate) const WARN: Color = Color::Yellow;

/// Geometric glyph for thinking / reasoning state (used in activity bar and preview).
pub(crate) const THINKING_GLYPH: &str = "◇";
/// Geometric glyph for succeeded tool cards in the committed transcript.
pub(crate) const TOOL_OK_GLYPH: &str = "▸";
/// Geometric glyph for succeeded live tools in [`crate::app::tool_progress::ToolView`].
pub(crate) const TOOL_DONE_GLYPH: &str = "✓";
/// Geometric glyph for failed or denied tools.
pub(crate) const TOOL_ERR_GLYPH: &str = "✗";
/// Geometric glyph for unknown-outcome or not-executed tools.
pub(crate) const TOOL_WARN_GLYPH: &str = "?";
/// Geometric glyph for unpaired tool declarations awaiting result or repair.
pub(crate) const TOOL_UNPAIRED_GLYPH: &str = "○";
/// Left-rail prefix for indented tool details and inline diffs.
pub(crate) const RAIL_PREFIX: &str = "  │ ";
/// Braille spinner frames for active live tools.
pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
