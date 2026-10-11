use super::file::ConfigError;
use crate::definition::{
    CredentialRef, Provider, SessionDefinition, WorkspaceTool, WorkspaceToolLimits,
};
use causa::kernel::ReasoningEffort;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Explicit entry-point choices layered over a selected model preset.
#[derive(Default)]
pub struct DefinitionOverrides {
    /// Explicit wire protocol.
    pub provider: Option<Provider>,
    /// Explicit model identifier.
    pub model: Option<String>,
    /// Explicit full endpoint URL.
    pub endpoint: Option<String>,
    /// Explicit credential environment variable name, never its value.
    pub api_key_env: Option<String>,
    /// Explicit work deadline in seconds, restricted to 1..=86400.
    pub timeout_secs: Option<u64>,
    /// Explicit retained work count.
    pub capacity: Option<usize>,
    /// Explicit model invocation limit.
    pub max_model_rounds: Option<u32>,
    /// Explicit tool declaration limit.
    pub max_tool_calls: Option<u32>,
    /// Explicit output token limit per model call.
    pub max_tokens: Option<u32>,
    /// Explicit reasoning effort.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Explicit total model calls per invocation on transient failure.
    pub max_model_attempts: Option<u32>,
    /// Explicit root resolved relative to the current directory at resolution time.
    pub workspace_root: Option<PathBuf>,
    /// Explicit selected tools; Some(empty) disables preset tools.
    pub tools: Option<Vec<WorkspaceTool>>,
    /// Explicit effective tool limits, which may only lower supported bounds.
    pub tool_limits: Option<WorkspaceToolLimits>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    provider: Provider,
    model: String,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    timeout_secs: Option<u64>,
    capacity: Option<usize>,
    max_model_rounds: Option<u32>,
    max_tool_calls: Option<u32>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>,
    max_model_attempts: Option<u32>,
    pub(super) workspace_root: Option<PathBuf>,
    tools: Option<Vec<WorkspaceTool>>,
    tool_limits: Option<ProfileToolLimits>,
}

// Sparse startup defaults are deliberately separate from the complete persisted limits.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileToolLimits {
    max_concurrent_operations: Option<usize>,
    call_timeout_secs: Option<u64>,
    max_output_bytes: Option<usize>,
    max_directory_entries: Option<usize>,
    max_file_scan_bytes: Option<usize>,
    max_search_files: Option<usize>,
    max_search_depth: Option<usize>,
    max_search_scan_bytes: Option<usize>,
    max_search_matches: Option<usize>,
}
impl ProfileToolLimits {
    fn effective(&self) -> WorkspaceToolLimits {
        let defaults = WorkspaceToolLimits::default();
        WorkspaceToolLimits {
            max_concurrent_operations: self
                .max_concurrent_operations
                .unwrap_or(defaults.max_concurrent_operations),
            call_timeout_secs: self.call_timeout_secs.unwrap_or(defaults.call_timeout_secs),
            max_output_bytes: self.max_output_bytes.unwrap_or(defaults.max_output_bytes),
            max_directory_entries: self
                .max_directory_entries
                .unwrap_or(defaults.max_directory_entries),
            max_file_scan_bytes: self
                .max_file_scan_bytes
                .unwrap_or(defaults.max_file_scan_bytes),
            max_search_files: self.max_search_files.unwrap_or(defaults.max_search_files),
            max_search_depth: self.max_search_depth.unwrap_or(defaults.max_search_depth),
            max_search_scan_bytes: self
                .max_search_scan_bytes
                .unwrap_or(defaults.max_search_scan_bytes),
            max_search_matches: self
                .max_search_matches
                .unwrap_or(defaults.max_search_matches),
        }
    }
}

pub(super) fn resolve(
    profile: Option<&Profile>,
    options: DefinitionOverrides,
) -> Result<SessionDefinition, ConfigError> {
    let provider = options
        .provider
        .or(profile.map(|p| p.provider))
        .ok_or(ConfigError::ProviderRequired)?;
    let model = options
        .model
        .or_else(|| profile.map(|p| p.model.clone()))
        .ok_or(ConfigError::ModelRequired)?;
    let timeout = options
        .timeout_secs
        .or(profile.and_then(|p| p.timeout_secs))
        .unwrap_or(120);
    if !(1..=86400).contains(&timeout) {
        return Err(ConfigError::InvalidTimeout);
    }
    let workspace_root = match options.workspace_root {
        Some(path) => Some(absolute_root(path, None)?),
        None => profile.and_then(|p| p.workspace_root.clone()),
    };
    let definition = SessionDefinition {
        provider,
        model,
        endpoint: options
            .endpoint
            .or_else(|| profile.and_then(|p| p.endpoint.clone())),
        credential: CredentialRef::Environment(
            options
                .api_key_env
                .or_else(|| profile.and_then(|p| p.api_key_env.clone()))
                .unwrap_or_else(|| match provider {
                    Provider::Anthropic => "ANTHROPIC_API_KEY".into(),
                    _ => "OPENAI_API_KEY".into(),
                }),
        ),
        work_timeout_secs: Some(timeout),
        retained_work_capacity: options
            .capacity
            .or(profile.and_then(|p| p.capacity))
            .unwrap_or(256),
        max_model_rounds: options
            .max_model_rounds
            .or(profile.and_then(|p| p.max_model_rounds))
            .unwrap_or(10),
        max_tool_calls: options
            .max_tool_calls
            .or(profile.and_then(|p| p.max_tool_calls))
            .unwrap_or(64),
        // No default: an unset limit stays unset, and the Anthropic protocol
        // then rejects the request before it is sent.
        max_tokens: options.max_tokens.or(profile.and_then(|p| p.max_tokens)),
        reasoning_effort: options
            .reasoning_effort
            .or(profile.and_then(|p| p.reasoning_effort)),
        // No default: retry cost is the user's choice, so unset means one call.
        max_model_attempts: options
            .max_model_attempts
            .or(profile.and_then(|p| p.max_model_attempts)),
        workspace_root,
        tools: options
            .tools
            .or_else(|| profile.and_then(|p| p.tools.clone()))
            .unwrap_or_default(),
        tool_limits: options
            .tool_limits
            .or_else(|| {
                profile.and_then(|p| p.tool_limits.as_ref().map(ProfileToolLimits::effective))
            })
            .unwrap_or_default(),
    };
    definition.validate().map_err(ConfigError::Definition)?;
    Ok(definition)
}

pub(super) fn absolute_root(path: PathBuf, base: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::EmptyPath);
    }
    let absolute = if path.is_absolute() {
        path
    } else {
        let base = match base {
            Some(base) => base.to_owned(),
            None => std::env::current_dir().map_err(|error| ConfigError::Io {
                path: path.clone(),
                kind: error.kind(),
            })?,
        };
        base.join(path)
    };
    let resolved = std::fs::canonicalize(&absolute)
        .map_err(|error| ConfigError::Workspace { kind: error.kind() })?;
    if !resolved.is_dir() {
        return Err(ConfigError::Workspace {
            kind: std::io::ErrorKind::NotADirectory,
        });
    }
    Ok(resolved)
}

/// Parse the shared comma-separated tool selection; an empty string disables all tools.
/// Unknown and duplicate names are rejected rather than granting extra capabilities.
pub fn parse_tools(value: &str) -> Result<Vec<WorkspaceTool>, ConfigError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut tools = Vec::new();
    for name in value.split(',').map(str::trim) {
        let tool = match name {
            "list_directory" => WorkspaceTool::ListDirectory,
            "read_file" => WorkspaceTool::ReadFile,
            "search_text" => WorkspaceTool::SearchText,
            _ => return Err(ConfigError::InvalidTools),
        };
        if tools.contains(&tool) {
            return Err(ConfigError::InvalidTools);
        }
        tools.push(tool);
    }
    Ok(tools)
}
