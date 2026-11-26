//! Conversation-first terminal layout; commands and sessions appear on demand.
mod panels;
mod splash;
mod surface;

pub(crate) use surface::{
    ACCENT, MUTED, conversation_height, draw, format_queue_preview, transcript_width,
};
