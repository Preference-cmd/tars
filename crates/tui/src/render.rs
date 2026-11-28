//! Conversation-first terminal layout; commands and sessions appear on demand.
mod panels;
mod splash;
mod surface;
pub(crate) mod theme;

#[cfg(test)]
pub(crate) use surface::{
    activity_bar_height, footer_base_height, format_attachment_bar, format_queue_preview,
    format_workspace_label_with_home, header_height, highlight_line_matches, scroll_bar_height,
    truncate_spans_display_width,
};
pub(crate) use surface::{
    conversation_height, draw, format_queued_prompt_preview, format_workspace_label,
    transcript_width,
};
pub(crate) use theme::{ACCENT, MUTED};
