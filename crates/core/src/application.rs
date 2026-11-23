//! Shared instance ownership, supervised work, controlled materials and persistence.

mod execution;

pub use execution::text_input;

mod closing;
mod entry;
mod host;
mod managed;
mod opening;
mod outcome;
mod repair;
mod saving;
mod supervisor;
mod types;

pub use crate::assembly::AssemblyError;
pub use crate::mcp::McpStartError;
pub use crate::store::{CommitStage, StoreError};
pub use crate::usage::{
    CallCounts, CallUsage, LastCall, PurposeUsage, SessionUsage, UsageFigures, UsagePurpose,
    UsageTotals, WorkUsage, session_usage_lines,
};
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
    MaterialVersion, OpenReport, PreparedCreate, SaveStatus, SavedSessionInspection,
    SavedSessionSummary, SessionListing, SessionStatus, SessionTarget, WorkspaceRulesReport,
};

#[cfg(test)]
mod measurements;
#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod roundtrip_tests;
