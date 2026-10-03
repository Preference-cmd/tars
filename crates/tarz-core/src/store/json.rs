use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{CommitStage, PreparedCommit, SavedSessionRecord, StoreError, record::canonical_uuid};

/// A resource limit, not a performance claim or a material-history limit.
pub(crate) const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Default)]
struct Counters {
    serialization_count: AtomicU64,
    snapshot_clones: AtomicU64,
    bytes_written: AtomicU64,
    writes: AtomicU64,
}
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StoreMetrics {
    pub(crate) serialization_count: u64,
    pub(crate) snapshot_clones: u64,
    pub(crate) bytes_written: u64,
    pub(crate) writes: u64,
}

#[derive(Clone)]
pub(crate) struct SessionStore {
    directory: PathBuf,
    counters: Arc<Counters>,
    #[cfg(test)]
    hook: Arc<Mutex<Option<FaultHook>>>,
}
impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}
#[derive(Debug)]
pub(crate) struct ListedSession {
    pub(crate) conversation_id: String,
    pub(crate) record: Result<SavedSessionRecord, StoreError>,
    pub(crate) active: Result<bool, StoreError>,
}
impl SessionStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            directory: root.join("sessions"),
            counters: Arc::default(),
            #[cfg(test)]
            hook: Arc::default(),
        }
    }
    #[cfg(test)]
    pub(crate) fn set_fault_hook(&self, hook: Option<FaultHook>) {
        *self.hook.lock().unwrap() = hook;
    }
    pub(crate) fn acquire_writer(&self, id: &str) -> Result<Writer, StoreError> {
        canonical_uuid(id)?;
        fs::create_dir_all(&self.directory)?;
        let lock = open_lock(&self.directory, id)?;
        acquire_lock(&lock, id)?;
        let writer = Writer {
            id: id.into(),
            directory: self.directory.clone(),
            _lock: lock,
            state: Mutex::new(WriterState::default()),
            counters: self.counters.clone(),
            #[cfg(test)]
            hook: Mutex::new(self.hook.lock().unwrap().clone()),
        };
        writer.cleanup_pending()?;
        Ok(writer)
    }
    /// Each final file is inspected independently; no whole-directory snapshot.
    pub(crate) fn list(&self) -> Result<Vec<ListedSession>, StoreError> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|x| x.to_str()) else {
                continue;
            };
            let identity = canonical_uuid(id);
            let record = match identity {
                Ok(()) => read_record(&path, id).and_then(|r| {
                    r.ok_or_else(|| {
                        StoreError::Io(Arc::new(io::Error::from(io::ErrorKind::NotFound)))
                    })
                }),
                Err(error) => Err(error),
            };
            let active = (|| {
                canonical_uuid(id)?;
                let lock = open_lock(&self.directory, id)?;
                match acquire_lock(&lock, id) {
                    Ok(()) => Ok(false),
                    Err(StoreError::Occupied(_)) => Ok(true),
                    Err(e) => Err(e),
                }
            })();
            result.push(ListedSession {
                conversation_id: id.into(),
                record,
                active,
            });
        }
        result.sort_by(|a, b| a.conversation_id.cmp(&b.conversation_id));
        Ok(result)
    }
    #[cfg(test)]
    pub(crate) fn metrics(&self) -> StoreMetrics {
        StoreMetrics {
            serialization_count: self.counters.serialization_count.load(Ordering::Relaxed),
            snapshot_clones: self.counters.snapshot_clones.load(Ordering::Relaxed),
            bytes_written: self.counters.bytes_written.load(Ordering::Relaxed),
            writes: self.counters.writes.load(Ordering::Relaxed),
        }
    }
}

// The outer Option distinguishes an unread baseline from confirmed absence.
#[derive(Default)]
struct WriterState {
    baseline: Option<Option<SavedSessionRecord>>,
    pending: Option<(PreparedCommit, Option<SavedSessionRecord>)>,
}
pub(crate) struct Writer {
    id: String,
    directory: PathBuf,
    _lock: File,
    state: Mutex<WriterState>,
    counters: Arc<Counters>,
    #[cfg(test)]
    hook: Mutex<Option<FaultHook>>,
}
impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
// Arc ownership ensures this runs only after every blocking operation stops.
// Explicit release also avoids a fork-to-exec descriptor briefly extending the
// lock after the activity's own File is closed. Failure still closes the file.
impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommitResolution {
    Applied(Box<SavedSessionRecord>),
    NotApplied,
}

impl Writer {
    fn clone_record(&self, record: &SavedSessionRecord) -> SavedSessionRecord {
        self.counters
            .snapshot_clones
            .fetch_add(1, Ordering::Relaxed);
        record.clone()
    }
    fn clone_snapshot(&self, record: &Option<SavedSessionRecord>) -> Option<SavedSessionRecord> {
        record.as_ref().map(|r| self.clone_record(r))
    }
    fn path(&self) -> PathBuf {
        self.directory.join(format!("{}.json", self.id))
    }
    pub(crate) fn read_latest(&self) -> Result<Option<SavedSessionRecord>, StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let record = read_record(&self.path(), &self.id)?;
        if state.baseline.is_none() {
            state.baseline = Some(self.clone_snapshot(&record));
        }
        Ok(record)
    }
    pub(crate) fn commit(
        &self,
        operation: &PreparedCommit,
    ) -> Result<SavedSessionRecord, StoreError> {
        operation.validate(&self.id)?;
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let latest = read_record(&self.path(), &self.id)?;
        if let Some(current) = &latest
            && current.last_commit_id == operation.record.last_commit_id
        {
            if current != &operation.record {
                return Err(StoreError::CommitIdentityConflict);
            }
            state.baseline = Some(self.clone_snapshot(&latest));
            state.pending = None;
            return Ok(self.clone_record(current));
        }
        if let Some((pending, _)) = &state.pending {
            if pending.record.last_commit_id == operation.record.last_commit_id {
                if pending != operation {
                    return Err(StoreError::CommitIdentityConflict);
                }
            } else if latest.as_ref() == Some(&pending.record) {
                state.pending = None;
                state.baseline = Some(self.clone_snapshot(&latest));
            } else {
                return Err(StoreError::PendingCommit);
            }
        }
        let baseline = state
            .baseline
            .get_or_insert_with(|| self.clone_snapshot(&latest));
        if latest != *baseline || revision(&latest) != operation.expected_revision {
            return Err(StoreError::Conflict {
                expected: operation.expected_revision,
                actual: revision(&latest),
            });
        }
        if state.pending.is_none() {
            state.pending = Some((
                PreparedCommit {
                    expected_revision: operation.expected_revision,
                    record: self.clone_record(&operation.record),
                },
                latest,
            ));
        }
        self.hit(CommitStage::Serialize)?;
        self.counters
            .serialization_count
            .fetch_add(1, Ordering::Relaxed);
        let bytes = serialize_record(&operation.record)?;
        let mut pending = tempfile::Builder::new()
            .prefix(&format!(".{}.pending-", self.id))
            .tempfile_in(&self.directory)
            .map_err(|e| commit_error(CommitStage::Write, e))?;
        self.hit(CommitStage::Write)?;
        struct Counted<'a> {
            file: &'a mut tempfile::NamedTempFile,
            counter: &'a AtomicU64,
        }
        impl Write for Counted<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let written = self.file.write(bytes)?;
                self.counter.fetch_add(written as u64, Ordering::Relaxed);
                Ok(written)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.file.flush()
            }
        }
        Counted {
            file: &mut pending,
            counter: &self.counters.bytes_written,
        }
        .write_all(&bytes)
        .map_err(|e| commit_error(CommitStage::Write, e))?;
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        self.hit(CommitStage::Sync)?;
        pending
            .as_file()
            .sync_all()
            .map_err(|e| commit_error(CommitStage::Sync, e))?;
        self.hit(CommitStage::Replace)?;
        pending
            .persist(self.path())
            .map_err(|e| commit_error(CommitStage::Replace, e.error))?;
        self.hit(CommitStage::Confirm)?;
        let confirmed = read_record(&self.path(), &self.id)?;
        if confirmed.as_ref() != Some(&operation.record) {
            return Err(StoreError::ConfirmationUnknown);
        }
        self.hit(CommitStage::Reply)?;
        state.baseline = Some(confirmed);
        state.pending = None;
        Ok(self.clone_record(&operation.record))
    }
    pub(crate) fn resolve_commit(
        &self,
        operation: &PreparedCommit,
    ) -> Result<CommitResolution, StoreError> {
        operation.validate(&self.id)?;
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let latest = read_record(&self.path(), &self.id)?;
        if let Some(current) = &latest
            && current.last_commit_id == operation.record.last_commit_id
        {
            if current != &operation.record {
                return Err(StoreError::CommitIdentityConflict);
            }
            state.baseline = Some(self.clone_snapshot(&latest));
            state.pending = None;
            return Ok(CommitResolution::Applied(Box::new(
                self.clone_record(current),
            )));
        }
        let Some((pending, baseline)) = &state.pending else {
            return Err(StoreError::ConfirmationUnknown);
        };
        if pending != operation {
            return Err(
                if pending.record.last_commit_id == operation.record.last_commit_id {
                    StoreError::CommitIdentityConflict
                } else {
                    StoreError::PendingCommit
                },
            );
        }
        if latest == *baseline {
            return Ok(CommitResolution::NotApplied);
        }
        Err(StoreError::Conflict {
            expected: operation.expected_revision,
            actual: revision(&latest),
        })
    }
    /// Stale temp files are scoped by exact identity and removed only under lock.
    fn cleanup_pending(&self) -> Result<(), StoreError> {
        let prefix = format!(".{}.pending-", self.id);
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
                && entry.file_type()?.is_file()
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn set_fault_hook(&self, hook: Option<FaultHook>) {
        *self.hook.lock().unwrap() = hook;
    }
    fn hit(&self, stage: CommitStage) -> Result<(), StoreError> {
        #[cfg(test)]
        {
            let hook = self.hook.lock().unwrap().clone();
            if let Some(hook) = hook {
                match hook(stage) {
                    FaultAction::Continue => {}
                    FaultAction::Fail(error) => return Err(commit_error(stage, error)),
                }
            }
        }
        let _ = stage;
        Ok(())
    }
}
#[cfg(test)]
pub(crate) type FaultHook = Arc<dyn Fn(CommitStage) -> FaultAction + Send + Sync>;
#[cfg(test)]
pub(crate) enum FaultAction {
    Continue,
    Fail(io::Error),
}

fn revision(record: &Option<SavedSessionRecord>) -> Option<u64> {
    record.as_ref().map(|r| r.revision)
}
fn commit_error(stage: CommitStage, source: io::Error) -> StoreError {
    StoreError::CommitIo {
        stage,
        source: Arc::new(source),
    }
}
fn open_lock(directory: &Path, id: &str) -> Result<File, StoreError> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{id}.lock")))?)
}
fn acquire_lock(lock: &File, id: &str) -> Result<(), StoreError> {
    match lock.try_lock() {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(StoreError::Occupied(id.into())),
        Err(TryLockError::Error(error)) => Err(classify_lock_error(error)),
    }
}
fn classify_lock_error(error: io::Error) -> StoreError {
    if error.kind() == io::ErrorKind::Unsupported {
        StoreError::LockUnsupported(Arc::new(error))
    } else {
        StoreError::Io(Arc::new(error))
    }
}
fn read_record(path: &Path, id: &str) -> Result<Option<SavedSessionRecord>, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if file.metadata()?.len() > MAX_RECORD_BYTES as u64 {
        return Err(StoreError::RecordTooLarge {
            limit: MAX_RECORD_BYTES,
        });
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge {
            limit: MAX_RECORD_BYTES,
        });
    }
    let record: SavedSessionRecord = serde_json::from_slice(&bytes)?;
    record.validate(id)?;
    Ok(Some(record))
}
fn serialize_record(record: &SavedSessionRecord) -> Result<Vec<u8>, StoreError> {
    struct Bounded {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_RECORD_BYTES - self.bytes.len() {
                self.exceeded = true;
                return Err(io::Error::other("session record size limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Bounded {
        bytes: Vec::new(),
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut buffer, record);
    if buffer.exceeded {
        return Err(StoreError::RecordTooLarge {
            limit: MAX_RECORD_BYTES,
        });
    }
    result?;
    Ok(buffer.bytes)
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    #[test]
    fn unsupported_and_other_lock_errors_keep_distinct_original_causes() {
        let error = classify_lock_error(io::Error::new(
            io::ErrorKind::Unsupported,
            "fixture unsupported",
        ));
        assert!(
            matches!(error, StoreError::LockUnsupported(source) if source.kind() == io::ErrorKind::Unsupported)
        );
        let error = classify_lock_error(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture denied",
        ));
        assert!(
            matches!(error, StoreError::Io(source) if source.kind() == io::ErrorKind::PermissionDenied)
        );
    }
}
