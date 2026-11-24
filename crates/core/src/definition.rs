//! Persistable, credential-free execution choices and their pure validation.

mod code_execution;
mod command;
mod mcp;
mod narrowing;
mod types;
mod workspace;

pub use code_execution::CodeExecutionSettings;
pub use command::{CommandNetwork, CommandSettings};
pub use mcp::{McpApproval, McpServerConfig, McpToolSetting};
pub use narrowing::{
    ChildModelOverride, ChildNarrowingRequest, NarrowingError, check_permission_mode_narrowing,
    narrow_child_definition, permission_mode_rank,
};
pub use workspace::{BuiltinTool, WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};

#[cfg(test)]
pub(crate) use mcp::expand_template;
pub(crate) use mcp::{
    expand_template_recording, is_transport_header, redact_substitutions, valid_header_name,
    valid_header_value_template, valid_server_id, valid_url_template, validate_template,
};
pub(crate) use types::RenderOptions;
pub use types::{
    CredentialRef, DefinitionError, MAX_COMPACT_THRESHOLD, MIN_COMPACT_THRESHOLD, Provider,
    SessionDefinition, Temperature,
};
pub(crate) use types::{valid_environment_name, valid_http_endpoint};

#[cfg(test)]
mod tests;
