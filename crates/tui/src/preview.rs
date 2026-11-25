//! Bounded temporary model text, independently indexed from committed history.
mod buffer;

pub(crate) use buffer::{Preview, PreviewLineKind};
pub(crate) const MAX_PREVIEW_BYTES: usize = 256 * 1024;
pub(crate) const MAX_REASONING_BYTES: usize = 64 * 1024;
