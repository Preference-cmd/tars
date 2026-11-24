use crate::compact::estimate::{UsageTracker, UsageTrackingGateway};
use crate::compact::preparer::{CompactionPreparer, TransientTrimPreparer};
use crate::compact::{CompactionRuntime, ProtectedInputs};
use crate::definition::{CredentialRef, DefinitionError, RenderOptions, SessionDefinition};
use crate::external_tools::ExternalTools;
use crate::mcp::McpStartError;
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

/// Typed session assembly failure before any model call is made.
#[derive(Clone, Debug, thiserror::Error)]
pub enum AssemblyError {
    /// Session definition failed validation.
    #[error("invalid session definition: {0}")]
    Definition(#[from] DefinitionError),
    /// Configured credential environment variable is missing, non-Unicode or blank.
    #[error("credential environment variable is missing, non-Unicode or blank")]
    CredentialUnavailable,
    /// Workspace root could not be opened or validated.
    #[error("workspace resource unavailable: {0}")]
    Workspace(#[source] Arc<WorkspaceError>),
    /// Command tool or its platform sandbox is unavailable.
    #[error("command tool unavailable: {0}")]
    CommandUnavailable(String),
    /// Search tool could not be bound.
    #[error("search tool unavailable: {0}")]
    SearchUnavailable(String),
    /// Required MCP server failed to start.
    #[error("{0}")]
    McpStart(#[source] Arc<McpStartError>),
}

impl From<McpStartError> for AssemblyError {
    fn from(error: McpStartError) -> Self {
        Self::McpStart(Arc::new(error))
    }
}

/// Host resources an assembly uses, plus the store path a command must not read.
pub(crate) struct SessionResources {
    pub(crate) tools: Arc<ToolResources>,
    /// Sessions directory; the command tool must not read it and history lives
    /// beside the records.
    pub(crate) storage: Option<std::path::PathBuf>,
    /// Restored history index of a reopened session.
    pub(crate) history: Vec<crate::compact::HistoryRecordEntry>,
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
    /// Initial session-wide usage statistics restored from a saved record.
    pub(crate) initial_usage: Option<crate::usage::SessionUsage>,
    /// Shared session skill registry bound to `load_skill`.
    pub(crate) skill_registry: Option<Arc<std::sync::RwLock<crate::skills::SkillRegistry>>>,
    /// Initial layered command approval rules loaded for this session.
    pub(crate) command_rules: crate::permission::CommandRuleSet,
    /// Optional workspace rules persistence backend for safe `.tars/permissions.toml` appending.
    pub(crate) workspace_rules_backend: Option<crate::permission::WorkspaceRulesBackend>,
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
            initial_usage: None,
            skill_registry: None,
            command_rules: crate::permission::CommandRuleSet::new(),
            workspace_rules_backend: None,
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

    /// Restore the reopened session's cumulative usage statistics.
    pub(crate) fn with_usage(mut self, usage: Option<crate::usage::SessionUsage>) -> Self {
        self.initial_usage = usage;
        self
    }

    /// Attach the session-shared skill registry for `load_skill`.
    pub(crate) fn with_skills(
        mut self,
        skill_registry: Arc<std::sync::RwLock<crate::skills::SkillRegistry>>,
    ) -> Self {
        self.skill_registry = Some(skill_registry);
        self
    }

    /// Attach the initial command approval rule set loaded when opening the session.
    pub(crate) fn with_command_rules(
        mut self,
        command_rules: crate::permission::CommandRuleSet,
    ) -> Self {
        self.command_rules = command_rules;
        self
    }

    /// Attach the workspace rules persistence backend when a workspace and Store are active.
    pub(crate) fn with_workspace_rules_backend(
        mut self,
        backend: Option<crate::permission::WorkspaceRulesBackend>,
    ) -> Self {
        self.workspace_rules_backend = backend;
        self
    }

    pub(crate) fn with_storage(mut self, storage: Option<std::path::PathBuf>) -> Self {
        self.storage = storage;
        self
    }

    /// Restore the reopened session's history index and running values.
    pub(crate) fn with_compaction(
        mut self,
        history: Vec<crate::compact::HistoryRecordEntry>,
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

/// Assembled session owner paired with its application-level permission hub.
pub(crate) struct AssembledSession {
    pub(crate) session: Session,
    pub(crate) permissions: Arc<crate::permission::PermissionHub>,
}

pub(crate) fn assemble(
    definition: &SessionDefinition,
    conversation_id: ConversationId,
    context: Context,
    resources: SessionResources,
    current_mode: crate::policy::PermissionMode,
    mcp: Option<Arc<crate::mcp::McpRuntime>>,
    allow_unavailable_command: bool,
) -> Result<AssembledSession, AssemblyError> {
    let SessionResources {
        tools,
        storage,
        history,
        auto_compact,
        compact_threshold,
        persistent,
        external_tools,
        initial_usage,
        skill_registry,
        command_rules,
        workspace_rules_backend,
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
                    root,
                    settings,
                    sandbox,
                    storage.clone(),
                ),
                None => crate::tools::CommandRuntime::bind(root, settings, storage.clone()),
            };
            #[cfg(not(test))]
            let command = crate::tools::CommandRuntime::bind(root, settings, storage.clone());
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
        let workspace_tools = match (command, search) {
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
        }
        .with_storage(root, storage.as_deref());
        Some(workspace_tools)
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
    let usage_hub = Arc::new(match initial_usage {
        Some(usage) => crate::usage::UsageHub::from_session_usage(usage),
        None => crate::usage::UsageHub::new(),
    });
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
    let compaction = CompactionRuntime::new_strict(
        definition,
        &conversation_id,
        persistent
            .then_some(storage_for_history.as_deref())
            .flatten(),
        history,
        auto_compact,
        compact_threshold,
        summary_gateway,
        usage.clone(),
    );
    if let Some(runtime) = &compaction {
        runtime.attach_usage_hub(usage_hub.clone());
    }
    let transient_trim = (compaction.is_none())
        .then_some(definition.context_window)
        .flatten()
        .map(|window| (window, Arc::new(ProtectedInputs::default())));
    let options = definition.run_options();
    let max_tokens = definition.max_tokens;
    let skill_registry = skill_registry
        .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(crate::skills::SkillRegistry::new())));
    let loaded_skills = Arc::new(crate::skills::LoadedSkillsState::new());
    loaded_skills.sync_from_context(context.blocks());
    let skills_enabled = definition.prelude.skills_enabled;
    let selected = definition.tools.clone();
    let has_persistent_compaction = compaction.is_some();
    let readonly_workspace_tools: Vec<crate::definition::WorkspaceTool> = selected
        .iter()
        .copied()
        .filter(|tool| {
            matches!(
                tool,
                crate::definition::WorkspaceTool::ListDirectory
                    | crate::definition::WorkspaceTool::ReadFile
                    | crate::definition::WorkspaceTool::SearchText
            )
        })
        .collect();
    let code_execution_bindings = crate::tools::code_execution::BuiltinBindings::from_session(
        &readonly_workspace_tools,
        has_persistent_compaction,
    );
    let code_execution = definition
        .code_execution
        .filter(|_| code_execution_bindings.any() || mcp.is_some());
    let call_timeout_secs = definition.tool_limits.call_timeout_secs;
    let builtin_timeout = (!selected.is_empty() || has_persistent_compaction || skills_enabled)
        .then(|| std::time::Duration::from_secs(call_timeout_secs));
    let builtin_timeout = match (
        builtin_timeout,
        command_settings
            .map(|settings| std::time::Duration::from_secs(settings.max_timeout_secs + 5)),
    ) {
        (Some(builtin), Some(command)) => Some(builtin.max(command)),
        (builtin, command) => builtin.or(command),
    };
    let builtin_timeout = match (
        builtin_timeout,
        code_execution.map(|settings| std::time::Duration::from_secs(settings.timeout_secs + 5)),
    ) {
        (Some(builtin), Some(ce)) => Some(builtin.max(ce)),
        (builtin, ce) => builtin.or(ce),
    };
    // The executor deadline applies to every call, built-in and MCP alike.
    // Each tool narrows it to its own frozen deadline (built-in limits or the
    // server's `tool_timeout_sec`), so the shared value is the largest.
    let mcp_timeout = mcp.as_ref().and_then(|mcp| mcp.max_tool_timeout());
    let call_timeout = match (builtin_timeout, mcp_timeout) {
        (Some(builtin), Some(mcp)) => Some(builtin.max(mcp)),
        (builtin, mcp) => builtin.or(mcp),
    };
    let permissions = Arc::new(crate::permission::PermissionHub::with_rules_and_backend(
        current_mode,
        command_rules,
        workspace_rules_backend,
    ));
    let view = permissions.view();
    #[cfg(test)]
    let test_processors = Arc::new(std::sync::Mutex::new(
        Vec::<Arc<dyn ToolBatchProcessor>>::new(),
    ));
    #[cfg(test)]
    let test_processors_capture = test_processors.clone();
    #[cfg(test)]
    let test_after_prepare_hook =
        Arc::new(std::sync::Mutex::new(None::<Arc<dyn Fn() + Send + Sync>>));
    #[cfg(test)]
    let test_after_prepare_hook_capture = test_after_prepare_hook.clone();
    #[cfg(test)]
    let tools_for_session = workspace_tools.clone();

    let compaction_for_work = compaction.clone();
    let transient_trim_for_work = transient_trim.clone();
    let usage_for_trim = usage;
    let skill_registry_for_work = skill_registry.clone();
    let loaded_skills_for_work = loaded_skills.clone();
    let permissions_for_work = permissions.clone();
    let mcp_for_work = mcp.clone();
    let session = Session::new_with_tools(
        conversation_id,
        context,
        move |work, _control, reporter| {
            let gateway = gateway.clone();
            let compact_for_work = compaction_for_work.clone();
            let transient_trim_for_work = transient_trim_for_work.clone();
            let usage_for_trim = usage_for_trim.clone();
            let reporter_for_compaction = reporter.clone();
            let skill_registry_for_work = skill_registry_for_work.clone();
            let loaded_skills_for_work = loaded_skills_for_work.clone();
            let options = options
                .clone()
                .with_processor_data(causa::kernel::ProcessorData::new(view.clone()));
            let mut tools: Vec<Arc<dyn causa::kernel::Tool>> = Vec::new();
            if let Some(wt) = &workspace_tools {
                tools.extend(wt.bind_work(&selected, reporter.clone()));
            }
            let recall_tool = compact_for_work.as_ref().map(|runtime| {
                let recall = Arc::new(crate::compact::RecallTool::new(
                    runtime.clone(),
                    reporter.clone(),
                    call_timeout_secs,
                ));
                tools.push(recall.clone());
                recall
            });
            if let Some(ce_settings) = code_execution {
                let mut host = crate::tools::code_execution::SubcallHost::new(
                    ce_settings,
                    code_execution_bindings,
                    workspace_tools.clone(),
                    recall_tool,
                );
                if let Some(mcp_runtime) = &mcp_for_work {
                    host = host.with_extra_provider(mcp_runtime.clone());
                }
                let run_code = Arc::new(crate::tools::code_execution::RunCodeTool::new(
                    host,
                    ce_settings,
                    reporter.clone(),
                ));
                tools.push(run_code);
            }
            let bind_load_skill = skills_enabled
                && !skill_registry_for_work
                    .read()
                    .expect("skill registry lock")
                    .is_empty();
            if bind_load_skill {
                let load_skill = Arc::new(crate::skills::LoadSkillTool::new(
                    skill_registry_for_work,
                    loaded_skills_for_work.clone(),
                    reporter.clone(),
                    call_timeout_secs,
                ));
                tools.push(load_skill);
            }
            let mut before: Vec<Arc<dyn ToolBatchProcessor>> = Vec::new();
            let orchestrator = crate::permission::ToolPolicyProcessor::new(
                work,
                reporter,
                permissions_for_work.clone(),
            );
            #[cfg(test)]
            let orchestrator = match test_after_prepare_hook_capture
                .lock()
                .expect("test after prepare hook lock")
                .clone()
            {
                Some(hook) => orchestrator.with_after_prepare_hook(hook),
                None => orchestrator,
            };
            before.push(Arc::new(orchestrator));
            let mcp = mcp_for_work.clone();
            #[cfg(test)]
            {
                let extras = test_processors_capture
                    .lock()
                    .expect("test processors lock");
                before.extend(extras.iter().cloned());
            }
            let mut after: Vec<Arc<dyn ToolBatchProcessor>> = Vec::new();
            let mut artifact_store: Option<Arc<dyn causa::kernel::ArtifactStore>> = None;
            if let Some(runtime) = &compact_for_work
                && let Some(artifacts) = runtime.artifacts()
            {
                let store: Arc<dyn causa::kernel::ArtifactStore> = Arc::new(artifacts);
                after.push(Arc::new(crate::compact::OutputSpillProcessor::new(
                    store.clone(),
                )));
                artifact_store = Some(store);
            }
            if bind_load_skill {
                after.push(Arc::new(crate::skills::LoadedSkillTrackerProcessor::new(
                    loaded_skills_for_work.clone(),
                )));
            }
            async move {
                let executor = ToolExecutor::new(tools, {
                    let mut options = ToolExecutorOptions::default();
                    options.call_timeout = call_timeout;
                    options.before = before;
                    options.after = after;
                    options.artifact_store = artifact_store;
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
                    options = options.with_preparer(Arc::new(
                        CompactionPreparer::new(runtime, reporter_for_compaction)
                            .with_loaded_skills(loaded_skills_for_work),
                    ));
                } else if let Some((window, protected)) = transient_trim_for_work {
                    options = options.with_preparer(Arc::new(
                        TransientTrimPreparer::new(
                            window,
                            max_tokens,
                            usage_for_trim,
                            protected,
                            reporter_for_compaction,
                        )
                        .with_loaded_skills(loaded_skills_for_work),
                    ));
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
    .with_skills(skill_registry, loaded_skills);
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
        None => match transient_trim {
            Some((_, protected)) => session.with_protected_inputs(protected),
            None => session,
        },
    };
    let session = session.with_usage(usage_hub);

    #[cfg(test)]
    let session = {
        let mut session = session;
        session.test_before_processors = test_processors;
        session.test_after_prepare_hook = test_after_prepare_hook;
        session.workspace_tools = tools_for_session;
        session
    };

    Ok(AssembledSession {
        session,
        permissions,
    })
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
