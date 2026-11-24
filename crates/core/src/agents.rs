//! Declarative Markdown agent preset discovery, model profile resolution, and workspace trust.

mod builtin;
mod discovery;
mod frontmatter;
mod model;
mod types;

#[cfg(test)]
mod tests;

pub use discovery::{MAX_AGENT_FILE_BYTES, scan_workspace_agents};
pub use model::{
    EffectiveModelConfig, ModelConfigInput, ModelProfile, ModelResolutionError,
    resolve_model_override,
};
pub use types::{
    AgentDiagnostic, AgentDiagnosticKind, AgentDiscovery, AgentModelConfig, AgentPreset,
    AgentPresetSource, AgentRegistry, WorkspaceAgentsReport, WorkspaceAgentsSnapshot,
};
