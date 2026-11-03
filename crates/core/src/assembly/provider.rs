use crate::compact::CompactionRuntime;
use crate::compact::estimate::{UsageTracker, UsageTrackingGateway};
use crate::compact::preparer::CompactionPreparer;
use crate::definition::{CredentialRef, DefinitionError, RenderOptions, SessionDefinition};
use crate::external_tools::ExternalTools;
use crate::session::{ConversationId, Session};
use crate::tools::{ToolResources, WorkspaceTools};
use crate::workspace::{Workspace, WorkspaceError};
use causa::kernel::ToolBatchProcessor;
use causa::kernel::{Context, ModelGateway};
use causa::providers::{
    AnthropicMessagesGateway, OpenAiChatCompletionsGateway, OpenAiResponsesGateway,
};
use causa::runtime::{RetryPolicy, RetryingGateway, ToolExecutor, ToolExecutorOptions, TurnRunner};
use std::sync::Arc;

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum AssemblyError {
    #[error("invalid session definition: {0}")]
    Definition(#[from] DefinitionError),
    #[error("credential environment variable is missing, non-Unicode or blank")]
    CredentialUnavailable,
    #[error("workspace resource unavailable: {0}")]
    Workspace(Arc<WorkspaceError>),
    #[error("command tool unavailable: {0}")]
    CommandUnavailable(String),
    #[error("search tool unavailable: {0}")]
    SearchUnavailable(String),
}

/// Host resources an assembly uses, plus the store path a command must not read.
pub(crate) struct SessionResources {
    pub(crate) tools: Arc<ToolResources>,
    /// Sessions directory; the command tool must not read it and history lives
    /// beside the records.
    pub(crate) storage: Option<std::path::PathBuf>,
    /// Restored history index of a reopened session.
    pub(crate) history: Vec<crate::compact::HistorySegment>,
    /// Running automatic-compaction switch read from the record, if present.
    pub(crate) auto_compact: Option<bool>,
    /// Running trigger percentage read from the record, if present.
    pub(crate) compact_threshold: Option<u8>,
    /// Whether this instance is persistent; history needs a persisted record.
    pub(crate) persistent: bool,
    /// Test-only gateway injection so the real assembly path can be exercised
    /// with a scripted model.
    #[cfg(test)]
    pub(crate) gateway_override: Option<Arc<dyn ModelGateway>>,
    /// Process-wide external-tool check used to bind the search runtime.
    pub(crate) external_tools: Option<ExternalTools>,
    /// Test-only command sandbox probe result injected into the real assembly
    /// path. Mirrors `CommandRuntime::bind_with_sandbox`: no production code
    /// can choose a backend or a probe result.
    #[cfg(test)]
    pub(crate) command_sandbox:
        Option<Result<crate::sandbox::Sandbox, crate::sandbox::SandboxUnavailable>>,
    /// Test-only search sandbox probe injection, same seam as the command one.
    #[cfg(test)]
    pub(crate) search_sandbox:
        Option<Result<crate::sandbox::Sandbox, crate::sandbox::SandboxUnavailable>>,
}

impl SessionResources {
    pub(crate) fn new(tools: Arc<ToolResources>) -> Self {
        Self {
            tools,
            storage: None,
            history: Vec::new(),
            auto_compact: None,
            compact_threshold: None,
            persistent: false,
            external_tools: None,
            #[cfg(test)]
            gateway_override: None,
            #[cfg(test)]
            command_sandbox: None,
            #[cfg(test)]
            search_sandbox: None,
        }
    }

    /// Bind the search runtime with the process-wide external-tool check.
    pub(crate) fn with_external_tools(mut self, tools: ExternalTools) -> Self {
        self.external_tools = Some(tools);
        self
    }

    pub(crate) fn with_storage(mut self, storage: Option<std::path::PathBuf>) -> Self {
        self.storage = storage;
        self
    }

    /// Restore the reopened session's history index and running values.
    pub(crate) fn with_compaction(
        mut self,
        history: Vec<crate::compact::HistorySegment>,
        auto_compact: Option<bool>,
        compact_threshold: Option<u8>,
    ) -> Self {
        self.history = history;
        self.auto_compact = auto_compact;
        self.compact_threshold = compact_threshold;
        self
    }

    /// Mark the instance as persistent so compaction may keep history.
    pub(crate) fn persistent(mut self, persistent: bool) -> Self {
        self.persistent = persistent;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_gateway(mut self, gateway: Arc<dyn ModelGateway>) -> Self {
        self.gateway_override = Some(gateway);
        self
    }

    /// Bind the command runtime with an injected probe result instead of
    /// probing the platform backend. Test-only: no production path can choose
    /// a backend or a probe outcome.
    #[cfg(test)]
    pub(crate) fn with_test_command_sandbox(
        mut self,
        sandbox: Result<crate::sandbox::Sandbox, crate::sandbox::SandboxUnavailable>,
    ) -> Self {
        self.command_sandbox = Some(sandbox);
        self
    }

    /// Bind the search runtime with an injected probe result instead of
    /// probing the platform backend.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn with_test_search_sandbox(
        mut self,
        sandbox: Result<crate::sandbox::Sandbox, crate::sandbox::SandboxUnavailable>,
    ) -> Self {
        self.search_sandbox = Some(sandbox);
        self
    }
}

pub(crate) fn assemble(
    definition: &SessionDefinition,
    conversation_id: ConversationId,
    context: Context,
    resources: SessionResources,
    current_mode: crate::permission::PermissionMode,
    mcp: Option<Arc<crate::mcp::McpRuntime>>,
    allow_unavailable_command: bool,
) -> Result<Session, AssemblyError> {
    let SessionResources {
        tools,
        storage,
        history,
        auto_compact,
        compact_threshold,
        persistent,
        external_tools,
        #[cfg(test)]
        gateway_override,
        #[cfg(test)]
        command_sandbox,
        #[cfg(test)]
        search_sandbox,
    } = resources;
    let storage_for_history = storage.clone();
    definition.validate()?;
    let command_settings = definition.effective_command_settings();
    let definition_command_settings = command_settings.unwrap_or_default();
    let mut command_status = None;
    let workspace_tools = if definition.tools.is_empty() {
        None
    } else {
        let root = definition
            .workspace_root
            .as_deref()
            .expect("validated root");
        // Bind the command runtime before opening the workspace: a platform
        // without a backend must fail creation with an error that names the
        // platform, not with a workspace error, and the workspace tools are
        // unavailable on those platforms anyway.
        let command = if let Some(settings) = command_settings {
            #[cfg(test)]
            let command = match command_sandbox {
                Some(sandbox) => crate::tools::CommandRuntime::bind_with_sandbox(
                    root, settings, sandbox, storage,
                ),
                None => crate::tools::CommandRuntime::bind(root, settings, storage),
            };
            #[cfg(not(test))]
            let command = crate::tools::CommandRuntime::bind(root, settings, storage);
            let command =
                command.map_err(|error| AssemblyError::CommandUnavailable(error.to_string()))?;
            // Creating a session fails closed without a backend; reopening may
            // keep the saved command tool and reject every call at runtime.
            if !allow_unavailable_command && let Some(reason) = command.unavailable_platform() {
                return Err(AssemblyError::CommandUnavailable(reason));
            }
            // The runtime is moved into the shared tool set; capture the probe
            // result first so the instance can report it without the tools.
            command_status = Some(command.sandbox_status());
            Some(command)
        } else {
            None
        };
        let workspace =
            Workspace::open(root).map_err(|error| AssemblyError::Workspace(Arc::new(error)))?;
        // The search runtime shares the session's workspace handle so its path
        // check uses handle-relative access; an unready sandbox or `rg` still
        // binds and rejects every call instead of failing creation.
        let search = if definition
            .tools
            .contains(&crate::definition::WorkspaceTool::SearchText)
        {
            #[cfg(test)]
            let bound = match search_sandbox {
                Some(sandbox) => crate::tools::SearchRuntime::bind_with_sandbox(
                    workspace.clone(),
                    root,
                    external_tools.as_ref(),
                    sandbox,
                ),
                None => crate::tools::SearchRuntime::bind(
                    workspace.clone(),
                    root,
                    external_tools.as_ref(),
                ),
            };
            #[cfg(not(test))]
            let bound =
                crate::tools::SearchRuntime::bind(workspace.clone(), root, external_tools.as_ref());
            Some(bound.map_err(|error| AssemblyError::SearchUnavailable(error.to_string()))?)
        } else {
            None
        };
        let tool_limits = definition.tool_limits.clone();
        let write_limits = definition.write_limits.unwrap_or_default();
        let settings = command_settings.unwrap_or_default();
        Some(match (command, search) {
            (None, None) => {
                WorkspaceTools::with_write_limits(workspace, tool_limits, write_limits, tools)
            }
            (Some(command), None) => WorkspaceTools::with_command(
                workspace,
                tool_limits,
                write_limits,
                tools,
                settings,
                Some(command),
            ),
            (command, search) => WorkspaceTools::with_runtimes(
                workspace,
                tool_limits,
                write_limits,
                tools,
                settings,
                command,
                search,
            ),
        })
    };
    // Tests may substitute only the protocol gateway, so the retry and
    // counting layers still wrap the fixture.
    #[cfg(test)]
    let innermost_gateway = match gateway_override {
        Some(gateway) => gateway,
        None => definition_gateway(definition)?,
    };
    #[cfg(not(test))]
    let innermost_gateway = definition_gateway(definition)?;
    let usage_hub = Arc::new(crate::usage::UsageHub::new());
    let conversation_retry = wrap_retry_and_count(
        innermost_gateway.clone(),
        crate::usage::UsagePurpose::Conversation,
        usage_hub.clone(),
        definition.max_model_attempts,
    );
    let summary_gateway = wrap_retry_and_count(
        innermost_gateway,
        crate::usage::UsagePurpose::Compaction,
        usage_hub.clone(),
        definition.max_model_attempts,
    );
    let usage = Arc::new(UsageTracker::default());
    // The summary request bypasses usage capture: its (possibly trimmed)
    // prompt is not the conversation's next request.
    let gateway: Arc<dyn ModelGateway> =
        Arc::new(UsageTrackingGateway::new(conversation_retry, usage.clone()));
    let compaction = CompactionRuntime::new(
        definition,
        &conversation_id,
        persistent
            .then_some(storage_for_history.as_deref())
            .flatten(),
        history,
        auto_compact,
        compact_threshold,
        summary_gateway,
        usage,
    );
    let options = definition.run_options();
    let selected = definition.tools.clone();
    let builtin_timeout = (!selected.is_empty())
        .then(|| std::time::Duration::from_secs(definition.tool_limits.call_timeout_secs));
    let builtin_timeout = match (
        builtin_timeout,
        command_settings
            .map(|settings| std::time::Duration::from_secs(settings.max_timeout_secs + 5)),
    ) {
        (Some(builtin), Some(command)) => Some(builtin.max(command)),
        (builtin, command) => builtin.or(command),
    };
    // The executor deadline applies to every call, built-in and MCP alike.
    // Each tool narrows it to its own frozen deadline (built-in limits or the
    // server's `tool_timeout_sec`), so the shared value is the largest.
    let mcp_timeout = mcp.as_ref().and_then(|mcp| mcp.max_tool_timeout());
    let call_timeout = match (builtin_timeout, mcp_timeout) {
        (Some(builtin), Some(mcp)) => Some(builtin.max(mcp)),
        (builtin, mcp) => builtin.or(mcp),
    };
    let mcp_table = mcp
        .as_ref()
        .map(|m| m.permission_table())
        .unwrap_or_else(|| Arc::new(crate::permission::McpPermissionTable::new()));
    let permissions = Arc::new(crate::permission::PermissionHub::with_mcp_table(
        current_mode,
        mcp_table,
    ));
    let view = permissions.view();
    let approvals = permissions.approvals();
    let session_permissions = permissions.clone();
    let mcp_for_work = mcp.clone();
    #[cfg(test)]
    let test_processors = Arc::new(std::sync::Mutex::new(
        Vec::<Arc<dyn ToolBatchProcessor>>::new(),
    ));
    #[cfg(test)]
    let test_processors_capture = test_processors.clone();
    #[cfg(test)]
    let test_intermediate = Arc::new(std::sync::Mutex::new(
        Vec::<Arc<dyn ToolBatchProcessor>>::new(),
    ));
    #[cfg(test)]
    let test_intermediate_capture = test_intermediate.clone();
    #[cfg(test)]
    let tools_for_session = workspace_tools.clone();
    let outside_sandbox_grants = workspace_tools
        .as_ref()
        .map(|tools| tools.outside_sandbox_grants());

    let compaction_for_work = compaction.clone();
    let session = Session::new_with_tools(
        conversation_id,
        context,
        move |work, _control, reporter| {
            let gateway = gateway.clone();
            let compact_for_work = compaction_for_work.clone();
            let reporter_for_compaction = reporter.clone();
            let options = options
                .clone()
                .with_processor_data(causa::kernel::ProcessorData::new(view.clone()));
            let prepared_writes = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
                causa::kernel::BlockId,
                crate::tools::PreparedWrite,
            >::new()));
            let tools = workspace_tools.as_ref().map_or_else(Vec::new, |tools| {
                tools.bind_with_prepared(&selected, reporter.clone(), prepared_writes.clone())
            });
            let mut before: Vec<Arc<dyn ToolBatchProcessor>> = Vec::new();
            if workspace_tools.is_some() || mcp_for_work.is_some() {
                before.push(Arc::new(crate::permission::ModeGateProcessor::new(
                    reporter.clone(),
                    approvals.clone(),
                )));
            }
            #[cfg(test)]
            {
                let extras = test_intermediate_capture
                    .lock()
                    .expect("test intermediate processors lock");
                before.extend(extras.iter().cloned());
            }
            if let Some(wt) = &workspace_tools {
                before.push(Arc::new(crate::permission::PrecheckProcessor::new(
                    wt.clone(),
                    reporter.clone(),
                    prepared_writes.clone(),
                )));
            }
            // Every tool that can have effects is gated: built-in tools and
            // MCP tools alike pass the permission processor.
            if workspace_tools.is_some() || mcp_for_work.is_some() {
                before.push(Arc::new(crate::permission::PermissionProcessor::new(
                    work,
                    reporter,
                    approvals.clone(),
                    definition_command_settings,
                    prepared_writes,
                    outside_sandbox_grants
                        .clone()
                        .unwrap_or_else(crate::permission::OutsideSandboxGrants::new),
                )));
            }
            let mcp = mcp_for_work.clone();
            #[cfg(test)]
            {
                let extras = test_processors_capture
                    .lock()
                    .expect("test processors lock");
                before.extend(extras.iter().cloned());
            }
            async move {
                let executor = ToolExecutor::new(tools, {
                    let mut options = ToolExecutorOptions::default();
                    options.call_timeout = call_timeout;
                    options.before = before;
                    options
                })
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
                if let Some(mcp) = &mcp {
                    for source in mcp.sources() {
                        executor.register_dynamic(source).map_err(|error| {
                            Box::new(error) as Box<dyn std::error::Error + Send + Sync>
                        })?;
                    }
                }
                let mut options = options;
                if let Some(runtime) = compact_for_work {
                    options = options.with_preparer(Arc::new(CompactionPreparer::new(
                        runtime,
                        reporter_for_compaction,
                    )));
                }
                Ok((
                    Arc::new(TurnRunner::new(gateway, Arc::new(executor))),
                    options,
                ))
            }
        },
        definition.session_config(),
    )
    .allow_builtin_repair()
    .with_permissions(session_permissions);
    let session = match mcp {
        Some(mcp) => session.with_mcp(mcp),
        None => session,
    };
    let session = match command_status {
        Some(status) => session.with_command_status(status),
        None => session,
    };
    let session = match compaction {
        Some(runtime) => session.with_compaction(runtime),
        None => session,
    };
    let session = session.with_usage(usage_hub);

    #[cfg(test)]
    let session = {
        let mut session = session;
        session.test_before_processors = test_processors;
        session.test_intermediate_processors = test_intermediate;
        session.workspace_tools = tools_for_session;
        session
    };

    Ok(session)
}

/// Count every attempt, then retry around the counter so each retry is
/// counted too. The retry wrapper repeats the same request for transient and
/// rate-limit failures, and never after output became visible.
fn wrap_retry_and_count(
    innermost: Arc<dyn ModelGateway>,
    purpose: crate::usage::UsagePurpose,
    hub: Arc<crate::usage::UsageHub>,
    max_attempts: Option<u32>,
) -> Arc<dyn ModelGateway> {
    let counted = crate::usage::CountingGateway::new(innermost, purpose, hub);
    match max_attempts.filter(|attempts| *attempts > 1) {
        Some(attempts) => Arc::new(RetryingGateway::new(counted, RetryPolicy::new(attempts))),
        None => Arc::new(counted),
    }
}

/// Protocol gateway for the definition, using its environment credential.
fn definition_gateway(
    definition: &SessionDefinition,
) -> Result<Arc<dyn ModelGateway>, AssemblyError> {
    let CredentialRef::Environment(name) = &definition.credential;
    let api_key = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(AssemblyError::CredentialUnavailable)?;
    Ok(protocol_gateway(
        definition.protocol_render_options()?,
        api_key,
        definition.endpoint.clone(),
    ))
}

pub(crate) fn protocol_gateway(
    render_options: RenderOptions,
    api_key: String,
    endpoint: Option<String>,
) -> Arc<dyn ModelGateway> {
    macro_rules! protocol_gateway {
        ($kind:ty, $options:expr) => {{
            let gateway = <$kind>::new(api_key).with_render_options($options);
            let gateway = match endpoint {
                Some(endpoint) => gateway.with_endpoint(endpoint),
                None => gateway,
            };
            Arc::new(gateway) as Arc<dyn ModelGateway>
        }};
    }
    match render_options {
        RenderOptions::Chat(options) => protocol_gateway!(OpenAiChatCompletionsGateway, options),
        RenderOptions::Responses(options) => protocol_gateway!(OpenAiResponsesGateway, options),
        RenderOptions::Anthropic(options) => protocol_gateway!(AnthropicMessagesGateway, options),
    }
}

#[cfg(test)]
pub(crate) fn assemble_with_runner(
    definition: &SessionDefinition,
    conversation_id: ConversationId,
    context: Context,
    runner: Arc<TurnRunner>,
) -> Session {
    let options = definition.run_options();
    Session::new(
        conversation_id,
        context,
        move |_, _| {
            let runner = runner.clone();
            let options = options.clone();
            async move { Ok((runner, options)) }
        },
        definition.session_config(),
    )
}
