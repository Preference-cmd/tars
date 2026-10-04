//! Bounded temporary model text, independently indexed from committed history.
mod buffer;

pub(crate) use buffer::Preview;
pub(crate) const MAX_PREVIEW_BYTES: usize = 256 * 1024;
