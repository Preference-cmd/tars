use std::{collections::HashMap, ffi::OsString, process::ExitCode};
use tars_core::{
    config::{Config, ConfigSource, DefinitionOverrides, parse_mcp, parse_tools},
    definition::{
        CommandNetwork, MAX_COMPACT_THRESHOLD, MIN_COMPACT_THRESHOLD, Provider, SessionDefinition,
    },
};

pub(super) type Values<'a> = HashMap<&'a str, &'a OsString>;

/// An invocation failure carrying the exit class promised by `--help`.
#[derive(Debug)]
pub(super) struct CliError {
    kind: CliErrorKind,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliErrorKind {
    Argument,
    Configuration,
}

impl CliError {
    pub(super) fn argument(message: impl Into<String>) -> Self {
        Self {
            kind: CliErrorKind::Argument,
            message: message.into(),
        }
    }

    pub(super) fn configuration(message: impl Into<String>) -> Self {
        Self {
            kind: CliErrorKind::Configuration,
            message: message.into(),
        }
    }

    pub(super) fn message(&self) -> &str {
        &self.message
    }

    pub(super) fn is_argument(&self) -> bool {
        self.kind == CliErrorKind::Argument
    }

    pub(super) fn exit_code(&self) -> ExitCode {
        match self.kind {
            CliErrorKind::Argument => ExitCode::from(2),
            CliErrorKind::Configuration => ExitCode::FAILURE,
        }
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        Self::argument(message)
    }
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self::argument(message)
    }
}

pub(super) fn string(values: &Values<'_>, key: &str) -> Result<Option<String>, CliError> {
    values
        .get(key)
        .map(|v| {
            v.to_str()
                .map(str::to_owned)
                .ok_or_else(|| CliError::argument("text option must be UTF-8"))
        })
        .transpose()
}

pub(super) fn load(values: &Values<'_>, disabled: bool) -> Result<Config, CliError> {
    let source = match (values.get("--config"), disabled) {
        (Some(_), true) => {
            return Err(CliError::argument(
                "--config and --no-config are mutually exclusive",
            ));
        }
        (Some(path), false) => ConfigSource::File((*path).into()),
        (None, true) => ConfigSource::Disabled,
        (None, false) => ConfigSource::Discover,
    };
    let config =
        Config::load(source).map_err(|error| CliError::configuration(error.to_string()))?;
    for warning in config.warnings() {
        eprintln!("{warning}");
    }
    Ok(config)
}

pub(super) fn definition(
    config: &Config,
    values: &Values<'_>,
) -> Result<SessionDefinition, CliError> {
    let provider = string(values, "--provider")?
        .map(|value| match value.as_str() {
            "openai-chat" => Ok(Provider::OpenAiChat),
            "openai-responses" => Ok(Provider::OpenAiResponses),
            "anthropic" => Ok(Provider::Anthropic),
            _ => Err(CliError::argument(
                "--provider must be openai-chat, openai-responses or anthropic",
            )),
        })
        .transpose()?;
    let number = |flag: &str| -> Result<Option<u64>, CliError> {
        string(values, flag)?
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| CliError::argument(format!("invalid numeric option: {flag}")))
            })
            .transpose()
    };
    let context_window = number("--context-window")?;
    if context_window == Some(0) {
        return Err(CliError::argument(
            "--context-window must be a positive token count",
        ));
    }
    // `on`/`off` set the switch alone; an integer sets the trigger percentage
    // and turns automatic compaction on.
    let (auto_compact, compact_threshold) = match string(values, "--auto-compact")?.as_deref() {
        None => (None, None),
        Some("on") => (Some(true), None),
        Some("off") => (Some(false), None),
        Some(value) => match value.parse::<u8>() {
            Ok(percent) if (MIN_COMPACT_THRESHOLD..=MAX_COMPACT_THRESHOLD).contains(&percent) => {
                (Some(true), Some(percent))
            }
            _ => {
                return Err(CliError::argument(
                    "--auto-compact must be on, off, or an integer percentage between 10 and 95",
                ));
            }
        },
    };
    let overrides = DefinitionOverrides {
        provider,
        workspace_root: values.get("--workspace").map(|path| (*path).into()),
        tools: string(values, "--tools")?
            .map(|value| parse_tools(&value).map_err(|error| CliError::argument(error.to_string())))
            .transpose()?,
        mcp: string(values, "--mcp")?
            .map(|value| parse_mcp(&value).map_err(|error| CliError::argument(error.to_string())))
            .transpose()?,
        tool_limits: None,
        write_limits: None,
        command_network: string(values, "--command-network")?
            .map(|value| match value.as_str() {
                "none" => Ok(CommandNetwork::None),
                "open" => Ok(CommandNetwork::Open),
                _ => Err(CliError::argument(
                    "--command-network must be none or open",
                )),
            })
            .transpose()?,
        permission_mode: string(values, "--permission-mode")?
            .map(|value| {
                value
                    .parse::<tars_core::permission::PermissionMode>()
                    .map_err(|_| {
                        CliError::argument(
                            "--permission-mode must be read-only, ask, accept-edits, or full-access",
                        )
                    })
            })
            .transpose()?,
        model: string(values, "--model")?,
        endpoint: string(values, "--endpoint")?,
        api_key_env: string(values, "--api-key-env")?,
        timeout_secs: number("--timeout-secs")?,
        capacity: number("--capacity")?
            .map(usize::try_from)
            .transpose()
            .map_err(|_| CliError::argument("capacity too large"))?,
        max_model_rounds: number("--max-model-rounds")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| CliError::argument("model round limit too large"))?,
        max_tool_calls: number("--max-tool-calls")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| CliError::argument("tool call limit too large"))?,
        context_window,
        auto_compact,
        compact_threshold,
        max_tokens: number("--max-tokens")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| CliError::argument("output token limit too large"))?,
        max_model_attempts: number("--max-model-attempts")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| CliError::argument("model attempt limit too large"))?,
        // Sent as written: accepted values differ by model and server.
        reasoning_effort: string(values, "--reasoning-effort")?,
        extra_body: string(values, "--extra-body")?
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| CliError::argument("--extra-body must be a JSON object"))
            })
            .transpose()?,
        render_options: string(values, "--render-options")?
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| CliError::argument("--render-options must be a JSON object"))
            })
            .transpose()?,
    };
    config
        .definition(string(values, "--profile")?.as_deref(), overrides)
        .map_err(|error| definition_error(error, values))
}

/// Config-file failures are configuration errors; an invalid value from this
/// command line stays an argument error. `Config::load` resolves every profile
/// first, so a definition error here can only come from an explicit override.
fn definition_error(error: tars_core::config::ConfigError, values: &Values<'_>) -> CliError {
    use tars_core::config::ConfigError;
    let explicit = match &error {
        ConfigError::UnknownMcpServer(_) | ConfigError::InvalidMcpServer(_) => {
            values.contains_key("--mcp")
        }
        ConfigError::InvalidTimeout => values.contains_key("--timeout-secs"),
        ConfigError::Workspace { .. } | ConfigError::EmptyPath => {
            values.contains_key("--workspace")
        }
        ConfigError::InvalidTools(_) => values.contains_key("--tools"),
        ConfigError::Definition(_) | ConfigError::ProviderRequired | ConfigError::ModelRequired => {
            true
        }
        _ => false,
    };
    let message = error.to_string();
    if explicit {
        CliError::argument(message)
    } else {
        CliError::configuration(message)
    }
}
