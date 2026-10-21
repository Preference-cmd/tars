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
}

pub(crate) fn assemble(
    definition: &SessionDefinition,
    conversation_id: ConversationId,
    context: Context,
    resources: Arc<ToolResources>,
) -> Result<Session, AssemblyError> {
    definition.validate()?;
    let workspace_tools = if definition.tools.is_empty() {
        None
    } else {
        let workspace = Workspace::open(
            definition
                .workspace_root
                .as_deref()
                .expect("validated root"),
        )
        .map_err(|error| AssemblyError::Workspace(Arc::new(error)))?;
        Some(WorkspaceTools::with_write_limits(
            workspace,
            definition.tool_limits.clone(),
            definition.write_limits.unwrap_or_default(),
            resources,
        ))
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
    let call_timeout = (!selected.is_empty())
        .then(|| std::time::Duration::from_secs(definition.tool_limits.call_timeout_secs));
    let permissions = Arc::new(crate::permission::PermissionHub::new(
        crate::permission::PermissionMode::Ask,
    ));
    let view = permissions.view();
    let approvals = permissions.approvals();
    let session_permissions = permissions.clone();
    #[cfg(test)]
    let test_processors = Arc::new(std::sync::Mutex::new(
        Vec::<Arc<dyn ToolBatchProcessor>>::new(),
    ));
    #[cfg(test)]
    let test_processors_capture = test_processors.clone();

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
            if let Some(wt) = &workspace_tools {
                before.push(Arc::new(crate::permission::PrecheckProcessor::new(
                    wt.clone(),
                    reporter.clone(),
                    prepared_writes.clone(),
                )));
                before.push(Arc::new(crate::permission::PermissionProcessor::new(
                    work,
                    reporter,
                    approvals.clone(),
                    prepared_writes,
                )));
            }
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
                Ok((
                    Arc::new(TurnRunner::new(gateway, Arc::new(executor))),
                    options,
                ))
            }
        },
        definition.session_config(),
    )
    .allow_readonly_repair()
    .with_permissions(session_permissions);

    #[cfg(test)]
    let session = {
        let mut session = session;
        session.test_before_processors = test_processors;
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
