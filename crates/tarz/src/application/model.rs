use crate::session::{Session, SessionConfig};
use causa::kernel::{Context, ModelGateway, ModelRef};
use causa::providers::{
    AnthropicMessagesGateway, OpenAiChatCompletionsGateway, OpenAiResponsesGateway,
};
use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnRunOptions, TurnRunner};
use std::sync::Arc;

/// Supported wire protocols; model identifiers are supplied explicitly.
#[derive(Clone, Copy, Debug)]
pub enum Provider {
    /// OpenAI Chat Completions wire format.
    OpenAiChat,
    /// OpenAI Responses wire format.
    OpenAiResponses,
    /// Anthropic Messages wire format.
    Anthropic,
}

/// Explicit connection settings. No Debug implementation exposes the credential.
pub struct ModelConfig {
    /// Selected wire protocol.
    pub provider: Provider,
    /// Provider model identifier.
    pub model: String,
    /// Credential obtained by the entry point, never stored in materials.
    pub api_key: String,
    /// Full endpoint override, including its path.
    pub endpoint: Option<String>,
}

impl ModelConfig {
    /// Build an idle session with a real HTTP gateway and an empty tool catalog.
    /// Each accepted work gets fresh options; construction sends no requests.
    pub fn session(self, context: Context, config: SessionConfig) -> Session {
        macro_rules! gateway {
            ($kind:ty) => {{
                let gateway = <$kind>::new(self.api_key);
                Arc::new(match self.endpoint {
                    Some(endpoint) => gateway.with_endpoint(endpoint),
                    None => gateway,
                }) as Arc<dyn ModelGateway>
            }};
        }
        let gateway = match self.provider {
            Provider::OpenAiChat => gateway!(OpenAiChatCompletionsGateway),
            Provider::OpenAiResponses => gateway!(OpenAiResponsesGateway),
            Provider::Anthropic => gateway!(AnthropicMessagesGateway),
        };
        let executor = Arc::new(
            ToolExecutor::new(vec![], ToolExecutorOptions::default())
                .expect("empty tool catalog is valid"),
        );
        let runner = Arc::new(TurnRunner::new(gateway, executor));
        Session::new(
            uuid::Uuid::now_v7().to_string().into(),
            context,
            move |_, _| {
                let runner = runner.clone();
                let options = TurnRunOptions::new(ModelRef::new(self.model.clone()));
                async move { Ok((runner, options)) }
            },
            config,
        )
    }
}
