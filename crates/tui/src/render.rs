//! Conversation-first terminal layout; commands and sessions appear on demand.
mod panels;
mod splash;
mod surface;

pub(crate) use surface::{
    ACCENT, MUTED, conversation_height, draw, format_queued_prompt_preview, transcript_width,
};
#[cfg(test)]
pub(crate) use surface::{format_attachment_bar, format_queue_preview};
