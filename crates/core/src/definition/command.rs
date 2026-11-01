use super::types::DefinitionError;
use serde::{Deserialize, Serialize};

/// Network access for the built-in command tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandNetwork {
    /// No outbound or inbound traffic, including loopback and DNS.
    #[default]
    None,
    /// Outbound traffic is allowed.
    Open,
}

impl CommandNetwork {
    /// Canonical kebab-case text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Open => "open",
        }
    }
}

impl std::fmt::Display for CommandNetwork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Frozen settings for the built-in command tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSettings {
    /// Network boundary for every command.
    #[serde(default)]
    pub network: CommandNetwork,
    /// Default command timeout in seconds when the model omits one.
    #[serde(default = "default_timeout")]
    pub default_timeout_secs: u64,
    /// Largest timeout a command may request, in seconds.
    #[serde(default = "maximum_timeout")]
    pub max_timeout_secs: u64,
}

impl Default for CommandSettings {
    fn default() -> Self {
        Self {
            network: CommandNetwork::None,
            default_timeout_secs: default_timeout(),
            max_timeout_secs: maximum_timeout(),
        }
    }
}

impl CommandSettings {
    /// Reject zero, inverted or above-hard-limit timeouts.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        if self.default_timeout_secs == 0
            || self.default_timeout_secs > self.max_timeout_secs
            || self.max_timeout_secs == 0
            || self.max_timeout_secs > maximum_timeout()
        {
            return Err(DefinitionError::InvalidCommandSettings);
        }
        Ok(())
    }
}

const fn default_timeout() -> u64 {
    120
}

const fn maximum_timeout() -> u64 {
    600
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_bounds_are_stable() {
        let settings = CommandSettings::default();
        assert_eq!(settings.network, CommandNetwork::None);
        assert_eq!(settings.default_timeout_secs, 120);
        assert_eq!(settings.max_timeout_secs, 600);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn zero_inverted_and_above_hard_limit_are_rejected() {
        for settings in [
            CommandSettings {
                default_timeout_secs: 0,
                ..CommandSettings::default()
            },
            CommandSettings {
                default_timeout_secs: 601,
                max_timeout_secs: 600,
                ..CommandSettings::default()
            },
            CommandSettings {
                max_timeout_secs: 601,
                ..CommandSettings::default()
            },
            CommandSettings {
                default_timeout_secs: 121,
                max_timeout_secs: 120,
                ..CommandSettings::default()
            },
        ] {
            assert_eq!(
                settings.validate(),
                Err(DefinitionError::InvalidCommandSettings)
            );
        }
    }
}
