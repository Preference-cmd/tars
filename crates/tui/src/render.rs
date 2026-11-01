//! Conversation-first terminal layout; commands and sessions appear on demand.
mod panels;
mod splash;
mod surface;

pub(crate) use surface::{conversation_height, draw, transcript_width};
