//! Read-only history segments beside a session record.
//!
//! A compaction replaces the current Context; the replaced material is kept as
//! one file per segment under `<sessions>/<conversation id>.history/`. The
//! session record lists which segments exist, so a stray file never becomes
//! visible material, and a file is always written before the Context changes.

use super::{HistorySegment, HistoryTrigger};
use crate::store::StoreError;
use causa::kernel::{BlockId, Context};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Wire format of one history segment document.
pub(crate) const HISTORY_FORMAT: &str = "tars-history-v1";
/// One segment document is bounded like one session record.
pub(crate) const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;

/// Milliseconds since the Unix epoch, or zero when the clock is before it.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One replaced conversation as written to its own file.
///
/// The full old Context is stored verbatim; the document is read for display
/// only and is never sent to the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySegmentDocument {
    /// `tars-history-v1`.
    pub format: String,
    /// Owning conversation identity.
    pub conversation_id: String,
    /// Canonical UUID shared with the record index entry.
    pub segment_id: String,
    /// Wall-clock creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// What triggered this compaction.
    pub trigger: HistoryTrigger,
    /// Estimated request size before the replacement.
    pub estimate_before: u64,
    /// Estimated request size after the replacement.
    pub estimate_after: u64,
    /// Summary block that replaced the stored material.
    pub summary_block_id: BlockId,
    /// The replaced material, exactly as it was.
    pub context: Context,
}

impl HistorySegmentDocument {
    fn validate(&self, conversation_id: &str, segment_id: &str) -> Result<(), StoreError> {
        if self.format != HISTORY_FORMAT {
            return Err(StoreError::InvalidHistory(format!(
                "unsupported history format: {}",
                self.format
            )));
        }
        if self.conversation_id != conversation_id {
            return Err(StoreError::InvalidHistory(
                "history segment belongs to a different conversation".into(),
            ));
        }
        if self.segment_id != segment_id {
            return Err(StoreError::InvalidHistory(
                "history segment file name does not match its identity".into(),
            ));
        }
        Ok(())
    }

    /// Index entry describing this document.
    pub(crate) fn index_entry(&self) -> HistorySegment {
        HistorySegment {
            id: self.segment_id.clone(),
            created_at_ms: self.created_at_ms,
            trigger: self.trigger,
            blocks: self.context.blocks().len(),
            summary_block_id: self.summary_block_id,
        }
    }
}

/// The history directory and the record-backed index for one conversation.
pub(crate) struct HistoryStore {
    directory: PathBuf,
    conversation_id: String,
    entries: Mutex<Vec<HistorySegment>>,
}

impl HistoryStore {
    /// Bind one conversation's history directory to its restored index.
    pub(crate) fn new(
        sessions_directory: &Path,
        conversation_id: &str,
        entries: Vec<HistorySegment>,
    ) -> Self {
        Self {
            directory: history_directory(sessions_directory, conversation_id),
            conversation_id: conversation_id.to_owned(),
            entries: Mutex::new(entries),
        }
    }

    /// Copy of the current index, in creation order.
    pub(crate) fn entries(&self) -> Vec<HistorySegment> {
        self.entries
            .lock()
            .expect("history index has no callbacks")
            .clone()
    }

    /// Write one segment document and fsync it.
    ///
    /// The caller must write this before it edits the Context. A segment that
    /// already exists is never overwritten: a repeated write is an error.
    pub(crate) fn write(&self, document: &HistorySegmentDocument) -> Result<(), StoreError> {
        document.validate(&self.conversation_id, &document.segment_id)?;
        let bytes = serde_json::to_vec(document)?;
        if bytes.len() > MAX_HISTORY_BYTES {
            return Err(StoreError::HistoryTooLarge {
                limit: MAX_HISTORY_BYTES,
            });
        }
        fs::create_dir_all(&self.directory)?;
        let path = self.segment_path(&document.segment_id);
        if path.exists() {
            return Err(StoreError::InvalidHistory(format!(
                "history segment {} already exists",
                document.segment_id
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
                    "history segment {} already exists",
                    document.segment_id
                ))
            } else {
                error.error.into()
            }
        })?;
        sync_directory(&self.directory);
        Ok(())
    }

    /// Append one index entry after its document and Context edit are in place.
    pub(crate) fn append(&self, entry: HistorySegment) {
        self.entries
            .lock()
            .expect("history index has no callbacks")
            .push(entry);
    }

    /// Read one segment document for display.
    pub(crate) fn read(&self, segment_id: &str) -> Result<HistorySegmentDocument, StoreError> {
        canonical_segment_id(segment_id)?;
        let path = self.segment_path(segment_id);
        let mut file = File::open(&path)?;
        let length = file.metadata()?.len();
        if length > MAX_HISTORY_BYTES as u64 {
            return Err(StoreError::HistoryTooLarge {
                limit: MAX_HISTORY_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        file.read_to_end(&mut bytes)?;
        let document: HistorySegmentDocument = serde_json::from_slice(&bytes)?;
        document.validate(&self.conversation_id, segment_id)?;
        Ok(document)
    }

    /// Drop one index entry after its Context edit turned out not to happen.
    pub(crate) fn remove_entry(&self, segment_id: &str) {
        self.entries
            .lock()
            .expect("history index has no callbacks")
            .retain(|entry| entry.id != segment_id);
    }

    /// Delete one segment file, ignoring a file that is already gone.
    pub(crate) fn remove(&self, segment_id: &str) {
        let _ = fs::remove_file(self.segment_path(segment_id));
    }

    fn segment_path(&self, segment_id: &str) -> PathBuf {
        self.directory.join(format!("{segment_id}.json"))
    }
}

/// History directory for one conversation under the sessions directory.
pub(crate) fn history_directory(sessions_directory: &Path, conversation_id: &str) -> PathBuf {
    sessions_directory.join(format!("{conversation_id}.history"))
}

/// Remove unreferenced segment files without requiring a bound store.
pub(crate) fn cleanup_segments(
    sessions_directory: &Path,
    conversation_id: &str,
    entries: &[HistorySegment],
) -> Result<(), StoreError> {
    cleanup_directory(
        &history_directory(sessions_directory, conversation_id),
        entries,
    )
}

fn cleanup_directory(directory: &Path, entries: &[HistorySegment]) -> Result<(), StoreError> {
    let mut referenced = std::collections::HashSet::with_capacity(entries.len());
    for entry in entries {
        canonical_segment_id(&entry.id)?;
        referenced.insert(format!("{}.json", entry.id));
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

fn canonical_segment_id(segment_id: &str) -> Result<(), StoreError> {
    match uuid::Uuid::parse_str(segment_id) {
        Ok(uuid) if uuid.hyphenated().to_string() == segment_id => Ok(()),
        _ => Err(StoreError::InvalidHistory(format!(
            "history segment identity is not a canonical UUID: {segment_id}"
        ))),
    }
}

/// Validate an index entry read from a session record.
pub(crate) fn validate_index(entries: &[HistorySegment]) -> Result<(), StoreError> {
    let mut seen = std::collections::HashSet::with_capacity(entries.len());
    for entry in entries {
        canonical_segment_id(&entry.id)?;
        if !seen.insert(entry.id.as_str()) {
            return Err(StoreError::InvalidHistory(format!(
                "duplicate history segment {}",
                entry.id
            )));
        }
    }
    Ok(())
}
