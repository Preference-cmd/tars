use crate::definition::{CredentialRef, DefinitionError, Provider, SessionDefinition};
use crate::session::{ConversationId, Session};
use crate::tools::{ToolResources, WorkspaceTools};
use crate::workspace::{Workspace, WorkspaceError};
use causa::kernel::{Context, ModelGateway};
use causa::providers::{
    AnthropicMessagesGateway, OpenAiChatCompletionsGateway, OpenAiResponsesGateway,
};
use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnRunner};
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
        Some(WorkspaceTools::new(
            workspace,
            definition.tool_limits.clone(),
            resources,
        ))
    };
    let CredentialRef::Environment(name) = &definition.credential;
    let api_key = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(AssemblyError::CredentialUnavailable)?;
    let gateway = gateway(definition.provider, api_key, definition.endpoint.clone());
    let options = definition.run_options();
    let selected = definition.tools.clone();
    let call_timeout = (!selected.is_empty())
        .then(|| std::time::Duration::from_secs(definition.tool_limits.call_timeout_secs));
    Ok(Session::new_with_tools(
        conversation_id,
        context,
        move |_, _, reporter| {
            let gateway = gateway.clone();
            let options = options.clone();
            let tools = workspace_tools
                .as_ref()
                .map_or_else(Vec::new, |tools| tools.bind(&selected, reporter));
            async move {
                let executor = ToolExecutor::new(
                    tools,
                    ToolExecutorOptions {
                        call_timeout,
                        ..Default::default()
                    },
                )
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
                Ok((
                    Arc::new(TurnRunner::new(gateway, Arc::new(executor))),
                    options,
                ))
            }
        },
        definition.session_config(),
    ))
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
    provider: Provider,
    api_key: String,
    endpoint: Option<String>,
) -> Arc<dyn ModelGateway> {
    macro_rules! gateway {
        ($kind:ty) => {{
            let gateway = <$kind>::new(api_key);
            Arc::new(match endpoint {
                Some(endpoint) => gateway.with_endpoint(endpoint),
                None => gateway,
            }) as Arc<dyn ModelGateway>
        }};
    }
    match provider {
        Provider::OpenAiChat => gateway!(OpenAiChatCompletionsGateway),
        Provider::OpenAiResponses => gateway!(OpenAiResponsesGateway),
        Provider::Anthropic => gateway!(AnthropicMessagesGateway),
    }
}
