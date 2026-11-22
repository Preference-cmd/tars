//! Resolve fixed definitions into runtime resources; never persist plaintext keys.

mod provider;

pub(crate) use provider::{AssembledSession, SessionResources, assemble};

#[cfg(test)]
mod tests;
