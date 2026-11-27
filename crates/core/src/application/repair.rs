//! Explicit, instance-bound material repair; never resumes or executes old work.

mod inspect;
mod plan;
mod types;

pub use types::{
    OriginalBatchPreview, OriginalResultPreview, RepairDiagnosis, RepairError, RepairInspection,
    RepairIssue, RepairIssueKind, RepairMode, RepairPlan, RepairSummary, RepairWarningKind,
    RepairWriteWarning,
};

pub(crate) use inspect::{admission_check, admission_check_with_media};
pub(super) use inspect::{diagnose, inspect_original_batch};
pub(super) use plan::{build_repair_plan, source_check};

#[cfg(test)]
mod measurements;
