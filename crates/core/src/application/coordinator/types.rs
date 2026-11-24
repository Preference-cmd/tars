//! Public request, outcome, configuration, and error types for [`super::SessionCoordinator`].

use crate::application::ApplicationError;
use crate::application::repair::RepairError;
use crate::definition::{ChildNarrowingRequest, NarrowingError};
use crate::events::{
    AgentInteraction, DEFAULT_DELEGATION_MAX_DEPTH, DEFAULT_MAX_ACTIVE_CHILD_OPS_PER_SESSION,
    DEFAULT_MAX_ACTIVE_OPS_PER_AGENT, DEFAULT_MAX_AUTO_WAKEUPS, DEFAULT_SIBLING_DEBOUNCE_MS,
    OnParentComplete, OperationExecutionMode, OperationStatus, Utf8PageSlice,
};
use crate::session::SessionError;
use crate::store::StoreError;
use std::sync::Arc;
use std::time::Duration;

/// Default operation timeout in seconds when neither request nor parent sets a tighter bound.
pub const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 120;

/// Resource limits and scheduling parameters for a session's `SessionCoordinator`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinatorConfig {
    /// Maximum delegation depth (`operations.depth`) from the entry agent.
    pub max_delegation_depth: u32,
    /// Maximum concurrent active delegated operations initiated by a single agent.
    pub max_active_ops_per_agent: usize,
    /// Maximum concurrent active child operations across a single session.
    pub max_active_child_ops_per_session: usize,
    /// Maximum automatic idle wakeups per agent per user interaction cycle.
    pub max_auto_wakeups: usize,
    /// Debounce window while sibling finite background tasks remain active.
    pub sibling_debounce: Duration,
    /// Default operation timeout in seconds.
    pub default_operation_timeout_secs: u64,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            max_delegation_depth: DEFAULT_DELEGATION_MAX_DEPTH,
            max_active_ops_per_agent: DEFAULT_MAX_ACTIVE_OPS_PER_AGENT,
            max_active_child_ops_per_session: DEFAULT_MAX_ACTIVE_CHILD_OPS_PER_SESSION,
            max_auto_wakeups: DEFAULT_MAX_AUTO_WAKEUPS,
            sibling_debounce: Duration::from_millis(DEFAULT_SIBLING_DEBOUNCE_MS),
            default_operation_timeout_secs: DEFAULT_OPERATION_TIMEOUT_SECS,
        }
    }
}

/// Request to spawn a new child agent and admit its initial operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnAgentRequest {
    /// Canonical identifier of the calling agent.
    pub caller_agent_id: String,
    /// Optional calling operation identifier; `None` when called from a root turn.
    pub caller_operation_id: Option<String>,
    /// Optional source `ToolCall` block identifier for idempotent deduplication.
    pub source_block_id: Option<String>,
    /// Human-readable agent name.
    pub name: String,
    /// Optional preset name from which the agent was created.
    pub preset_name: Option<String>,
    /// Optional frozen system role prompt prepended to the child's initial Context.
    pub role_prompt: Option<String>,
    /// Initial user prompt to submit to the child agent.
    pub prompt: String,
    /// Monotonic capability and permission narrowing bounds (`child <= parent`).
    pub narrowing: ChildNarrowingRequest,
    /// Interaction lifetime policy (`SingleWork` or `Reusable`).
    pub interaction: AgentInteraction,
    /// Whether the caller awaits completion (`Sync`) or returns immediately (`Background`).
    pub execution_mode: OperationExecutionMode,
    /// Policy applied to this operation if its parent completes normally.
    pub on_parent_complete: OnParentComplete,
    /// Optional operation timeout override in seconds (`<= parent` when parent sets one).
    pub timeout_secs: Option<u64>,
    /// Optional model round ceiling override (`<= parent.max_model_rounds`).
    pub max_rounds: Option<u32>,
    /// Optional coalescing key attached to background completion notifications.
    pub coalesce_key: Option<String>,
}

/// Request to send follow-up instructions to an existing `Reusable` idle agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageAgentRequest {
    /// Canonical identifier of the calling agent.
    pub caller_agent_id: String,
    /// Optional calling operation identifier; `None` when called from a root turn.
    pub caller_operation_id: Option<String>,
    /// Optional source `ToolCall` block identifier for idempotent deduplication.
    pub source_block_id: Option<String>,
    /// Canonical identifier of the target `Reusable` agent.
    pub target_agent_id: String,
    /// Follow-up prompt to submit to the target agent.
    pub prompt: String,
    /// Whether the caller awaits completion (`Sync`) or returns immediately (`Background`).
    pub execution_mode: OperationExecutionMode,
    /// Policy applied to this operation if its parent completes normally.
    pub on_parent_complete: OnParentComplete,
    /// Optional operation timeout override in seconds (`<= parent` and `<= target` bounds).
    pub timeout_secs: Option<u64>,
    /// Optional model round ceiling override (`<= caller` and `<= target` bounds).
    pub max_rounds: Option<u32>,
    /// Optional coalescing key attached to background completion notifications.
    pub coalesce_key: Option<String>,
}

/// Immediate admission receipt returned for background operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationReceipt {
    /// Canonical identifier of the target agent executing the operation.
    pub agent_id: String,
    /// Canonical identifier of the admitted operation.
    pub operation_id: String,
    /// Operation status at admission or replay.
    pub status: OperationStatus,
    /// Whether this receipt came from idempotent source-block deduplication replay.
    pub replayed: bool,
}

/// Terminal archived outcome of a completed, cancelled, or failed operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationOutcome {
    /// Canonical identifier of the target agent that executed the operation.
    pub agent_id: String,
    /// Canonical identifier of the operation.
    pub operation_id: String,
    /// Terminal lifecycle status.
    pub status: OperationStatus,
    /// Full archived output text produced by the operation, if any.
    pub result_text: Option<String>,
    /// Total UTF-8 byte length of `result_text` (`0` when `None`).
    pub result_bytes: usize,
    /// Optional error message when the operation did not complete normally.
    pub error: Option<String>,
}

/// Result returned by [`crate::application::ManagedSession::spawn_agent`] or [`crate::application::ManagedSession::message_agent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnAgentOutcome {
    /// Returned immediately for `OperationExecutionMode::Background`.
    Background(OperationReceipt),
    /// Returned after operation settlement for `OperationExecutionMode::Sync`.
    Completed(OperationOutcome),
}

/// Read-only paginated output view returned by [`crate::application::ManagedSession::read_agent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadAgentResult {
    /// Queried operation identifier.
    pub operation_id: String,
    /// Target agent identifier that owns this operation.
    pub target_agent_id: String,
    /// Current operation lifecycle status.
    pub status: OperationStatus,
    /// UTF-8 safe page slice of the archived output text.
    pub page: Utf8PageSlice,
    /// Optional archived error detail if the operation failed or faulted.
    pub error: Option<String>,
}

/// Typed failures produced by multi-agent coordination, admission, isolation, and repair.
#[derive(Clone, Debug, thiserror::Error)]
pub enum CoordinatorError {
    /// Underlying session application lifecycle or admission error.
    #[error(transparent)]
    Application(#[from] ApplicationError),
    /// Capability or permission narrowing (`child <= parent`) failed.
    #[error(transparent)]
    Narrowing(#[from] NarrowingError),
    /// Persistent store failure.
    #[error("session store: {0}")]
    Store(#[from] Arc<StoreError>),
    /// Referenced agent does not exist in this session.
    #[error("unknown agent: {0}")]
    UnknownAgent(String),
    /// Referenced operation does not exist in this session.
    #[error("unknown operation: {0}")]
    UnknownOperation(String),
    /// An agent attempted to delegate to or message itself.
    #[error("agent cannot delegate to or message itself")]
    SelfDelegation,
    /// Delegation or messaging would form a cycle with an ancestor agent.
    #[error("delegation cycle detected")]
    DelegationCycle,
    /// Delegation depth exceeded the configured ceiling.
    #[error("delegation depth {depth} exceeds maximum {max_depth}")]
    DepthExceeded {
        /// Attempted operation depth.
        depth: u32,
        /// Configured session ceiling.
        max_depth: u32,
    },
    /// Caller agent reached its concurrent active delegated operation limit.
    #[error("agent active operation concurrency limit ({limit}) reached")]
    AgentConcurrencyLimit {
        /// Per-caller concurrency limit.
        limit: usize,
    },
    /// Session reached its concurrent active child operation limit.
    #[error("session active child operation concurrency limit ({limit}) reached")]
    SessionConcurrencyLimit {
        /// Session-wide concurrency limit.
        limit: usize,
    },
    /// Delegation prompt exceeded [`crate::events::MAX_PROMPT_BYTES`].
    #[error("prompt size ({bytes} bytes) exceeds limit ({limit} bytes)")]
    PromptTooLarge {
        /// Prompt byte length.
        bytes: usize,
        /// Configured byte limit.
        limit: usize,
    },
    /// Serialized operation input exceeded [`crate::events::MAX_OPERATION_INPUT_BYTES`].
    #[error("operation input size ({bytes} bytes) exceeds limit ({limit} bytes)")]
    InputTooLarge {
        /// Serialized input byte length.
        bytes: usize,
        /// Configured byte limit.
        limit: usize,
    },
    /// Matching `(session_id, parent_operation_id, source_block_id)` had different parameters.
    #[error("deduplication conflict with existing operation {existing_operation_id}")]
    DedupConflict {
        /// Existing operation occupying the source block key.
        existing_operation_id: String,
    },
    /// Target agent has `SingleWork` interaction policy and cannot accept follow-up messages.
    #[error("agent {0} is single_work and does not accept follow-up messages")]
    NotReusable(String),
    /// Target agent is currently busy executing, saving, or compacting.
    #[error("agent {agent_id} is busy with operation {active_operation_id}")]
    AgentBusy {
        /// Target agent identifier.
        agent_id: String,
        /// Active operation or barrier description.
        active_operation_id: String,
    },
    /// Target agent's Context requires structural repair before accepting new work.
    #[error("agent {0} needs repair before accepting new work")]
    AgentNeedsRepair(String),
    /// Caller's frozen capabilities do not cover the target agent's frozen capabilities.
    #[error("caller capabilities cannot invoke target agent: {0}")]
    CapabilityMismatch(String),
    /// `caller_operation_id` belongs to a different agent than `caller_agent_id`.
    #[error("operation belongs to agent {expected_agent_id}, not {actual_agent_id}")]
    CrossAgentOperation {
        /// Actual target agent of the operation.
        expected_agent_id: String,
        /// Supplied caller agent identifier.
        actual_agent_id: String,
    },
    /// Coordinated operation or waiter was cancelled.
    #[error("operation cancelled")]
    Cancelled,
}

impl From<StoreError> for CoordinatorError {
    fn from(error: StoreError) -> Self {
        Self::Store(Arc::new(error))
    }
}

impl From<SessionError> for CoordinatorError {
    fn from(error: SessionError) -> Self {
        Self::Application(ApplicationError::Session(error))
    }
}

impl From<RepairError> for CoordinatorError {
    fn from(error: RepairError) -> Self {
        Self::Application(ApplicationError::Repair(error))
    }
}
