use super::file::ConfigError;
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use serde::Deserialize;

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
    };
    definition.validate().map_err(ConfigError::Definition)?;
    Ok(definition)
}
