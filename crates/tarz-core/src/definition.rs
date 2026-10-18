//! Persistable, credential-free execution choices and their pure validation.

mod types;
mod workspace;

pub use workspace::{WorkspaceTool, WorkspaceToolLimits};

pub(crate) use types::RenderOptions;
pub use types::{CredentialRef, DefinitionError, Provider, SessionDefinition};

#[cfg(test)]
mod tests;
