//! Long-session context compaction and history expansion.
//!
//! A session's materials only grow, so a long conversation eventually exceeds
//! the model's context window. Compaction replaces the current Context with a
//! summary written by the same model, and keeps the replaced original blocks as
//! one read-only rewrite record beside the session record.
//!
//! The pieces are deliberately split:
//!
//! - `history` owns the `tars-history-v1.1` rewrite files and the index stored in
//!   the session record.
//! - `expansion` expands the current Context into the full conversation.
//! - `estimate` turns a Context into a conservative token estimate and wraps
//!   the model gateway to remember the upstream-reported prompt size.
//! - `summary` builds the summary instruction and reads the model's answer.
//! - `runtime` holds the running settings, the force flag and the pairing of a
//!   written record with the replaced Context.
//! - `preparer` is the Causa `ContextPreparer` installed per work; it decides
//!   whether to compact before a logical model invocation.
//!
//! Causa edits made by a preparer are committed independently, so a record
//! file is always written before the Context is replaced; an abandoned attempt
//! leaves the Context untouched and is cleaned up on the next open.

pub(crate) mod artifacts;
pub(crate) mod block_text;
pub(crate) mod estimate;
pub(crate) mod expansion;
pub(crate) mod history;
pub(crate) mod preparer;
pub(crate) mod recall;
pub(crate) mod runtime;
pub(crate) mod summary;

use causa::kernel::{BlockContent, ContentPart, ContextBlock, Role, TextPayload};
use serde::{Deserialize, Serialize};

pub(crate) use artifacts::HistoryArtifacts;
pub(crate) use block_text::block_to_text;
pub(crate) use expansion::expand_conversation;
pub use expansion::{
    BlockStatus, ExpandedBlock, ExpandedConversation, ExpansionCursor, ExpansionStep, RewriteMark,
    UnreadableRecord,
};
pub use history::{HistoryRecord, HistoryRecordEntry, RangeReplacement, RewriteKind};
pub(crate) use recall::RecallTool;
pub use runtime::CompactionOutcome;
pub(crate) use runtime::CompactionRuntime;
pub use summary::SUMMARY_SOURCE;

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
    /// The rewrite record file could not be written.
    UnwritableHistory,
    /// The replacement edit was rejected; the record file was deleted.
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
    /// The Context was replaced; the record's index entry is saved with the next save.
    Finished {
        /// What asked for this attempt.
        trigger: HistoryTrigger,
        /// Estimated request size before this attempt.
        estimate_before: u64,
        /// Estimated request size after this attempt.
        estimate_after: u64,
        /// Identity of the rewrite record that holds the replaced material.
        record: String,
    },
    /// The attempt stopped without touching the Context.
    Abandoned {
        /// What asked for this attempt.
        trigger: HistoryTrigger,
        /// Why it stopped.
        reason: CompactionAbandon,
    },
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

/// Prefix of the model-facing note on a shortened tool result.
#[cfg_attr(not(test), allow(dead_code))] // Used by tests today; P4 will use this for result elision.
pub(crate) const SHORTENED_RESULT_NOTE_PREFIX: &str =
    "Shortened by compaction; full original: recall id=";

/// Build the model-facing note for a tool result shortened by compaction.
#[cfg_attr(not(test), allow(dead_code))] // Used by tests today; P4 will use this for result elision.
pub(crate) fn shortened_result_note(
    record_id: &str,
    block_id: impl std::fmt::Display,
) -> TextPayload {
    TextPayload::new(format!(
        "{SHORTENED_RESULT_NOTE_PREFIX}{record_id}/{block_id}"
    ))
}

#[cfg(test)]
mod tests;
