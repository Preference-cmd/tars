use causa::kernel::{BlockId, InvocationId, ToolResultStatus};

/// Observed execution/material phase, independent of work completion and saving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPhase {
    /// Declared or waiting for an operation permit; not proof of an actual start.
    Waiting,
    /// The operation acquired its permits and actually began.
    Running,
    /// An accepted tool result exists, but has not been committed as material.
    Returned,
    /// Causa committed the actual tool result to the returned context.
    Committed,
    /// Processing stopped before the application operation began, or while the
    /// declaration was still pending in the runner.
    NotStarted,
    /// A started operation has no confirmed result after abnormal termination.
    Unknown,
}

/// Small observed facts for one declaration; this is not the complete tool result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolProgress {
    /// Model round that declared the tool.
    pub invocation_id: InvocationId,
    /// Original declaration identity, including for repeated arguments.
    pub call_block_id: BlockId,
    /// At most 128 UTF-8 bytes of the effective tool name.
    pub tool_name: String,
    /// At most 256 UTF-8 bytes of JSON-formatted arguments, possibly abbreviated.
    pub arguments_summary: String,
    /// Actual observed stage; committed does not mean saved.
    pub phase: ToolPhase,
    /// Accepted result status when known, including unknown outcomes.
    pub status: Option<ToolResultStatus>,
    /// After settlement, the application can prove no file operation began.
    /// False alone does not prove a start. This can refine a conservative
    /// runtime UnknownOutcome without modifying the original result or batch.
    pub operation_not_started: bool,
}

/// Bounded state available through reliable work observation even after event loss.
///
/// At most 128 declarations are retained. This snapshot does not own complete
/// outputs, consume an uncommitted batch, or restore execution after reopening.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolProgressSnapshot {
    /// Retained declarations in their first observed order.
    pub calls: Vec<ToolProgress>,
    /// Additional declarations exceeded the observation bound.
    pub truncated: bool,
}
