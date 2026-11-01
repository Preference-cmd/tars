use causa::kernel::{ContextBlock, ControlError, EditError, InvocationId, TurnId};
use causa::runtime::TurnResult;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

/// Application-supplied conversation identity; this is not an access credential.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ConversationId(String);

impl ConversationId {
    /// Read the application's identity verbatim.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<String> for ConversationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl From<&str> for ConversationId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Explicit bounds for retained records and each accepted work's lifetime.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Maximum retained active and terminal records; zero rejects new work.
    pub retained_work_capacity: usize,
    /// Deadline measured from admission, including assembly, or no deadline.
    pub work_timeout: Option<Duration>,
}

/// A retained work's identity, scoped to its conversation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkRef {
    /// Conversation supplied to the owning session.
    pub conversation_id: ConversationId,
    /// Independently allocated UUIDv7 execution identity.
    pub turn_id: TurnId,
}

/// Advisory model progress scoped to an owning session instance and work.
/// Subscribe before submission to observe from the start; delivery remains
/// bounded and may lag or truncate. Late receivers have no history. This value
/// is never committed or persisted as material.
#[derive(Clone, Debug)]
pub struct WorkProgress {
    /// Work whose current model invocation produced this observation.
    pub work: WorkRef,
    /// Logical invocation within that work, including its model round.
    pub invocation_id: InvocationId,
    /// Bounded preview or model lifecycle notification.
    pub update: ProgressUpdate,
}

/// Nonterminal observations for a model preview, independent of work/save state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgressUpdate {
    /// A model request is ready; reset any previous round's preview.
    ModelStarted,
    /// Borrowed model text copied into a bounded, temporary preview.
    Text {
        /// At most 16 KiB of the incoming fragment, cut at a UTF-8 boundary.
        delta: String,
        /// The original fragment exceeded this observation's text bound.
        truncated: bool,
    },
    /// Complete output accepted by Causa, before material conversion/commit.
    /// This is not a successful work result or a save confirmation.
    ModelOutputReady,
    /// A bounded tool-stage observation; reliable snapshots remain on work observation.
    Tool(super::ToolProgress),
    /// Advisory context-compaction lifecycle; reliable facts remain the Context
    /// returned by the work and the saved record.
    Compaction(crate::compact::CompactionProgress),
}

/// One retained work's release facts, without its original materials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedWork {
    /// The retained work.
    pub work: WorkRef,
    /// Whether a caller has obtained its terminal result.
    pub read: bool,
    /// Whether the work is terminal.
    pub finished: bool,
    /// Whether an unclaimed uncommitted tool batch is still attached.
    pub uncommitted: bool,
}

/// Stable admission receipt, independent of later execution state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkReceipt {
    /// The accepted work, also returned on exact retained-key replay.
    pub work: WorkRef,
}

/// An idempotency key with the complete, ordered input to append atomically.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmitRequest {
    /// Caller-supplied key retained until explicit forgetting.
    pub request_key: String,
    /// Original identified blocks; an empty vector runs the current materials.
    pub blocks: Vec<ContextBlock>,
}

/// A refusal retaining every submitted input block.
#[derive(Debug, thiserror::Error)]
#[error("work rejected: {reason}")]
pub struct SubmitRejection {
    /// Structured admission failure.
    #[source]
    pub reason: SessionError,
    /// The original request, including all failed append inputs.
    pub request: SubmitRequest,
}

/// Snapshot whose terminal payload stays valid after owner edits or forgetting.
#[derive(Clone, Debug)]
pub struct WorkObservation {
    /// Identity of this snapshot's work.
    pub work: WorkRef,
    /// Current state or immutable terminal result.
    pub state: WorkState,
    /// Bounded actual tool facts, including returned but uncommitted results.
    /// Terminal snapshots are stable and do not consume the original batch.
    pub tools: Arc<super::ToolProgressSnapshot>,
}

/// Observable progress; scheduling may skip intermediate observations.
#[derive(Clone, Debug)]
pub enum WorkState {
    /// Inputs committed and work registered, before worker processing.
    Accepted,
    /// Assembly or the Causa runner is processing this work.
    Running,
    /// Frozen terminal value; materials were returned before publication.
    Finished(Arc<WorkResult>),
}

/// Original Causa or assembly result, frozen once selected.
// The whole result is shared by Arc; retain the original Causa value without
// an additional allocation solely to equalize enum variant sizes.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum WorkResult {
    /// Original result of the completed Causa run, retaining typed failed inputs.
    Run(TurnResult),
    /// Original assembly error, preserving source and downcast support.
    AssemblyFailed(Box<dyn Error + Send + Sync>),
    /// Cancellation or deadline selected before starting the runner.
    StoppedBeforeRun(ControlError),
    /// Abnormal task termination; lost materials are not fabricated.
    Faulted(WorkerFault),
}

/// Supervised task failure distinguishable from cooperative cancellation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkerFault {
    /// Worker unwound before returning its owned materials.
    #[error("worker panicked: {message}")]
    Panicked {
        /// Panic payload when textual, otherwise an explicit description.
        message: String,
    },
    /// Worker task was aborted while its supervisor remained operational.
    #[error("worker task was cancelled abnormally")]
    Cancelled,
}

/// Structured session and admission failures.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// Another work exclusively owns the context.
    #[error("session is busy with {work:?}")]
    Busy {
        /// Work currently holding the running slot.
        work: WorkRef,
    },
    /// Owner closed admission to new keys.
    #[error("session admission is closed")]
    Closed,
    /// Worker failure or poisoned owner access prevents new execution.
    #[error("session is faulted")]
    Faulted,
    /// Retained active and terminal record count reached its bound.
    #[error(
        "retained work capacity {capacity} reached; unread or unclaimed finished work cannot be released, so reopen the session or claim/adopt pending batches"
    )]
    Capacity {
        /// Configured record bound.
        capacity: usize,
    },
    /// Retained key was reused with different ordered blocks.
    #[error("request key conflicts with {work:?}")]
    Conflict {
        /// Original work bound to this key.
        work: WorkRef,
    },
    /// Atomic append rejected the input while returning it in the rejection.
    #[error("context edit failed: {0}")]
    ContextEdit(#[source] EditError),
    /// A generated execution identity collided with a retained record.
    #[error("candidate identity collides with {work:?}")]
    IdentityCollision {
        /// Existing retained work sharing the candidate identity.
        work: WorkRef,
    },
    /// No current Tokio runtime with an enabled time driver is available.
    #[error("a Tokio runtime with time enabled is required")]
    RuntimeUnavailable,
    /// The configured admission-relative absolute deadline cannot be represented.
    #[error("work deadline is not representable")]
    DeadlineUnrepresentable,
    /// Unknown, forgotten, or differently scoped work.
    #[error("work not found: {work:?}")]
    NotFound {
        /// Reference supplied by the caller.
        work: WorkRef,
    },
    /// Owner attempted terminal-only access before completion.
    #[error("work is not finished: {work:?}")]
    NotFinished {
        /// Active work supplied by the caller.
        work: WorkRef,
    },
    /// A worker lost the context on abnormal termination.
    #[error("context is unavailable")]
    ContextUnavailable,
}
