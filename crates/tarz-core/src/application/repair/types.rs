use crate::application::MaterialVersion;
use crate::session::WorkRef;
use causa::kernel::{BlockId, Context, ToolResultStatus};
use serde::Serialize;

pub(super) const DISPLAY_LIMIT: usize = 128;

/// Reason the current materials cannot form a supported self-contained request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairIssueKind {
    /// A declaration has no recorded result; its execution is not inferred.
    UnpairedDeclaration,
    /// A result has no earlier declaration.
    OrphanResult,
    /// More than one result answers the same declaration.
    DuplicateResult,
    /// A wire call identity is empty or reused.
    AmbiguousWireIdentity,
    /// The declaration has a blank name or non-object arguments.
    InvalidDeclaration,
    /// The application cannot resolve media or external artifacts for repair.
    UnsupportedReference,
    /// The selected public protocol renderer rejects the remaining structure.
    ProtocolStructure,
}

/// Which side a possible pre-interruption effect is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairWarningKind {
    /// A built-in write or edit call may have modified a workspace file.
    FileWrite,
    /// An MCP or other non-builtin call may have had effects on its own server,
    /// outside the workspace and outside the application's control.
    External,
}

/// Notification of a tool call that may have had a side effect before interruption.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepairWriteWarning {
    /// Identity of the tool call declaration block.
    pub call_block_id: BlockId,
    /// Name of the tool ("write_file", "edit_file", or an MCP/non-builtin name).
    pub tool_name: String,
    /// Target path from the call arguments, bounded to 512 UTF-8 bytes (empty if missing or invalid).
    pub path: String,
    /// Which side the possible effect is on: the workspace file or the tool's own server.
    pub kind: RepairWarningKind,
}
impl RepairWriteWarning {
    /// The advisory sentence for this warning, without layout decoration;
    /// surfaces add their own indentation and line punctuation. Names and paths
    /// come from model-provided arguments and are escaped as display text.
    pub fn advice(&self) -> String {
        let tool_name = crate::permission::escape_untrusted_text(&self.tool_name);
        let path = crate::permission::escape_untrusted_text(&self.path);
        match self.kind {
            RepairWarningKind::FileWrite if self.path.is_empty() => {
                format!("{tool_name} may have changed the file; read it again before continuing")
            }
            RepairWarningKind::FileWrite => format!(
                "{tool_name} {path} may have changed the file; read it again before continuing"
            ),
            RepairWarningKind::External => format!(
                "{tool_name} may have caused external side effects; check the server state before continuing"
            ),
        }
    }
}

/// One bounded diagnostic, with identities rather than copies of user content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepairIssue {
    /// Structured issue category.
    pub kind: RepairIssueKind,
    /// Affected block when one can be identified.
    pub block_id: Option<BlockId>,
    /// At most 512 UTF-8 bytes of diagnostic text.
    pub message: String,
}

/// Bounded display of a complete linear scan; total counts are never truncated.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RepairDiagnosis {
    /// Total blocks inspected.
    pub block_count: usize,
    /// Declarations without any matching result, irrespective of execution.
    pub unpaired_count: usize,
    /// Total issues found, including those omitted from `issues`.
    pub issue_count: usize,
    /// First 128 diagnostics in deterministic scan order.
    pub issues: Vec<RepairIssue>,
}
impl RepairDiagnosis {
    /// Whether this scan and the selected protocol preflight found no issues.
    pub fn is_ready(&self) -> bool {
        self.issue_count == 0
    }
    pub(super) fn push(&mut self, kind: RepairIssueKind, block_id: Option<BlockId>, message: &str) {
        self.issue_count += 1;
        if self.issues.len() < DISPLAY_LIMIT {
            self.issues.push(RepairIssue {
                kind,
                block_id,
                message: super::inspect::bounded(message, 512),
            });
        }
    }
}

/// A display excerpt of an actual original result; never a replacement payload.
#[derive(Clone, Debug, Serialize)]
pub struct OriginalResultPreview {
    /// Original result block identity.
    pub result_id: BlockId,
    /// Declaration answered by that result.
    pub call_block_id: BlockId,
    /// Original status, including UnknownOutcome.
    pub status: ToolResultStatus,
    /// At most 1024 UTF-8 bytes of the original JSON result (including notes).
    pub excerpt: String,
    /// Warning if this call may have had a side effect before interruption
    /// (a built-in write or edit for the file, or an external tool for its server).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_warning: Option<RepairWriteWarning>,
}

/// Read-only view of the retained batch; inspection does not claim it.
#[derive(Clone, Debug)]
pub struct OriginalBatchPreview {
    /// Retained work whose original batch was inspected.
    pub work: WorkRef,
    /// Whether an unclaimed original batch still exists.
    pub available: bool,
    /// Number of completed original results, including omitted display rows.
    pub completed_count: usize,
    /// Number of still-pending original declarations.
    pub pending_count: usize,
    /// At most 128 original result excerpts.
    pub results: Vec<OriginalResultPreview>,
    /// Reason adoption is unavailable; absence still requires plan validation.
    pub adoption_error: Option<RepairError>,
    /// Total number of completed original results that have warnings, of both kinds.
    pub write_warnings_count: usize,
    /// Total number of those warnings that concern external (non-builtin) calls.
    /// Never greater than `write_warnings_count`; consumers may derive the
    /// built-in write count by subtraction.
    pub external_warnings_count: usize,
}

/// Inspection of actual current materials, optionally with one retained work.
#[derive(Clone, Debug)]
pub struct RepairInspection {
    /// Exact instance and material generation inspected.
    pub version: MaterialVersion,
    /// Complete counts and bounded material diagnostics.
    pub diagnosis: RepairDiagnosis,
    /// Original batch observation when a work reference was supplied.
    pub original: Option<OriginalBatchPreview>,
}

/// Explicit choice of how to prepare the next request's materials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairMode {
    /// Remove only declarations with no result, without inferring non-execution.
    RemoveUnpaired,
    /// Adopt verified original results of this instance's controlled interruption.
    AdoptOriginal(WorkRef),
}

/// Bounded differences; the immutable plan contains the full candidate materials.
#[derive(Clone, Debug, Serialize)]
pub struct RepairSummary {
    /// Number of blocks in the current materials.
    pub original_blocks: usize,
    /// Number of blocks in the complete proposed replacement.
    pub candidate_blocks: usize,
    /// Number of unpaired declarations removed.
    pub removed_count: usize,
    /// First 128 removed declaration identities.
    pub removed: Vec<BlockId>,
    /// Number of original result blocks adopted without payload changes.
    pub adopted_count: usize,
    /// First 128 adopted result identities.
    pub adopted: Vec<BlockId>,
    /// Identity of the new application explanation, placed after the exchange.
    pub explanation_id: BlockId,
    /// Number of declarations that may have had a side effect before interruption:
    /// built-in write or edit calls for files, and external calls for their servers.
    pub write_warnings_count: usize,
    /// Total number of those warnings that concern external (non-builtin) calls.
    /// Never greater than `write_warnings_count`; consumers may derive the
    /// built-in write count by subtraction.
    pub external_warnings_count: usize,
    /// First 128 warnings in deterministic scan order.
    pub write_warnings: Vec<RepairWriteWarning>,
}

/// Opaque candidate tied to one instance, material generation and claim revision.
/// Holding a plan owns a material copy, not a lock, batch claim or execution task.
#[derive(Debug)]
pub struct RepairPlan {
    pub(super) base: MaterialVersion,
    pub(super) claims_revision: u64,
    pub(super) mode: RepairMode,
    pub(super) candidate: Context,
    pub(super) summary: RepairSummary,
}
impl RepairPlan {
    /// Exact baseline required by application.
    pub fn base(&self) -> &MaterialVersion {
        &self.base
    }
    /// Choice used to create this candidate.
    pub fn mode(&self) -> &RepairMode {
        &self.mode
    }
    /// Bounded display of the proposed changes.
    pub fn summary(&self) -> &RepairSummary {
        &self.summary
    }
    /// Complete immutable candidate, including actual adopted result facts.
    pub fn candidate(&self) -> &Context {
        &self.candidate
    }
}

/// Explicit repair/admission failures; no failure mutates current materials.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RepairError {
    /// Request materials need explicit attention before admitting a new key.
    #[error("request materials need attention ({} unpaired declarations, {} issues); inspect repair before sending new work", .0.unpaired_count, .0.issue_count)]
    Materials(RepairDiagnosis),
    /// There is no incomplete exchange for either mode to change.
    #[error("materials need no structural repair")]
    NotNeeded,
    /// An original batch has been claimed, forgotten or was never returned.
    #[error("original uncommitted results are unavailable")]
    OriginalUnavailable,
    /// Assembly or interruption provenance is outside the reviewed built-in tool path.
    #[error(
        "original-result adoption requires the standard built-in tool assembly and controlled cancellation or deadline"
    )]
    UnsupportedSource,
    /// The original batch contains MCP or other non-builtin calls, whose
    /// server-side cancellation is outside the application's control.
    #[error(
        "a batch containing MCP or other non-builtin calls can only have its unpaired declarations removed; external side effects may already have happened"
    )]
    NonBuiltinCalls,
    /// The original batch no longer exactly matches the unresolved tail exchange.
    #[error("original batch does not match a single unresolved tail exchange")]
    InvalidOriginal,
    /// A claim or forget changed retained evidence after this plan was created.
    #[error("retained work or original batch changed; inspect and prepare a new plan")]
    StaleEvidence,
    /// The current saved revision differs from the CLI preview baseline.
    #[error("saved revision changed: expected {expected}, current {actual}")]
    RevisionConflict {
        /// Preview baseline supplied by the caller.
        expected: u64,
        /// Current confirmed revision under the instance's writer lock.
        actual: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::{RepairWarningKind, RepairWriteWarning};
    use causa::kernel::BlockId;

    #[test]
    fn warning_advice_escapes_model_supplied_tool_and_path() {
        let warning = RepairWriteWarning {
            call_block_id: BlockId::new(uuid::Uuid::now_v7()),
            tool_name: "write\u{202e}tool".into(),
            path: "dir/\u{2066}file\u{2028}.txt".into(),
            kind: RepairWarningKind::FileWrite,
        };
        let advice = warning.advice();
        assert!(advice.contains("\\u{202e}"));
        assert!(advice.contains("\\u{2066}"));
        assert!(advice.contains("\\u{2028}"));
        assert!(!advice.contains('\u{202e}'));
        assert!(!advice.contains('\u{2066}'));
        assert!(!advice.contains('\u{2028}'));
    }
}
