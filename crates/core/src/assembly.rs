//! Resolve fixed definitions into runtime resources; never persist plaintext keys.

mod provider;

pub use provider::AssemblyError;
pub(crate) use provider::{AssembledSession, SessionResources, assemble};

#[cfg(test)]
mod tests;
