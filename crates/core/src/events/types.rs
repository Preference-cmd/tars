//! Shared constants and core domain enums for the three-plane event bus.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// `BlockMeta.source` tag used when injecting queued `<task-notification>` messages.
pub const TASK_NOTIFICATION_SOURCE: &str = "tars:task-notification";

/// Default maximum inline body bytes for a single rendered `<task-notification>`.
pub const DEFAULT_NOTIFICATION_INLINE_MAX_BYTES: usize = 8 * 1024;

/// Maximum UTF-8 byte length returned by a single `read_agent` page.
pub const MAX_READ_AGENT_PAGE_BYTES: usize = 64 * 1024;

/// Maximum serialized UTF-8 byte length accepted for `operations.input_json`.
pub const MAX_OPERATION_INPUT_BYTES: usize = 128 * 1024;

/// Maximum UTF-8 byte length accepted for an agent delegation prompt.
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// Default automatic idle-wakeup budget per user interaction cycle.
pub const DEFAULT_MAX_AUTO_WAKEUPS: usize = 16;

/// Default debounce window (in milliseconds) while sibling finite background tasks remain active.
pub const DEFAULT_SIBLING_DEBOUNCE_MS: u64 = 50;

/// Default maximum delegation depth (`operations.depth`) from the entry agent.
pub const DEFAULT_DELEGATION_MAX_DEPTH: u32 = 1;

/// Default maximum concurrent active delegated operations initiated by a single agent.
pub const DEFAULT_MAX_ACTIVE_OPS_PER_AGENT: usize = 2;

/// Default maximum concurrent active child operations across a single session.
pub const DEFAULT_MAX_ACTIVE_CHILD_OPS_PER_SESSION: usize = 4;

/// Error returned when parsing an unrecognized [`AgentInteraction`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown agent interaction: '{0}' (expected 'single_work' or 'reusable')")]
pub struct ParseAgentInteractionError(pub String);

/// Interaction lifetime policy of a session agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInteraction {
    /// Agent accepts a single initial operation and does not accept follow-up `message_agent` calls.
    SingleWork,
    /// Agent retains its context and can accept follow-up `message_agent` operations when idle.
    Reusable,
}

impl AgentInteraction {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SingleWork => "single_work",
            Self::Reusable => "reusable",
        }
    }
}

impl fmt::Display for AgentInteraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentInteraction {
    type Err = ParseAgentInteractionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "single_work" => Ok(Self::SingleWork),
            "reusable" => Ok(Self::Reusable),
            other => Err(ParseAgentInteractionError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an unrecognized [`OperationExecutionMode`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown operation execution mode: '{0}' (expected 'sync' or 'background')")]
pub struct ParseOperationExecutionModeError(pub String);

/// Whether an operation blocks its caller or executes asynchronously in the background.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationExecutionMode {
    /// Caller awaits operation completion and receives output directly in `ToolResult`.
    Sync,
    /// Caller receives immediate admission metadata and results arrive via `agent_events`.
    Background,
}

impl OperationExecutionMode {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Background => "background",
        }
    }
}

impl fmt::Display for OperationExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OperationExecutionMode {
    type Err = ParseOperationExecutionModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sync" => Ok(Self::Sync),
            "background" => Ok(Self::Background),
            other => Err(ParseOperationExecutionModeError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an unrecognized [`OnParentComplete`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown on_parent_complete policy: '{0}' (expected 'continue' or 'cancel')")]
pub struct ParseOnParentCompleteError(pub String);

/// Policy applied to an active child operation when its parent completes normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnParentComplete {
    /// Allow the admitted child operation to continue running after the parent completes normally.
    Continue,
    /// Cancel the child operation (and prevent starting `creating` work) when the parent completes.
    Cancel,
}

impl OnParentComplete {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Cancel => "cancel",
        }
    }
}

impl fmt::Display for OnParentComplete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OnParentComplete {
    type Err = ParseOnParentCompleteError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "continue" => Ok(Self::Continue),
            "cancel" => Ok(Self::Cancel),
            other => Err(ParseOnParentCompleteError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an unrecognized [`OperationStatus`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown operation status: '{0}'")]
pub struct ParseOperationStatusError(pub String);

/// Lifecycle status of a persisted operation or supervised background task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    /// Admitted to storage and awaiting execution startup.
    Creating,
    /// Actively executing.
    Running,
    /// Finished normally with a completed outcome.
    Completed,
    /// Terminated by explicit or cascaded cancellation.
    Cancelled,
    /// Terminated due to a model or execution failure.
    Failed,
    /// Terminated because the operation deadline expired.
    Deadline,
    /// Terminated due to an unexpected worker/internal fault.
    Faulted,
    /// Interrupted by an unclean shutdown or reopen recovery.
    Interrupted,
}

impl OperationStatus {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Deadline => "deadline",
            Self::Faulted => "faulted",
            Self::Interrupted => "interrupted",
        }
    }

    /// Returns `true` when this status is final and must not be overwritten.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Creating | Self::Running)
    }

    /// Returns `true` when this status represents a non-terminal operation (`creating` or `running`).
    pub const fn is_active(self) -> bool {
        !self.is_terminal()
    }
}

impl fmt::Display for OperationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OperationStatus {
    type Err = ParseOperationStatusError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "creating" => Ok(Self::Creating),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            "deadline" => Ok(Self::Deadline),
            "faulted" => Ok(Self::Faulted),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(ParseOperationStatusError(other.to_owned())),
        }
    }
}
