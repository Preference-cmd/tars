use super::types::AgentModelConfig;
use crate::definition::{
    ChildModelOverride, CredentialRef, Provider, SessionDefinition, Temperature,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Credential-free authorized model profile exposed to agent preset and call resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    /// Profile identifier from `[profiles.<name>]`.
    pub name: String,
    /// Wire protocol provider.
    pub provider: Provider,
    /// Provider model identifier.
    pub model: String,
    /// Optional explicit HTTP(S) endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Credential environment reference (never a plaintext secret).
    pub credential: CredentialRef,
    /// Optional per-call output token cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Optional reasoning effort string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Optional validated sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Temperature>,
    /// Optional top-level request extra fields.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    /// Optional model context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Optional total attempts on transient/rate-limit failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_attempts: Option<u32>,
    /// Optional protocol render options.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub render_options: serde_json::Map<String, serde_json::Value>,
}

/// Optional per-call `model` overrides accepted by `spawn_agent`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfigInput {
    /// Authorized named profile from `config.toml` (`[profiles.<name>]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Explicit provider model identifier override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Explicit reasoning effort override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Explicit sampling temperature override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Temperature>,
    /// Explicit output token cap override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Resolved effective model and generation parameters for a child agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveModelConfig {
    /// Effective wire protocol provider.
    pub provider: Provider,
    /// Effective model identifier.
    pub model: String,
    /// Effective endpoint URL.
    pub endpoint: Option<String>,
    /// Effective credential reference.
    pub credential: CredentialRef,
    /// Effective per-call output token cap.
    pub max_tokens: Option<u32>,
    /// Effective reasoning effort.
    pub reasoning_effort: Option<String>,
    /// Effective sampling temperature.
    pub temperature: Option<Temperature>,
    /// Effective extra top-level JSON request fields.
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    /// Effective model context window in tokens.
    pub context_window: Option<u64>,
    /// Effective attempt count on transient failure.
    pub max_model_attempts: Option<u32>,
    /// Effective protocol render options.
    pub render_options: serde_json::Map<String, serde_json::Value>,
}

/// Typed errors returned when resolving preset and call model overrides.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelResolutionError {
    /// Named profile was blank.
    #[error("model.profile must not be blank")]
    BlankProfile,
    /// Requested profile is not in the host's authorized profile map.
    #[error("unknown or unauthorized model profile '{0}'")]
    UnknownProfile(String),
    /// Explicit model identifier was blank.
    #[error("model.id must not be blank")]
    BlankModelId,
    /// Explicit reasoning effort was blank.
    #[error("model.reasoning_effort must not be blank")]
    BlankReasoningEffort,
    /// Zero `max_tokens` cannot produce a response.
    #[error("model.max_tokens must be positive when set")]
    InvalidMaxTokens,
    /// Temperature exceeds the target protocol's accepted range (`0.0..=1.0` for Anthropic).
    #[error("model.temperature exceeds Anthropic maximum of 1.0")]
    InvalidTemperature,
    /// Anthropic protocol requires a positive `max_tokens` value.
    #[error("Anthropic protocol requires max_tokens to be set")]
    AnthropicMaxTokensRequired,
}

/// Resolves preset + call `model` overrides against the parent definition and authorized profiles.
///
/// Switching profile updates provider, model, endpoint, credential, and profile generation defaults,
/// clears stale parent `render_options` when changing protocol, checks Anthropic `max_tokens` and
/// `temperature <= 1.0` invariants, and NEVER alters tools, workspace, or permission mode.
/// When `call_model` selects a different profile than `preset_model`, `preset_model`'s `id`,
/// `reasoning_effort`, `temperature`, and `max_tokens` are discarded so the new profile's defaults
/// apply cleanly underneath `call_model`'s own explicit overrides.
pub fn resolve_model_override(
    parent: &SessionDefinition,
    preset_model: Option<&AgentModelConfig>,
    call_model: Option<&ModelConfigInput>,
    authorized_profiles: &BTreeMap<String, ModelProfile>,
) -> Result<Option<ChildModelOverride>, ModelResolutionError> {
    let call_switches_profile = match (
        call_model.and_then(|c| c.profile.as_deref()),
        preset_model.and_then(|p| p.profile.as_deref()),
    ) {
        (Some(call_prof), Some(preset_prof)) => call_prof.trim() != preset_prof.trim(),
        (Some(_), None) => true,
        _ => false,
    };
    let effective_preset = if call_switches_profile {
        None
    } else {
        preset_model
    };

    let profile_name = call_model
        .and_then(|c| c.profile.as_deref())
        .or_else(|| effective_preset.and_then(|p| p.profile.as_deref()));
    let model_id = call_model
        .and_then(|c| c.id.as_deref())
        .or_else(|| effective_preset.and_then(|p| p.id.as_deref()));
    let reasoning_effort = call_model
        .and_then(|c| c.reasoning_effort.as_deref())
        .or_else(|| effective_preset.and_then(|p| p.reasoning_effort.as_deref()));
    let temperature = call_model
        .and_then(|c| c.temperature)
        .or_else(|| effective_preset.and_then(|p| p.temperature));
    let max_tokens = call_model
        .and_then(|c| c.max_tokens)
        .or_else(|| effective_preset.and_then(|p| p.max_tokens));

    if profile_name.is_none()
        && model_id.is_none()
        && reasoning_effort.is_none()
        && temperature.is_none()
        && max_tokens.is_none()
    {
        return Ok(None);
    }

    if let Some(name) = profile_name
        && name.trim().is_empty()
    {
        return Err(ModelResolutionError::BlankProfile);
    }
    if let Some(id) = model_id
        && id.trim().is_empty()
    {
        return Err(ModelResolutionError::BlankModelId);
    }
    if let Some(effort) = reasoning_effort
        && effort.trim().is_empty()
    {
        return Err(ModelResolutionError::BlankReasoningEffort);
    }
    if max_tokens == Some(0) {
        return Err(ModelResolutionError::InvalidMaxTokens);
    }

    let (effective_provider, effective_temperature, effective_max_tokens, override_out) =
        if let Some(profile_name) = profile_name {
            let profile = authorized_profiles
                .get(profile_name.trim())
                .ok_or_else(|| ModelResolutionError::UnknownProfile(profile_name.to_owned()))?;
            let final_model = model_id
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|| profile.model.clone());
            let final_effort = reasoning_effort
                .map(|s| s.trim().to_owned())
                .or_else(|| profile.reasoning_effort.clone());
            let final_temp = temperature.or(profile.temperature);
            let final_max_tokens = max_tokens.or(profile.max_tokens);
            let child_override = ChildModelOverride {
                provider: Some(profile.provider),
                model: Some(final_model),
                endpoint: Some(profile.endpoint.clone()),
                credential: Some(profile.credential.clone()),
                max_tokens: Some(final_max_tokens),
                reasoning_effort: Some(final_effort),
                temperature: Some(final_temp),
                extra_body: Some(profile.extra_body.clone()),
                context_window: Some(profile.context_window),
                max_model_attempts: Some(profile.max_model_attempts),
                render_options: Some(profile.render_options.clone()),
            };
            (
                profile.provider,
                final_temp,
                final_max_tokens,
                child_override,
            )
        } else {
            let final_temp = temperature.or(parent.temperature);
            let final_max_tokens = max_tokens.or(parent.max_tokens);
            let child_override = ChildModelOverride {
                provider: None,
                model: model_id.map(|s| s.trim().to_owned()),
                endpoint: None,
                credential: None,
                max_tokens: max_tokens.map(Some),
                reasoning_effort: reasoning_effort.map(|s| Some(s.trim().to_owned())),
                temperature: temperature.map(Some),
                extra_body: None,
                context_window: None,
                max_model_attempts: None,
                render_options: None,
            };
            (
                parent.provider,
                final_temp,
                final_max_tokens,
                child_override,
            )
        };

    if effective_provider == Provider::Anthropic {
        if effective_temperature.is_some_and(|t| t.as_f32() > 1.0) {
            return Err(ModelResolutionError::InvalidTemperature);
        }
        if effective_max_tokens.is_none() {
            return Err(ModelResolutionError::AnthropicMaxTokensRequired);
        }
    }

    Ok(Some(override_out))
}
