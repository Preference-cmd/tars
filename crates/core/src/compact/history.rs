//! Read-only history rewrite records beside a session record.
//!
//! A compaction or context edit replaces material in the current Context; the
//! replaced original blocks are kept as one rewrite record under
//! `<sessions>/<conversation id>.history/<record id>.json`. The session
//! record lists which records exist in its `history` index, so a stray file
//! never becomes visible material, and a record file is always written and
//! synced before the Context changes.

use super::HistoryTrigger;
use crate::store::StoreError;
use causa::kernel::{BlockId, Context, ContextBlock};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Wire format of one history rewrite record document.
pub(crate) const HISTORY_FORMAT: &str = "tars-history-v1.1";
/// One rewrite document is bounded like one session record.
pub(crate) const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;

/// Milliseconds since the Unix epoch, or zero when the clock is before it.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The kind of rewrite performed on the Context material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RewriteKind {
    /// A run of material replaced by a model-written brief.
    Summarize,
    /// One block (such as a large tool result) replaced in place by a shorter one.
    Elide,
    /// Several earlier briefs merged into one.
    Merge,
}

impl RewriteKind {
    /// Stable lower-case name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Summarize => "summarize",
            Self::Elide => "elide",
            Self::Merge => "merge",
        }
    }
}

/// One range replaced in the Context: the removed blocks and what took their place.
///
/// Every replacement inserts at least one block; expansion puts the originals
/// back where that inserted run stands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangeReplacement {
    /// Replaced half-open range, in the coordinates of the Context before the rewrite.
    pub range: std::ops::Range<usize>,
    /// Full original blocks that were removed.
    pub originals: Vec<ContextBlock>,
    /// IDs of the blocks inserted in place of the range, in order.
    pub inserted_ids: Vec<BlockId>,
}

/// One planned replacement with its inserted blocks ready to apply to a Context.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlannedReplacement {
    /// Half-open range of the current Context to replace.
    pub(crate) range: std::ops::Range<usize>,
    /// New blocks to insert in place of that range.
    pub(crate) with: Vec<ContextBlock>,
}

/// One rewrite ready for the shared commit path.
///
/// The record is built from the Context at commit time, so the stored
/// originals are exactly the blocks the edit removes.
#[derive(Clone, Debug)]
pub(crate) struct Rewrite {
    /// What asked for this rewrite.
    pub(crate) trigger: HistoryTrigger,
    /// What kind of rewrite this is.
    pub(crate) kind: RewriteKind,
    /// Estimated request size before the replacement.
    pub(crate) estimate_before: u64,
    /// Estimated request size after the replacement.
    pub(crate) estimate_after: u64,
    /// Ranges to replace, ascending and non-overlapping, each with a non-empty insertion.
    pub(crate) replacements: Vec<PlannedReplacement>,
}

/// Why the shared commit path did not change the Context.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RewriteError {
    /// The record could not be written; the Context is unchanged.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The replacements were refused before or by the Context edit; no record file remains.
    #[error("context edit rejected: {0}")]
    Rejected(String),
}

/// One rewrite record as written to its own file (`tars-history-v1.1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecord {
    /// `tars-history-v1.1`.
    pub format: String,
    /// Owning conversation identity.
    pub conversation_id: String,
    /// Canonical UUID of this rewrite record.
    pub record_id: String,
    /// Wall-clock creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// What triggered this rewrite.
    pub trigger: HistoryTrigger,
    /// What kind of rewrite was performed (`summarize`, `elide`, `merge`).
    pub kind: RewriteKind,
    /// Estimated request size before the replacement.
    pub estimate_before: u64,
    /// Estimated request size after the replacement.
    pub estimate_after: u64,
    /// In order, each replaced range with its removed original blocks and inserted block IDs.
    pub replacements: Vec<RangeReplacement>,
}

impl HistoryRecord {
    /// Validate the record against its expected conversation and record identity.
    pub(crate) fn validate(
        &self,
        conversation_id: &str,
        record_id: &str,
    ) -> Result<(), StoreError> {
        if self.format != HISTORY_FORMAT {
            return Err(StoreError::InvalidHistory(format!(
                "unsupported history format: {}; expected {}",
                self.format, HISTORY_FORMAT
            )));
        }
        if self.conversation_id != conversation_id {
            return Err(StoreError::InvalidHistory(
                "history record belongs to a different conversation".into(),
            ));
        }
        if self.record_id != record_id {
            return Err(StoreError::InvalidHistory(
                "history record file name does not match its identity".into(),
            ));
        }
        if self.replacements.is_empty()
            || self
                .replacements
                .iter()
                .any(|replacement| replacement.inserted_ids.is_empty())
        {
            return Err(StoreError::InvalidHistory(
                "history record has a replacement without inserted blocks".into(),
            ));
        }
        Ok(())
    }

    /// Index entry describing this document in the session record.
    pub(crate) fn index_entry(&self) -> HistoryRecordEntry {
        HistoryRecordEntry {
            record_id: self.record_id.clone(),
            created_at_ms: self.created_at_ms,
            trigger: self.trigger,
            kind: self.kind,
            removed_block_count: self.replacements.iter().map(|r| r.originals.len()).sum(),
            inserted_block_ids: self
                .replacements
                .iter()
                .flat_map(|r| r.inserted_ids.iter().copied())
                .collect(),
        }
    }
}

/// One rewrite record index entry as stored in the session record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecordEntry {
    /// Canonical UUID of the history rewrite file (`<record_id>.json`).
    pub record_id: String,
    /// Wall-clock creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// What triggered the rewrite.
    pub trigger: HistoryTrigger,
    /// The kind of rewrite (`summarize`, `elide`, `merge`).
    pub kind: RewriteKind,
    /// Number of blocks removed by this rewrite.
    pub removed_block_count: usize,
    /// IDs of all blocks inserted by this rewrite, in range order.
    pub inserted_block_ids: Vec<BlockId>,
}

/// The history directory and the record-backed index for one conversation.
pub(crate) struct HistoryStore {
    directory: PathBuf,
    conversation_id: String,
    entries: Mutex<Vec<HistoryRecordEntry>>,
    #[cfg(test)]
    read_count: AtomicUsize,
}

impl HistoryStore {
    /// Bind one conversation's history directory to its restored index.
    pub(crate) fn new(
        sessions_directory: &Path,
        conversation_id: &str,
        entries: Vec<HistoryRecordEntry>,
    ) -> Self {
        Self {
            directory: history_directory(sessions_directory, conversation_id),
            conversation_id: conversation_id.to_owned(),
            entries: Mutex::new(entries),
            #[cfg(test)]
            read_count: AtomicUsize::new(0),
        }
    }

    /// Copy of the current index, in creation order.
    pub(crate) fn entries(&self) -> Vec<HistoryRecordEntry> {
        self.entries
            .lock()
            .expect("history index has no callbacks")
            .clone()
    }

    /// Number of record files read through this store.
    #[cfg(test)]
    pub(crate) fn read_count(&self) -> usize {
        self.read_count.load(Ordering::SeqCst)
    }

    /// Write one rewrite record document and fsync it.
    ///
    /// The caller must write this before it edits the Context. A record that
    /// already exists is never overwritten: a repeated write is an error.
    pub(crate) fn write(&self, document: &HistoryRecord) -> Result<(), StoreError> {
        document.validate(&self.conversation_id, &document.record_id)?;
        let bytes = serde_json::to_vec(document)?;
        if bytes.len() > MAX_HISTORY_BYTES {
            return Err(StoreError::HistoryTooLarge {
                limit: MAX_HISTORY_BYTES,
            });
        }
        fs::create_dir_all(&self.directory)?;
        let path = self.record_path(&document.record_id);
        if path.exists() {
            return Err(StoreError::InvalidHistory(format!(
                "history record {} already exists",
                document.record_id
            )));
        }
        let mut pending = tempfile::Builder::new()
            .prefix(".pending-")
            .tempfile_in(&self.directory)?;
        pending.write_all(&bytes)?;
        pending.as_file().sync_all()?;
        pending.persist_noclobber(&path).map_err(|error| {
            if error.error.kind() == io::ErrorKind::AlreadyExists {
                StoreError::InvalidHistory(format!(
                    "history record {} already exists",
                    document.record_id
                ))
            } else {
                error.error.into()
            }
        })?;
        sync_directory(&self.directory);
        Ok(())
    }

    /// Shared commit path for every Context rewrite.
    ///
    /// Builds the record from the current Context, writes and fsyncs it,
    /// applies all replacements in one `context.edit()` … `commit()`, and only
    /// then appends the index entry. A refused edit removes the written file
    /// and leaves the Context and the index unchanged.
    pub(crate) fn commit_rewrite(
        &self,
        context: &mut Context,
        rewrite: Rewrite,
    ) -> Result<HistoryRecordEntry, RewriteError> {
        let record = self.record_for(context.blocks(), &rewrite)?;
        self.write(&record)?;
        let mut edit = context.edit();
        for replacement in rewrite.replacements {
            edit = edit.replace(replacement.range, replacement.with);
        }
        if let Err(failure) = edit.commit() {
            self.remove(&record.record_id);
            return Err(RewriteError::Rejected(format!("{:?}", failure.reason)));
        }
        let entry = record.index_entry();
        self.append(entry.clone());
        Ok(entry)
    }

    fn record_for(
        &self,
        blocks: &[ContextBlock],
        rewrite: &Rewrite,
    ) -> Result<HistoryRecord, RewriteError> {
        if rewrite.replacements.is_empty() {
            return Err(RewriteError::Rejected(
                "a rewrite replaces at least one range".into(),
            ));
        }
        let mut replacements = Vec::with_capacity(rewrite.replacements.len());
        let mut previous_end = 0;
        for planned in &rewrite.replacements {
            let range = planned.range.clone();
            if range.start >= range.end || range.start < previous_end || range.end > blocks.len() {
                return Err(RewriteError::Rejected(format!(
                    "range {}..{} is empty, out of order or past the end of {} blocks",
                    range.start,
                    range.end,
                    blocks.len()
                )));
            }
            if planned.with.is_empty() {
                return Err(RewriteError::Rejected(format!(
                    "range {}..{} inserts no blocks",
                    range.start, range.end
                )));
            }
            previous_end = range.end;
            replacements.push(RangeReplacement {
                originals: blocks[range.clone()].to_vec(),
                inserted_ids: planned.with.iter().map(ContextBlock::id).collect(),
                range,
            });
        }
        Ok(HistoryRecord {
            format: HISTORY_FORMAT.to_owned(),
            conversation_id: self.conversation_id.clone(),
            record_id: uuid::Uuid::now_v7().to_string(),
            created_at_ms: now_ms(),
            trigger: rewrite.trigger,
            kind: rewrite.kind,
            estimate_before: rewrite.estimate_before,
            estimate_after: rewrite.estimate_after,
            replacements,
        })
    }

    /// Append one index entry after its document and Context edit are in place.
    pub(crate) fn append(&self, entry: HistoryRecordEntry) {
        self.entries
            .lock()
            .expect("history index has no callbacks")
            .push(entry);
    }

    /// Read one rewrite record document.
    pub(crate) fn read(&self, record_id: &str) -> Result<HistoryRecord, StoreError> {
        canonical_record_id(record_id)?;
        let path = self.record_path(record_id);
        let mut file = File::open(&path)?;
        let length = file.metadata()?.len();
        if length > MAX_HISTORY_BYTES as u64 {
            return Err(StoreError::HistoryTooLarge {
                limit: MAX_HISTORY_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        file.read_to_end(&mut bytes)?;

        #[derive(Deserialize)]
        struct FormatCheck {
            format: String,
        }
        let check: FormatCheck = serde_json::from_slice(&bytes)?;
        if check.format != HISTORY_FORMAT {
            return Err(StoreError::InvalidHistory(format!(
                "unsupported history format: {}; expected {}. Older history records cannot be read; export materials with `tars session export-materials` to preserve conversation materials.",
                check.format, HISTORY_FORMAT
            )));
        }

        let document: HistoryRecord = serde_json::from_slice(&bytes)?;
        document.validate(&self.conversation_id, record_id)?;
        #[cfg(test)]
        self.read_count.fetch_add(1, Ordering::SeqCst);
        Ok(document)
    }

    /// Drop one index entry after its Context edit turned out not to happen.
    pub(crate) fn remove_entry(&self, record_id: &str) {
        self.entries
            .lock()
            .expect("history index has no callbacks")
            .retain(|entry| entry.record_id != record_id);
    }

    /// Delete one record file, ignoring a file that is already gone.
    pub(crate) fn remove(&self, record_id: &str) {
        let _ = fs::remove_file(self.record_path(record_id));
    }

    fn record_path(&self, record_id: &str) -> PathBuf {
        self.directory.join(format!("{record_id}.json"))
    }
}

/// History directory for one conversation under the sessions directory.
pub(crate) fn history_directory(sessions_directory: &Path, conversation_id: &str) -> PathBuf {
    sessions_directory.join(format!("{conversation_id}.history"))
}

/// Remove unreferenced record files without requiring a bound store.
pub(crate) fn cleanup_records(
    sessions_directory: &Path,
    conversation_id: &str,
    entries: &[HistoryRecordEntry],
) -> Result<(), StoreError> {
    cleanup_directory(
        &history_directory(sessions_directory, conversation_id),
        entries,
    )
}

fn cleanup_directory(directory: &Path, entries: &[HistoryRecordEntry]) -> Result<(), StoreError> {
    let mut referenced = std::collections::HashSet::with_capacity(entries.len());
    for entry in entries {
        canonical_record_id(&entry.record_id)?;
        referenced.insert(format!("{}.json", entry.record_id));
    }
    let listing = match fs::read_dir(directory) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in listing {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if referenced.contains(name) {
            continue;
        }
        if entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn sync_directory(directory: &Path) {
    // Directory fsync is best effort: not every platform or filesystem
    // supports it, and the record commit is the durable authority.
    if let Ok(directory) = File::open(directory) {
        let _ = directory.sync_all();
    }
}

fn canonical_record_id(record_id: &str) -> Result<(), StoreError> {
    match uuid::Uuid::parse_str(record_id) {
        Ok(uuid) if uuid.hyphenated().to_string() == record_id => Ok(()),
        _ => Err(StoreError::InvalidHistory(format!(
            "history record identity is not a canonical UUID: {record_id}"
        ))),
    }
}

/// Validate an index entry read from a session record.
pub(crate) fn validate_index(entries: &[HistoryRecordEntry]) -> Result<(), StoreError> {
    let mut seen = std::collections::HashSet::with_capacity(entries.len());
    for entry in entries {
        canonical_record_id(&entry.record_id)?;
        if !seen.insert(entry.record_id.as_str()) {
            return Err(StoreError::InvalidHistory(format!(
                "duplicate history record {}",
                entry.record_id
            )));
        }
    }
    Ok(())
}
