//! Explicit, instance-bound material repair; never resumes or executes old work.

mod inspect;
mod plan;
mod types;

pub use types::{
    OriginalBatchPreview, OriginalResultPreview, RepairDiagnosis, RepairError, RepairInspection,
    RepairIssue, RepairIssueKind, RepairMode, RepairPlan, RepairSummary,
};

pub(super) use inspect::admission_check;

#[cfg(test)]
mod measurements;
