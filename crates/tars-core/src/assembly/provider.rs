use crate::definition::{CredentialRef, DefinitionError, RenderOptions, SessionDefinition};
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
}

/// Host resources an assembly uses, plus the store path a command must not read.
pub(crate) struct SessionResources {
    pub(crate) tools: Arc<ToolResources>,
    pub(crate) storage: Option<std::path::PathBuf>,
}

impl SessionResources {
    pub(crate) fn new(tools: Arc<ToolResources>) -> Self {
        Self {
            tools,
            storage: None,
        }
    }

    pub(crate) fn with_storage(mut self, storage: Option<std::path::PathBuf>) -> Self {
        self.storage = storage;
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
    let SessionResources { tools, storage } = resources;
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
        let workspace =
            Workspace::open(root).map_err(|error| AssemblyError::Workspace(Arc::new(error)))?;
        if let Some(settings) = command_settings {
            let command = crate::tools::CommandRuntime::bind(root, settings, storage)
                .map_err(|error| AssemblyError::CommandUnavailable(error.to_string()))?;
            if !allow_unavailable_command && let Some(reason) = command.unavailable_platform() {
                return Err(AssemblyError::CommandUnavailable(reason));
            }
            // The runtime is moved into the shared tool set; capture the probe
            // result first so the instance can report it without the tools.
            command_status = Some(command.sandbox_status());
            Some(WorkspaceTools::with_command(
                workspace,
                definition.tool_limits.clone(),
                definition.write_limits.unwrap_or_default(),
                tools.clone(),
                settings,
                Some(command),
            ))
        } else {
            Some(WorkspaceTools::with_write_limits(
                workspace,
                definition.tool_limits.clone(),
                definition.write_limits.unwrap_or_default(),
                tools,
            ))
        }
    };
    let CredentialRef::Environment(name) = &definition.credential;
    let api_key = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(AssemblyError::CredentialUnavailable)?;
    let gateway = gateway(
        definition.protocol_render_options()?,
        api_key,
        definition.endpoint.clone(),
        definition.max_model_attempts,
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

    let session = Session::new_with_tools(
        conversation_id,
        context,
        move |work, _control, reporter| {
            let gateway = gateway.clone();
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

pub(crate) fn gateway(
    render_options: RenderOptions,
    api_key: String,
    endpoint: Option<String>,
    max_attempts: Option<u32>,
) -> Arc<dyn ModelGateway> {
    macro_rules! gateway {
        ($kind:ty, $options:expr) => {{
            let gateway = <$kind>::new(api_key).with_render_options($options);
            let gateway = match endpoint {
                Some(endpoint) => gateway.with_endpoint(endpoint),
                None => gateway,
            };
            // The wrapper repeats the same request for transient and
            // rate-limit failures, and never after output became visible.
            match max_attempts.filter(|attempts| *attempts > 1) {
                Some(attempts) => {
                    Arc::new(RetryingGateway::new(gateway, RetryPolicy::new(attempts)))
                        as Arc<dyn ModelGateway>
                }
                None => Arc::new(gateway) as Arc<dyn ModelGateway>,
            }
        }};
    }
    match render_options {
        RenderOptions::Chat(options) => gateway!(OpenAiChatCompletionsGateway, options),
        RenderOptions::Responses(options) => gateway!(OpenAiResponsesGateway, options),
        RenderOptions::Anthropic(options) => gateway!(AnthropicMessagesGateway, options),
    }
}
