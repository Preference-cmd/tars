//! Permission mode definitions and parsing.

use serde::{Deserialize, Serialize};

/// The execution permission mode controlling which tool calls require approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Read-only mode: only read tools execute; write tools are refused.
    ReadOnly,
    /// Ask mode: write tools require approval before execution.
    #[default]
    Ask,
    /// Accept edits mode: workspace write tools execute without asking.
    AcceptEdits,
    /// Full access mode: all tools execute without asking.
    FullAccess,
}

impl PermissionMode {
    /// Returns the canonical kebab-case string slice for this mode.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::FullAccess => "full-access",
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Error returned when parsing an unrecognized permission mode string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown permission mode: '{0}' (expected 'read-only', 'ask', 'accept-edits', or 'full-access')"
)]
pub struct ParsePermissionModeError(pub String);

impl std::str::FromStr for PermissionMode {
    type Err = ParsePermissionModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read-only" => Ok(Self::ReadOnly),
            "ask" => Ok(Self::Ask),
            "accept-edits" => Ok(Self::AcceptEdits),
            "full-access" => Ok(Self::FullAccess),
            other => Err(ParsePermissionModeError(other.to_owned())),
        }
    }
}
