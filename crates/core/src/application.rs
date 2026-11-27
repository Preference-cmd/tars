//! Shared instance ownership, supervised work, controlled materials and persistence.

mod execution;

pub use execution::{multimodal_input, text_input};

mod closing;
mod coordinator;
mod entry;
mod host;
mod managed;
mod opening;
mod outcome;
pub(crate) mod repair;
mod saving;
mod supervisor;
mod types;

pub use crate::assembly::AssemblyError;
pub use crate::mcp::McpStartError;
pub use crate::store::{
    CommitStage, LegacyImportSummary, MAX_SESSION_TITLE_CHARS, StoreError, validate_session_title,
};
pub use crate::usage::{
    CallCounts, CallUsage, LastCall, PurposeUsage, SessionUsage, UsageFigures, UsagePurpose,
    UsageTotals, WorkUsage, session_usage_lines,
};
pub use coordinator::{
    CoordinatorConfig, CoordinatorError, DEFAULT_OPERATION_TIMEOUT_SECS, MessageAgentRequest,
    OperationOutcome, OperationReceipt, ReadAgentResult, SpawnAgentOutcome, SpawnAgentRequest,
};
pub(crate) use entry::Entry;
pub use host::SessionHost;
pub use managed::ManagedSession;
pub use outcome::WorkOutcome;
pub use repair::{
    OriginalBatchPreview, OriginalResultPreview, RepairDiagnosis, RepairError, RepairInspection,
    RepairIssue, RepairIssueKind, RepairMode, RepairPlan, RepairSummary, RepairWarningKind,
    RepairWriteWarning,
};
pub use types::{
    ApplicationError, CloseOutcome, ClosePolicy, CommitDisposition, CompactionSettings,
    ContextUsage, HostOptions, Lifecycle, ManagedSubmitRejection, MaterialSnapshot,
    MaterialVersion, OpenReport, PreparedCreate, SaveStatus, SavedAgentInspection,
    SavedAgentSummary, SavedOperationSummary, SavedSessionInspection, SavedSessionSummary,
    SessionListing, SessionStatus, SessionTarget, WorkspaceRulesReport,
};

#[cfg(test)]
mod measurements;
#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod roundtrip_tests;
