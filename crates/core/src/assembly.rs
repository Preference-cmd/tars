//! Resolve fixed definitions into runtime resources; never persist plaintext keys.

mod provider;
mod retry;

#[cfg(test)]
pub(crate) use provider::protocol_gateway;
pub(crate) use provider::{AssembledSession, SessionResources, assemble};
pub use provider::{AssemblyError, CredentialUnavailableReason};
pub(crate) use retry::StreamRetryHook;

#[cfg(test)]
mod retry_tests;
#[cfg(test)]
mod tests;
