//! Persistable, credential-free execution choices and their pure validation.

mod types;

pub use types::{CredentialRef, DefinitionError, Provider, SessionDefinition};

#[cfg(test)]
mod tests;
