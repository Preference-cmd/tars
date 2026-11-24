//! Monotonic capability and permission narrowing (`child <= parent`) for child agents.

use super::{
    CodeExecutionSettings, CommandNetwork, CommandSettings, CredentialRef, DefinitionError,
    Provider, SessionDefinition, Temperature, WorkspaceTool, WorkspaceToolLimits, WriteToolLimits,
};
use crate::policy::PermissionMode;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Optional model and generation parameter overrides for a child agent.
///
/// Applying a [`ChildModelOverride`] never widens any tool, workspace, resource-limit,
/// or permission-mode boundary on the resulting [`SessionDefinition`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChildModelOverride {
    /// Override wire protocol provider.
    pub provider: Option<Provider>,
    /// Override model identifier.
    pub model: Option<String>,
    /// Override endpoint (`Some(None)` clears the parent endpoint; `None` inherits parent).
    pub endpoint: Option<Option<String>>,
    /// Override credential environment reference.
    pub credential: Option<CredentialRef>,
    /// Override generated token cap (`Some(None)` clears parent cap; `None` inherits parent).
    pub max_tokens: Option<Option<u32>>,
    /// Override reasoning effort (`Some(None)` clears parent setting; `None` inherits parent).
    pub reasoning_effort: Option<Option<String>>,
    /// Override sampling temperature (`Some(None)` clears parent setting; `None` inherits parent).
    pub temperature: Option<Option<Temperature>>,
    /// Override extra top-level JSON request fields.
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Override context window (`Some(None)` clears parent setting; `None` inherits parent).
    pub context_window: Option<Option<u64>>,
    /// Override retry attempt count (`Some(None)` clears parent setting; `None` inherits parent).
    pub max_model_attempts: Option<Option<u32>>,
    /// Override protocol render options.
    pub render_options: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Requested child capability bounds validated monotonically against a parent definition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChildNarrowingRequest {
    /// Optional model/generation override; never alters tool or permission capabilities.
    pub model_override: Option<ChildModelOverride>,
    /// Optional workspace root (`None` inherits parent; `Some(None)` disables workspace & tools).
    pub workspace_root: Option<Option<PathBuf>>,
    /// Optional subset of `parent.tools` (`None` inherits parent).
    pub tools: Option<Vec<WorkspaceTool>>,
    /// Optional tightened workspace tool limits (`None` inherits parent).
    pub tool_limits: Option<WorkspaceToolLimits>,
    /// Optional tightened write tool limits (`None` inherits parent; `Some(None)` clears).
    pub write_limits: Option<Option<WriteToolLimits>>,
    /// Optional tightened command settings (`None` inherits parent; `Some(None)` clears).
    pub command: Option<Option<CommandSettings>>,
    /// Optional tightened code-execution settings (`None` inherits parent; `Some(None)` clears).
    pub code_execution: Option<Option<CodeExecutionSettings>>,
    /// Optional subset of `parent.mcp_servers` identifiers (`None` inherits parent).
    pub mcp_server_ids: Option<Vec<String>>,
    /// Optional tightened model round ceiling (`<= parent.max_model_rounds`).
    pub max_model_rounds: Option<u32>,
    /// Optional tightened tool call ceiling (`<= parent.max_tool_calls`).
    pub max_tool_calls: Option<u32>,
    /// Optional tightened operation timeout (`<= parent.work_timeout_secs` when parent sets one).
    pub work_timeout_secs: Option<u64>,
    /// Optional tightened permission mode (`child <= parent`).
    pub permission_mode: Option<PermissionMode>,
}

/// Error returned when a child agent request attempts to escalate or violate definition bounds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NarrowingError {
    /// Child requested a workspace root when parent had none, used `..`, or escaped parent root.
    #[error("child workspace_root cannot exceed or escape parent workspace_root")]
    WorkspaceEscalation,
    /// Child requested enabled workspace tools while disabling `workspace_root`.
    #[error("workspace_root cannot be disabled while workspace tools remain enabled")]
    WorkspaceRequiredForTools,
    /// Child tool list contained duplicates.
    #[error("child tool selection contains a duplicate")]
    DuplicateTool,
    /// Child requested a workspace tool not present in the parent definition.
    #[error("child cannot enable workspace tool '{}' absent from parent", .0.name())]
    ToolEscalation(WorkspaceTool),
    /// Child requested workspace tool limits exceeding the parent bounds.
    #[error("child tool_limits cannot exceed parent bounds")]
    ToolLimitsEscalation,
    /// Child requested write tool limits when parent had none or exceeding parent bounds.
    #[error("child write_limits cannot exceed parent bounds")]
    WriteLimitsEscalation,
    /// Child requested command settings without parent `run_command`/settings or with wider network/timeouts.
    #[error("child command settings cannot exceed parent bounds")]
    CommandSettingsEscalation,
    /// Child requested code-execution settings when parent had none or exceeding parent bounds.
    #[error("child code_execution settings cannot exceed parent bounds")]
    CodeExecutionEscalation,
    /// Child requested duplicate MCP server identifiers.
    #[error("child mcp_server_ids contains duplicate server '{0}'")]
    DuplicateMcpServer(String),
    /// Child requested an MCP server identifier not present in the parent definition.
    #[error("child cannot enable MCP server '{0}' absent from parent")]
    UnknownMcpServer(String),
    /// Child requested more model rounds than the parent permits.
    #[error("child max_model_rounds ({child}) cannot exceed parent ({parent})")]
    RoundLimitEscalation {
        /// Parent ceiling.
        parent: u32,
        /// Requested child ceiling.
        child: u32,
    },
    /// Child requested more tool calls than the parent permits.
    #[error("child max_tool_calls ({child}) cannot exceed parent ({parent})")]
    ToolCallLimitEscalation {
        /// Parent ceiling.
        parent: u32,
        /// Requested child ceiling.
        child: u32,
    },
    /// Child requested a longer work timeout than the parent permits.
    #[error("child work_timeout_secs ({child}) cannot exceed parent ({parent})")]
    TimeoutEscalation {
        /// Parent timeout in seconds.
        parent: u64,
        /// Requested child timeout in seconds.
        child: u64,
    },
    /// Child requested a permission mode less strict than the parent's active mode.
    #[error("child permission mode '{child}' cannot exceed parent '{parent}'")]
    PermissionModeEscalation {
        /// Parent's active permission mode.
        parent: PermissionMode,
        /// Requested child permission mode.
        child: PermissionMode,
    },
    /// Resulting child definition failed [`SessionDefinition::validate`].
    #[error("narrowed child definition is invalid: {0}")]
    InvalidChildDefinition(#[from] DefinitionError),
}

/// Monotonic strictness rank for [`PermissionMode`] (`ReadOnly < Ask < AcceptEdits < FullAccess`).
pub const fn permission_mode_rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::ReadOnly => 0,
        PermissionMode::Ask => 1,
        PermissionMode::AcceptEdits => 2,
        PermissionMode::FullAccess => 3,
    }
}

/// Verify that `child` is at least as strict as `parent_active`.
pub fn check_permission_mode_narrowing(
    parent_active: PermissionMode,
    child: PermissionMode,
) -> Result<(), NarrowingError> {
    if permission_mode_rank(child) > permission_mode_rank(parent_active) {
        return Err(NarrowingError::PermissionModeEscalation {
            parent: parent_active,
            child,
        });
    }
    Ok(())
}

fn write_limits_lte(child: WriteToolLimits, parent: WriteToolLimits) -> bool {
    child.max_edit_file_bytes <= parent.max_edit_file_bytes
        && child.max_write_bytes <= parent.max_write_bytes
        && child.max_edit_segments <= parent.max_edit_segments
}

fn command_settings_lte(child: CommandSettings, parent: CommandSettings) -> bool {
    !(parent.network == CommandNetwork::None && child.network == CommandNetwork::Open)
        && child.default_timeout_secs <= parent.default_timeout_secs
        && child.max_timeout_secs <= parent.max_timeout_secs
}

fn clean_lexical_absolute_path(path: &Path) -> Result<PathBuf, NarrowingError> {
    if !path.is_absolute() {
        return Err(NarrowingError::WorkspaceEscalation);
    }
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return Err(NarrowingError::WorkspaceEscalation),
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                cleaned.push(component.as_os_str());
            }
        }
    }
    Ok(cleaned)
}

/// Derive a child [`SessionDefinition`] from `parent` and `req`, rejecting any capability,
/// workspace, resource-limit, or permission escalation.
pub fn narrow_child_definition(
    parent: &SessionDefinition,
    req: &ChildNarrowingRequest,
) -> Result<SessionDefinition, NarrowingError> {
    let mut child = parent.clone();

    if let Some(model_override) = &req.model_override {
        let provider_changed = model_override
            .provider
            .is_some_and(|provider| provider != parent.provider);
        if let Some(provider) = model_override.provider {
            child.provider = provider;
        }
        if let Some(model) = &model_override.model {
            child.model = model.clone();
        }
        if let Some(endpoint) = &model_override.endpoint {
            child.endpoint = endpoint.clone();
        }
        if let Some(credential) = &model_override.credential {
            child.credential = credential.clone();
        }
        if let Some(max_tokens) = model_override.max_tokens {
            child.max_tokens = max_tokens;
        }
        if let Some(reasoning_effort) = &model_override.reasoning_effort {
            child.reasoning_effort = reasoning_effort.clone();
        }
        if let Some(temperature) = model_override.temperature {
            child.temperature = temperature;
        }
        if let Some(extra_body) = &model_override.extra_body {
            child.extra_body = extra_body.clone();
        }
        if let Some(context_window) = model_override.context_window {
            child.context_window = context_window;
        }
        if let Some(max_model_attempts) = model_override.max_model_attempts {
            child.max_model_attempts = max_model_attempts;
        }
        if let Some(render_options) = &model_override.render_options {
            child.render_options = render_options.clone();
        } else if provider_changed {
            child.render_options.clear();
        }
    }

    let workspace_explicitly_disabled = matches!(req.workspace_root, Some(None));
    match &req.workspace_root {
        None => {}
        Some(None) => {
            child.workspace_root = None;
        }
        Some(Some(requested_root)) => {
            let Some(parent_root) = &parent.workspace_root else {
                return Err(NarrowingError::WorkspaceEscalation);
            };
            let norm_parent = clean_lexical_absolute_path(parent_root)?;
            let norm_child = clean_lexical_absolute_path(requested_root)?;
            if !norm_child.starts_with(&norm_parent) {
                return Err(NarrowingError::WorkspaceEscalation);
            }
            child.workspace_root = Some(norm_child);
        }
    }

    match &req.tools {
        None => {
            if workspace_explicitly_disabled {
                child.tools.clear();
                child.write_limits = None;
                child.command = None;
            }
        }
        Some(requested_tools) => {
            if !requested_tools.is_empty() && child.workspace_root.is_none() {
                return Err(NarrowingError::WorkspaceRequiredForTools);
            }
            let mut seen = HashSet::with_capacity(requested_tools.len());
            for &tool in requested_tools {
                if !seen.insert(tool) {
                    return Err(NarrowingError::DuplicateTool);
                }
                if !parent.tools.contains(&tool) {
                    return Err(NarrowingError::ToolEscalation(tool));
                }
            }
            child.tools = requested_tools.clone();
            if workspace_explicitly_disabled {
                child.write_limits = None;
                child.command = None;
            }
        }
    }

    if let Some(limits) = &req.tool_limits {
        limits.validate()?;
        let p = &parent.tool_limits;
        if limits.max_concurrent_operations > p.max_concurrent_operations
            || limits.call_timeout_secs > p.call_timeout_secs
            || limits.max_output_bytes > p.max_output_bytes
            || limits.max_directory_entries > p.max_directory_entries
            || limits.max_file_scan_bytes > p.max_file_scan_bytes
            || limits.max_search_files > p.max_search_files
            || limits.max_search_depth > p.max_search_depth
            || limits.max_search_scan_bytes > p.max_search_scan_bytes
            || limits.max_search_matches > p.max_search_matches
        {
            return Err(NarrowingError::ToolLimitsEscalation);
        }
        child.tool_limits = limits.clone();
    }

    let parent_has_write = parent
        .tools
        .iter()
        .any(|t| matches!(t, WorkspaceTool::WriteFile | WorkspaceTool::EditFile));
    let child_has_write = child
        .tools
        .iter()
        .any(|t| matches!(t, WorkspaceTool::WriteFile | WorkspaceTool::EditFile));
    let parent_effective_write = parent
        .write_limits
        .or_else(|| parent_has_write.then(WriteToolLimits::default));

    if child_has_write {
        match req.write_limits {
            None => {}
            Some(None) => {
                let Some(pwl) = parent_effective_write else {
                    return Err(NarrowingError::WriteLimitsEscalation);
                };
                if !write_limits_lte(WriteToolLimits::default(), pwl) {
                    return Err(NarrowingError::WriteLimitsEscalation);
                }
                child.write_limits = None;
            }
            Some(Some(wl)) => {
                let Some(pwl) = parent_effective_write else {
                    return Err(NarrowingError::WriteLimitsEscalation);
                };
                wl.validate()?;
                if !write_limits_lte(wl, pwl) {
                    return Err(NarrowingError::WriteLimitsEscalation);
                }
                child.write_limits = Some(wl);
            }
        }
    } else {
        match req.write_limits {
            None | Some(None) => {
                child.write_limits = None;
            }
            Some(Some(wl)) => {
                if child.workspace_root.is_none() {
                    return Err(NarrowingError::WriteLimitsEscalation);
                }
                let Some(pwl) = parent_effective_write else {
                    return Err(NarrowingError::WriteLimitsEscalation);
                };
                wl.validate()?;
                if !write_limits_lte(wl, pwl) {
                    return Err(NarrowingError::WriteLimitsEscalation);
                }
                child.write_limits = Some(wl);
            }
        }
    }

    if child.tools.contains(&WorkspaceTool::RunCommand) {
        let parent_effective_cmd = parent
            .effective_command_settings()
            .ok_or(NarrowingError::CommandSettingsEscalation)?;
        match req.command {
            None => {}
            Some(None) => {
                if !command_settings_lte(CommandSettings::default(), parent_effective_cmd) {
                    return Err(NarrowingError::CommandSettingsEscalation);
                }
                child.command = None;
            }
            Some(Some(cmd)) => {
                cmd.validate()?;
                if !command_settings_lte(cmd, parent_effective_cmd) {
                    return Err(NarrowingError::CommandSettingsEscalation);
                }
                child.command = Some(cmd);
            }
        }
    } else {
        match req.command {
            None | Some(None) => {
                child.command = None;
            }
            Some(Some(_)) => {
                return Err(NarrowingError::CommandSettingsEscalation);
            }
        }
    }

    match req.code_execution {
        None => {}
        Some(None) => {
            child.code_execution = None;
        }
        Some(Some(ce)) => {
            let Some(pce) = parent.code_execution else {
                return Err(NarrowingError::CodeExecutionEscalation);
            };
            ce.validate()?;
            if ce.timeout_secs > pce.timeout_secs
                || ce.memory_limit_bytes > pce.memory_limit_bytes
                || ce.stack_limit_bytes > pce.stack_limit_bytes
                || ce.max_subcalls > pce.max_subcalls
                || ce.max_concurrency > pce.max_concurrency
                || ce.max_output_bytes > pce.max_output_bytes
            {
                return Err(NarrowingError::CodeExecutionEscalation);
            }
            child.code_execution = Some(ce);
        }
    }

    if let Some(mcp_ids) = &req.mcp_server_ids {
        let mut seen = HashSet::with_capacity(mcp_ids.len());
        let mut selected = Vec::with_capacity(mcp_ids.len());
        for id in mcp_ids {
            if !seen.insert(id.as_str()) {
                return Err(NarrowingError::DuplicateMcpServer(id.clone()));
            }
            let Some(config) = parent.mcp_servers.iter().find(|s| &s.id == id) else {
                return Err(NarrowingError::UnknownMcpServer(id.clone()));
            };
            selected.push(config.clone());
        }
        child.mcp_servers = selected;
    }

    if let Some(rounds) = req.max_model_rounds {
        if rounds > parent.max_model_rounds {
            return Err(NarrowingError::RoundLimitEscalation {
                parent: parent.max_model_rounds,
                child: rounds,
            });
        }
        child.max_model_rounds = rounds;
    }

    if let Some(tool_calls) = req.max_tool_calls {
        if tool_calls > parent.max_tool_calls {
            return Err(NarrowingError::ToolCallLimitEscalation {
                parent: parent.max_tool_calls,
                child: tool_calls,
            });
        }
        child.max_tool_calls = tool_calls;
    }

    if let Some(timeout) = req.work_timeout_secs {
        if let Some(parent_timeout) = parent.work_timeout_secs
            && timeout > parent_timeout
        {
            return Err(NarrowingError::TimeoutEscalation {
                parent: parent_timeout,
                child: timeout,
            });
        }
        child.work_timeout_secs = Some(timeout);
    }

    if let Some(mode) = req.permission_mode {
        check_permission_mode_narrowing(parent.initial_permission_mode(), mode)?;
        child.permission_mode = (mode != PermissionMode::Ask).then_some(mode);
    }

    child.validate()?;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::McpServerConfig;
    use std::collections::BTreeMap;

    fn full_parent_definition(root: &Path) -> SessionDefinition {
        SessionDefinition {
            provider: Provider::OpenAiChat,
            model: "parent-model".into(),
            endpoint: Some("https://api.example.com/v1".into()),
            credential: CredentialRef::Environment("TARS_PARENT_KEY".into()),
            work_timeout_secs: Some(120),
            retained_work_capacity: 16,
            max_model_rounds: 8,
            max_tool_calls: 16,
            max_tokens: Some(4096),
            reasoning_effort: Some("medium".into()),
            temperature: None,
            extra_body: Default::default(),
            context_window: Some(64_000),
            auto_compact: true,
            compact_threshold: 70,
            prompt_cache: true,
            max_model_attempts: Some(3),
            render_options: Default::default(),
            workspace_root: Some(root.to_path_buf()),
            tools: vec![
                WorkspaceTool::ListDirectory,
                WorkspaceTool::ReadFile,
                WorkspaceTool::SearchText,
                WorkspaceTool::WriteFile,
                WorkspaceTool::EditFile,
                WorkspaceTool::RunCommand,
            ],
            tool_limits: WorkspaceToolLimits::default(),
            write_limits: Some(WriteToolLimits::default()),
            permission_mode: Some(PermissionMode::AcceptEdits),
            command: Some(CommandSettings {
                network: CommandNetwork::None,
                default_timeout_secs: 60,
                max_timeout_secs: 300,
            }),
            mcp_servers: vec![
                McpServerConfig {
                    id: "docs".into(),
                    command: None,
                    args: Vec::new(),
                    url: Some("https://mcp.example.com/docs".into()),
                    env: BTreeMap::new(),
                    headers: BTreeMap::new(),
                    startup_timeout_sec: 10,
                    tool_timeout_sec: 30,
                    enabled_tools: None,
                    disabled_tools: None,
                    approval: crate::definition::McpApproval::Writes,
                    tools: BTreeMap::new(),
                    required: false,
                },
                McpServerConfig {
                    id: "search".into(),
                    command: None,
                    args: Vec::new(),
                    url: Some("https://mcp.example.com/search".into()),
                    env: BTreeMap::new(),
                    headers: BTreeMap::new(),
                    startup_timeout_sec: 10,
                    tool_timeout_sec: 30,
                    enabled_tools: None,
                    disabled_tools: None,
                    approval: crate::definition::McpApproval::Prompt,
                    tools: BTreeMap::new(),
                    required: false,
                },
            ],
            prelude: Default::default(),
            code_execution: Some(CodeExecutionSettings::default()),
        }
    }

    #[test]
    fn valid_narrowing_tightens_bounds_and_disabling_run_command_clears_command_settings() {
        let parent_root = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\repo")
        } else {
            PathBuf::from("/workspace/repo")
        };
        let child_subpath = parent_root.join("crates/core");
        let parent = full_parent_definition(&parent_root);

        let mut tighter_limits = parent.tool_limits.clone();
        tighter_limits.max_concurrent_operations = 2;
        tighter_limits.max_output_bytes = 4096;

        let req = ChildNarrowingRequest {
            model_override: Some(ChildModelOverride {
                provider: Some(Provider::Anthropic),
                model: Some("claude-sonnet".into()),
                reasoning_effort: Some(Some("high".into())),
                max_tokens: Some(Some(2048)),
                ..Default::default()
            }),
            workspace_root: Some(Some(child_subpath.clone())),
            tools: Some(vec![WorkspaceTool::ReadFile, WorkspaceTool::SearchText]),
            tool_limits: Some(tighter_limits.clone()),
            write_limits: Some(Some(WriteToolLimits {
                max_edit_file_bytes: 1024,
                max_write_bytes: 512,
                max_edit_segments: 4,
            })),
            command: None,
            code_execution: Some(Some(CodeExecutionSettings {
                timeout_secs: 10,
                ..CodeExecutionSettings::default()
            })),
            mcp_server_ids: Some(vec!["docs".into()]),
            max_model_rounds: Some(4),
            max_tool_calls: Some(6),
            work_timeout_secs: Some(30),
            permission_mode: Some(PermissionMode::ReadOnly),
        };

        let child = narrow_child_definition(&parent, &req).expect("valid narrowing");
        assert_eq!(child.provider, Provider::Anthropic);
        assert_eq!(child.model, "claude-sonnet");
        assert_eq!(child.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(child.max_tokens, Some(2048));
        assert_eq!(child.workspace_root, Some(child_subpath));
        assert_eq!(
            child.tools,
            vec![WorkspaceTool::ReadFile, WorkspaceTool::SearchText]
        );
        assert_eq!(child.tool_limits, tighter_limits);
        assert!(child.command.is_none());
        assert_eq!(child.mcp_servers.len(), 1);
        assert_eq!(child.mcp_servers[0].id, "docs");
        assert_eq!(
            child.mcp_servers[0].approval,
            parent.mcp_servers[0].approval
        );
        assert_eq!(child.max_model_rounds, 4);
        assert_eq!(child.max_tool_calls, 6);
        assert_eq!(child.work_timeout_secs, Some(30));
        assert_eq!(child.permission_mode, Some(PermissionMode::ReadOnly));
    }

    #[test]
    fn switching_model_only_never_widens_tools_workspace_or_permissions() {
        let parent_root = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\repo")
        } else {
            PathBuf::from("/workspace/repo")
        };
        let mut parent = full_parent_definition(&parent_root);
        parent.tools = vec![WorkspaceTool::ReadFile];
        parent.write_limits = None;
        parent.command = None;
        parent.permission_mode = Some(PermissionMode::ReadOnly);

        let child = narrow_child_definition(
            &parent,
            &ChildNarrowingRequest {
                model_override: Some(ChildModelOverride {
                    provider: Some(Provider::OpenAiResponses),
                    model: Some("gpt-5".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(child.tools, parent.tools);
        assert_eq!(child.workspace_root, parent.workspace_root);
        assert_eq!(child.tool_limits, parent.tool_limits);
        assert_eq!(child.write_limits, parent.write_limits);
        assert_eq!(child.permission_mode, parent.permission_mode);
        assert_eq!(child.work_timeout_secs, parent.work_timeout_secs);
    }

    #[test]
    fn disabling_workspace_clears_tools_write_and_command_but_rejects_explicit_tools() {
        let parent_root = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\repo")
        } else {
            PathBuf::from("/workspace/repo")
        };
        let parent = full_parent_definition(&parent_root);

        let child = narrow_child_definition(
            &parent,
            &ChildNarrowingRequest {
                workspace_root: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(child.workspace_root.is_none());
        assert!(child.tools.is_empty());
        assert!(child.write_limits.is_none());
        assert!(child.command.is_none());

        let err = narrow_child_definition(
            &parent,
            &ChildNarrowingRequest {
                workspace_root: Some(None),
                tools: Some(vec![WorkspaceTool::ReadFile]),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err, NarrowingError::WorkspaceRequiredForTools);
    }

    #[test]
    fn every_escalation_attempt_is_rejected() {
        let parent_root = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\repo")
        } else {
            PathBuf::from("/workspace/repo")
        };
        let parent = full_parent_definition(&parent_root);

        // 1. Extra tool not in parent
        let mut restricted_parent = parent.clone();
        restricted_parent.tools = vec![WorkspaceTool::ReadFile];
        restricted_parent.command = None;
        assert_eq!(
            narrow_child_definition(
                &restricted_parent,
                &ChildNarrowingRequest {
                    tools: Some(vec![WorkspaceTool::ReadFile, WorkspaceTool::WriteFile]),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::ToolEscalation(WorkspaceTool::WriteFile))
        );

        // 2. Workspace invention when parent has none
        let mut no_ws_parent = parent.clone();
        no_ws_parent.workspace_root = None;
        no_ws_parent.tools.clear();
        no_ws_parent.command = None;
        no_ws_parent.write_limits = None;
        assert_eq!(
            narrow_child_definition(
                &no_ws_parent,
                &ChildNarrowingRequest {
                    workspace_root: Some(Some(parent_root.clone())),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::WorkspaceEscalation)
        );

        // 3. Workspace path traversal (`..`) and sibling directory escape
        let traversal = parent_root.join("sub/../other");
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    workspace_root: Some(Some(traversal)),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::WorkspaceEscalation)
        );
        let sibling = parent_root.parent().unwrap().join("repo-evil");
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    workspace_root: Some(Some(sibling)),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::WorkspaceEscalation)
        );

        // 4. tool_limits widening
        let mut wider_limits = parent.tool_limits.clone();
        wider_limits.max_output_bytes = 32 * 1024;
        let mut tighter_parent = parent.clone();
        tighter_parent.tool_limits.max_output_bytes = 16 * 1024;
        assert_eq!(
            narrow_child_definition(
                &tighter_parent,
                &ChildNarrowingRequest {
                    tool_limits: Some(wider_limits),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::ToolLimitsEscalation)
        );

        // 5. write_limits widening & invention
        let mut no_wl_parent = parent.clone();
        no_wl_parent.tools = vec![WorkspaceTool::ReadFile];
        no_wl_parent.command = None;
        no_wl_parent.write_limits = None;
        assert_eq!(
            narrow_child_definition(
                &no_wl_parent,
                &ChildNarrowingRequest {
                    write_limits: Some(Some(WriteToolLimits::default())),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::WriteLimitsEscalation)
        );
        let mut small_wl_parent = parent.clone();
        small_wl_parent.write_limits = Some(WriteToolLimits {
            max_edit_file_bytes: 1024,
            max_write_bytes: 1024,
            max_edit_segments: 2,
        });
        assert_eq!(
            narrow_child_definition(
                &small_wl_parent,
                &ChildNarrowingRequest {
                    write_limits: Some(Some(WriteToolLimits {
                        max_edit_file_bytes: 2048,
                        max_write_bytes: 1024,
                        max_edit_segments: 2,
                    })),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::WriteLimitsEscalation)
        );

        // 6. CommandNetwork::None -> Open and command timeout widening
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    command: Some(Some(CommandSettings {
                        network: CommandNetwork::Open,
                        default_timeout_secs: 60,
                        max_timeout_secs: 300,
                    })),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::CommandSettingsEscalation)
        );
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    command: Some(Some(CommandSettings {
                        network: CommandNetwork::None,
                        default_timeout_secs: 61,
                        max_timeout_secs: 300,
                    })),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::CommandSettingsEscalation)
        );

        // 7. code_execution invention & widening
        let mut no_ce_parent = parent.clone();
        no_ce_parent.code_execution = None;
        assert_eq!(
            narrow_child_definition(
                &no_ce_parent,
                &ChildNarrowingRequest {
                    code_execution: Some(Some(CodeExecutionSettings::default())),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::CodeExecutionEscalation)
        );
        let mut small_ce_parent = parent.clone();
        small_ce_parent.code_execution = Some(CodeExecutionSettings {
            timeout_secs: 15,
            ..CodeExecutionSettings::default()
        });
        assert_eq!(
            narrow_child_definition(
                &small_ce_parent,
                &ChildNarrowingRequest {
                    code_execution: Some(Some(CodeExecutionSettings {
                        timeout_secs: 20,
                        ..CodeExecutionSettings::default()
                    })),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::CodeExecutionEscalation)
        );

        // 8. timeout extension
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    work_timeout_secs: Some(121),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::TimeoutEscalation {
                parent: 120,
                child: 121,
            })
        );

        // 9. max_model_rounds and max_tool_calls increase
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    max_model_rounds: Some(9),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::RoundLimitEscalation {
                parent: 8,
                child: 9,
            })
        );
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    max_tool_calls: Some(17),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::ToolCallLimitEscalation {
                parent: 16,
                child: 17,
            })
        );

        // 10. PermissionMode widening (ReadOnly -> Ask, Ask -> FullAccess)
        let mut readonly_parent = parent.clone();
        readonly_parent.permission_mode = Some(PermissionMode::ReadOnly);
        assert_eq!(
            narrow_child_definition(
                &readonly_parent,
                &ChildNarrowingRequest {
                    permission_mode: Some(PermissionMode::Ask),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::PermissionModeEscalation {
                parent: PermissionMode::ReadOnly,
                child: PermissionMode::Ask,
            })
        );
        let mut ask_parent = parent.clone();
        ask_parent.permission_mode = None; // Ask
        assert_eq!(
            narrow_child_definition(
                &ask_parent,
                &ChildNarrowingRequest {
                    permission_mode: Some(PermissionMode::FullAccess),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::PermissionModeEscalation {
                parent: PermissionMode::Ask,
                child: PermissionMode::FullAccess,
            })
        );

        // 11. Unknown MCP server ID
        assert_eq!(
            narrow_child_definition(
                &parent,
                &ChildNarrowingRequest {
                    mcp_server_ids: Some(vec!["unknown-mcp".into()]),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::UnknownMcpServer("unknown-mcp".into()))
        );
    }

    #[test]
    fn write_limits_and_command_default_effective_bounds_are_enforced_both_ways() {
        let parent_root = if cfg!(windows) {
            PathBuf::from(r"C:\workspace\repo")
        } else {
            PathBuf::from("/workspace/repo")
        };

        // Parent has WriteFile + RunCommand with None settings -> effective defaults apply,
        // so tightening in child succeeds!
        let mut default_parent = full_parent_definition(&parent_root);
        default_parent.write_limits = None;
        default_parent.command = None;

        let tightened = narrow_child_definition(
            &default_parent,
            &ChildNarrowingRequest {
                write_limits: Some(Some(WriteToolLimits {
                    max_edit_file_bytes: 1024,
                    max_write_bytes: 512,
                    max_edit_segments: 4,
                })),
                command: Some(Some(CommandSettings {
                    network: CommandNetwork::None,
                    default_timeout_secs: 30,
                    max_timeout_secs: 60,
                })),
                ..Default::default()
            },
        )
        .expect("tightening when parent uses default effective bounds must succeed");
        assert_eq!(tightened.write_limits.unwrap().max_write_bytes, 512);
        assert_eq!(tightened.command.unwrap().default_timeout_secs, 30);

        // Parent has tight write_limits and tight command settings -> child passing Some(None)
        // would revert to larger assembly defaults, so it MUST be rejected!
        let mut tight_parent = full_parent_definition(&parent_root);
        tight_parent.write_limits = Some(WriteToolLimits {
            max_edit_file_bytes: 1024,
            max_write_bytes: 1024,
            max_edit_segments: 2,
        });
        tight_parent.command = Some(CommandSettings {
            network: CommandNetwork::None,
            default_timeout_secs: 30,
            max_timeout_secs: 60,
        });

        assert_eq!(
            narrow_child_definition(
                &tight_parent,
                &ChildNarrowingRequest {
                    write_limits: Some(None),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::WriteLimitsEscalation)
        );
        assert_eq!(
            narrow_child_definition(
                &tight_parent,
                &ChildNarrowingRequest {
                    command: Some(None),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::CommandSettingsEscalation)
        );

        // Duplicate tool, duplicate MCP server ID, and work_timeout_secs = Some(0) vs None
        assert_eq!(
            narrow_child_definition(
                &tight_parent,
                &ChildNarrowingRequest {
                    tools: Some(vec![WorkspaceTool::ReadFile, WorkspaceTool::ReadFile]),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::DuplicateTool)
        );
        assert_eq!(
            narrow_child_definition(
                &tight_parent,
                &ChildNarrowingRequest {
                    mcp_server_ids: Some(vec!["docs".into(), "docs".into()]),
                    ..Default::default()
                },
            ),
            Err(NarrowingError::DuplicateMcpServer("docs".into()))
        );

        let mut no_timeout_parent = full_parent_definition(&parent_root);
        no_timeout_parent.work_timeout_secs = None;
        let zero_timeout_child = narrow_child_definition(
            &no_timeout_parent,
            &ChildNarrowingRequest {
                work_timeout_secs: Some(0),
                ..Default::default()
            },
        )
        .expect("Some(0) narrows None timeout");
        assert_eq!(zero_timeout_child.work_timeout_secs, Some(0));
    }
}
