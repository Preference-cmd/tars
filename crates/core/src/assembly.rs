//! Resolve fixed definitions into runtime resources; never persist plaintext keys.

mod provider;
mod retry;

pub use provider::AssemblyError;
pub(crate) use provider::{AssembledSession, SessionResources, assemble};
pub(crate) use retry::StreamRetryHook;

#[cfg(test)]
mod retry_tests;
#[cfg(test)]
mod tests;
