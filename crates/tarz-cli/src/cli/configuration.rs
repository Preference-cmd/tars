use std::{collections::HashMap, ffi::OsString};
use tarz_core::{
    config::{Config, ConfigSource, DefinitionOverrides, parse_tools},
    definition::{Provider, SessionDefinition},
};

pub(super) type Values<'a> = HashMap<&'a str, &'a OsString>;

pub(super) fn string(values: &Values<'_>, key: &str) -> Result<Option<String>, String> {
    values
        .get(key)
        .map(|v| {
            v.to_str()
                .map(str::to_owned)
                .ok_or_else(|| "text option must be UTF-8".into())
        })
        .transpose()
}

pub(super) fn load(values: &Values<'_>, disabled: bool) -> Result<Config, String> {
    let source = match (values.get("--config"), disabled) {
        (Some(_), true) => return Err("--config and --no-config are mutually exclusive".into()),
        (Some(path), false) => ConfigSource::File((*path).into()),
        (None, true) => ConfigSource::Disabled,
        (None, false) => ConfigSource::Discover,
    };
    Config::load(source).map_err(|e| e.to_string())
}

pub(super) fn definition(
    config: &Config,
    values: &Values<'_>,
) -> Result<SessionDefinition, String> {
    let provider = string(values, "--provider")?
        .map(|value| match value.as_str() {
            "openai-chat" => Ok(Provider::OpenAiChat),
            "openai-responses" => Ok(Provider::OpenAiResponses),
            "anthropic" => Ok(Provider::Anthropic),
            _ => Err("--provider must be openai-chat, openai-responses or anthropic".to_owned()),
        })
        .transpose()?;
    let number = |flag: &str| -> Result<Option<u64>, String> {
        string(values, flag)?
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| format!("invalid numeric option: {flag}"))
            })
            .transpose()
    };
    let overrides = DefinitionOverrides {
        provider,
        workspace_root: values.get("--workspace").map(|path| (*path).into()),
        tools: string(values, "--tools")?
            .map(|value| parse_tools(&value).map_err(|e| e.to_string()))
            .transpose()?,
        tool_limits: None,
        model: string(values, "--model")?,
        endpoint: string(values, "--endpoint")?,
        api_key_env: string(values, "--api-key-env")?,
        timeout_secs: number("--timeout-secs")?,
        capacity: number("--capacity")?
            .map(usize::try_from)
            .transpose()
            .map_err(|_| "capacity too large")?,
        max_model_rounds: number("--max-model-rounds")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| "model round limit too large")?,
        max_tool_calls: number("--max-tool-calls")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| "tool call limit too large")?,
        max_tokens: number("--max-tokens")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| "output token limit too large")?,
        max_model_attempts: number("--max-model-attempts")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| "model attempt limit too large")?,
        reasoning_effort: string(values, "--reasoning-effort")?
            .map(|value| {
                serde_json::from_value(serde_json::Value::String(value)).map_err(|_| {
                    "--reasoning-effort must be none, minimal, low, medium, high or xhigh"
                        .to_owned()
                })
            })
            .transpose()?,
    };
    config
        .definition(string(values, "--profile")?.as_deref(), overrides)
        .map_err(|e| e.to_string())
}
