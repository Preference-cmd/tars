//! Long-session context compaction.
//!
//! A session's materials only grow, so a long conversation eventually exceeds
//! the model's context window. Compaction replaces the current Context with a
//! summary written by the same model, and keeps the replaced conversation as
//! one read-only history segment beside the session record.
//!
//! The pieces are deliberately split:
//!
//! - `history` owns the `tars-history-v1` segment files and the index stored in
//!   the session record.
//! - `estimate` turns a Context into a conservative token estimate and wraps
//!   the model gateway to remember the upstream-reported prompt size.
//! - `summary` builds the summary instruction and reads the model's answer.
//! - `runtime` holds the running settings, the force flag and the pairing of a
//!   written segment with the replaced Context.
//! - `preparer` is the Causa `ContextPreparer` installed per work; it decides
//!   whether to compact before a logical model invocation.
//!
//! Causa edits made by a preparer are committed independently, so a segment
//! file is always written before the Context is replaced; an abandoned attempt
//! leaves the Context untouched and is cleaned up on the next open.

pub(crate) mod estimate;
pub(crate) mod history;
pub(crate) mod preparer;
pub(crate) mod runtime;
pub(crate) mod summary;

use causa::kernel::{BlockContent, BlockId, ContentPart, ContextBlock, Role};
use serde::{Deserialize, Serialize};

pub use history::HistorySegmentDocument;
pub use runtime::CompactionOutcome;
pub(crate) use runtime::CompactionRuntime;

/// Why a compaction attempt was abandoned before editing the Context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionAbandon {
    /// The summary came back empty or blank.
    EmptySummary,
    /// The replacement estimate is not smaller than the estimate before it.
    NoShrink,
    /// The summary model call failed or was stopped by the turn's control.
    RequestFailed,
    /// Current materials still contain tool declarations without results.
    UnpairedMaterials,
    /// The history segment file could not be written.
    UnwritableHistory,
    /// The replacement edit was rejected; the segment file was deleted.
    EditRejected,
    /// The session has no persistent history directory.
    Unavailable,
}

impl CompactionAbandon {
    /// Stable lower-case name used in progress and diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::EmptySummary => "empty_summary",
            Self::NoShrink => "no_shrink",
            Self::RequestFailed => "request_failed",
            Self::UnpairedMaterials => "unpaired_materials",
            Self::UnwritableHistory => "unwritable_history",
            Self::EditRejected => "edit_rejected",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Advisory compaction lifecycle carried through `WorkProgress`.
///
/// Like other progress observations this is losable: reliable facts remain the
/// returned Context and the saved record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionProgress {
    /// A summary request is about to start; nothing has changed yet.
    Started {
        /// What asked for this attempt.
        trigger: HistoryTrigger,
        /// Estimated request size before this attempt.
        estimate_before: u64,
    },
    /// The Context was replaced; the segment is persisted with the next save.
    Finished {
        /// What asked for this attempt.
        trigger: HistoryTrigger,
        /// Estimated request size before this attempt.
        estimate_before: u64,
        /// Estimated request size after this attempt.
        estimate_after: u64,
        /// Identity of the history segment that holds the replaced Context.
        segment: String,
    },
    /// The attempt stopped without touching the Context.
    Abandoned {
        /// What asked for this attempt.
        trigger: HistoryTrigger,
        /// Why it stopped.
        reason: CompactionAbandon,
    },
}

/// One replaced conversation segment as listed in the session record.
///
/// The record is the authority for which segments exist; files that are not
/// listed are unreferenced and are cleaned up while the session write lock is
/// held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySegment {
    /// Canonical UUID of the history segment file (`<id>.json`).
    pub id: String,
    /// Wall-clock creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// What triggered the compaction.
    pub trigger: HistoryTrigger,
    /// Number of blocks the replaced Context held.
    pub blocks: usize,
    /// Summary block that replaced those blocks in the current Context.
    pub summary_block_id: BlockId,
}

/// What triggered one compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryTrigger {
    /// The request estimate crossed the configured threshold.
    Auto,
    /// The previous round ended on the provider's context-window limit.
    Passive,
    /// A user asked for compaction explicitly.
    Manual,
    /// A trigger written by a newer version this one does not know.
    #[serde(other)]
    Unknown,
}

impl HistoryTrigger {
    /// Stable lower-case name used by history documents and progress.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Passive => "passive",
            Self::Manual => "manual",
            Self::Unknown => "unknown",
        }
    }
}

/// Verbatim user-role text of one submitted request, when it carries any.
///
/// Only used to keep the current turn's request visible in a mid-turn summary;
/// empty requests (continuing the current material) return `None`.
pub fn user_request_text(blocks: &[ContextBlock]) -> Option<String> {
    let mut messages: Vec<String> = Vec::new();
    for block in blocks {
        let BlockContent::Message { role, parts } = block.content() else {
            continue;
        };
        if *role != Role::User {
            continue;
        }
        let mut text = String::new();
        for part in parts {
            if let ContentPart::Text(payload) = part {
                text.push_str(&payload.0);
            }
        }
        if !text.trim().is_empty() {
            messages.push(text);
        }
    }
    (!messages.is_empty()).then(|| messages.join("\n\n"))
}

#[cfg(test)]
mod tests;
