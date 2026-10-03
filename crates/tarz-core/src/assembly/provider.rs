use crate::definition::{CredentialRef, DefinitionError, Provider, SessionDefinition};
use crate::session::{ConversationId, Session};
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
}

pub(crate) fn assemble(
    definition: &SessionDefinition,
    conversation_id: ConversationId,
    context: Context,
) -> Result<Session, AssemblyError> {
    definition.validate()?;
    let CredentialRef::Environment(name) = &definition.credential;
    let api_key = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(AssemblyError::CredentialUnavailable)?;
    let gateway = gateway(definition.provider, api_key, definition.endpoint.clone());
    let executor = Arc::new(
        ToolExecutor::new(vec![], ToolExecutorOptions::default())
            .expect("empty tool catalog is valid"),
    );
    Ok(assemble_with_runner(
        definition,
        conversation_id,
        context,
        Arc::new(TurnRunner::new(gateway, executor)),
    ))
}

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
