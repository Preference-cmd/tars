//! Persistable, credential-free execution choices and their pure validation.

mod mcp;
mod types;
mod workspace;

pub use mcp::{McpApproval, McpServerConfig, McpToolSetting};
pub use workspace::{WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};

pub(crate) use mcp::valid_server_id;
pub(crate) use types::RenderOptions;
pub use types::{CredentialRef, DefinitionError, Provider, SessionDefinition};

#[cfg(test)]
mod tests;
