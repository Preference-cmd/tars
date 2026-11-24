//! SQLite v3 session store, per-session OS advisory locks, and atomic writer commits.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use causa::kernel::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    CommitStage, PreparedCommit, SavedSessionRecord, StoreError,
    record::{RECORD_FORMAT, WireSessionUsage, canonical_uuid},
    schema::{self, ImmediateTx},
};
use crate::compact::HistoryRecordEntry;
use crate::definition::SessionDefinition;
use crate::policy::PermissionMode;

/// A resource limit, not a performance claim or a material-history limit.
pub(crate) const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredSessionPolicy {
    pub(super) format: String,
}

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

/// Persistent SQLite v3 store rooted at `<store_root>`.
///
/// Layout:
/// - `<store_root>/tars.db` (WAL database with 7-table schema v1)
/// - `<store_root>/locks/<session_id>.lock` (OS advisory write lock per session)
#[derive(Clone)]
pub(crate) struct SessionStore {
    root: PathBuf,
    db_path: PathBuf,
    locks_dir: PathBuf,
    counters: Arc<Counters>,
    #[cfg(test)]
    hook: Arc<Mutex<Option<FaultHook>>>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn normalize_root(root: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&root) {
        return canonical;
    }
    let mut normalized = PathBuf::new();
    for component in root.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[derive(Debug)]
pub(crate) struct ListedSession {
    pub(crate) conversation_id: String,
    pub(crate) record: Result<SavedSessionRecord, StoreError>,
    pub(crate) active: Result<bool, StoreError>,
}

impl SessionStore {
    /// Construct a store view bound to `root` without creating any files.
    pub(crate) fn new(root: PathBuf) -> Self {
        let root = std::path::absolute(&root).unwrap_or(root);
        let root = normalize_root(root);
        let db_path = root.join("tars.db");
        let locks_dir = root.join("locks");
        Self {
            root,
            db_path,
            locks_dir,
            counters: Arc::default(),
            #[cfg(test)]
            hook: Arc::default(),
        }
    }

    /// Normalized store root directory (`<store_root>`).
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Path to `<store_root>/tars.db`.
    pub(crate) fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Path to `<store_root>/locks`.
    #[cfg(test)]
    pub(crate) fn locks_dir(&self) -> &Path {
        &self.locks_dir
    }

    /// Reads and validates one saved record without locking or mutating any files.
    pub(crate) fn read_saved(&self, id: &str) -> Result<Option<SavedSessionRecord>, StoreError> {
        canonical_uuid(id)?;
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            return Ok(None);
        };
        read_session_record(&conn, id)
    }

    #[cfg(test)]
    pub(crate) fn set_fault_hook(&self, hook: Option<FaultHook>) {
        *self.hook.lock().unwrap() = hook;
    }

    /// Acquire the OS advisory write lock for `id` and open a read-write SQLite connection.
    pub(crate) fn acquire_writer(&self, id: &str) -> Result<Writer, StoreError> {
        canonical_uuid(id)?;
        fs::create_dir_all(&self.locks_dir)?;
        let lock = open_lock(&self.locks_dir, id)?;
        acquire_lock(&lock, id)?;
        let conn = schema::open_read_write(&self.db_path)?;
        Ok(Writer {
            id: id.into(),
            conn: Mutex::new(conn),
            _lock: lock,
            state: Mutex::new(WriterState::default()),
            counters: self.counters.clone(),
            #[cfg(test)]
            hook: Mutex::new(self.hook.lock().unwrap().clone()),
        })
    }

    /// List all persisted sessions via a lock-free WAL read-only snapshot.
    pub(crate) fn list(&self) -> Result<Vec<ListedSession>, StoreError> {
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT
                s.session_id,
                s.revision,
                s.last_commit_id,
                s.policy_json,
                s.entry_agent_id,
                s.title,
                s.permission_mode,
                s.auto_compact,
                s.compact_threshold,
                s.usage_json,
                s.updated_at_ms,
                a.agent_id,
                a.definition_json,
                a.context_json,
                a.history_index_json,
                a.usage_json
             FROM sessions s
             LEFT JOIN agents a
               ON s.session_id = a.session_id
              AND s.entry_agent_id = a.agent_id
             ORDER BY s.session_id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut raw_rows = Vec::new();
        while let Some(row) = rows.next()? {
            let session_id = match row.get_ref(0)? {
                rusqlite::types::ValueRef::Text(bytes) => {
                    String::from_utf8_lossy(bytes).into_owned()
                }
                rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                rusqlite::types::ValueRef::Real(r) => r.to_string(),
                rusqlite::types::ValueRef::Blob(_) => "<blob>".to_owned(),
                rusqlite::types::ValueRef::Null => "<null>".to_owned(),
            };
            let raw = RawSessionRow::from_row(row).map_err(StoreError::from);
            raw_rows.push((session_id, raw));
        }

        let mut result = Vec::with_capacity(raw_rows.len());
        for (id, raw_result) in raw_rows {
            let record = canonical_uuid(&id)
                .and_then(|()| raw_result.and_then(|raw| decode_session_row(raw, &id)));
            let active = (|| {
                canonical_uuid(&id)?;
                check_lock_active(&self.locks_dir, &id)
            })();
            result.push(ListedSession {
                conversation_id: id,
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

/// Exclusive per-session writer holding the OS advisory lock and a SQLite connection.
pub(crate) struct Writer {
    id: String,
    conn: Mutex<Connection>,
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

    pub(super) fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        f(&conn)
    }

    pub(crate) fn read_latest(&self) -> Result<Option<SavedSessionRecord>, StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        let record = read_session_record(&conn, &self.id)?;
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
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        let latest = match read_session_record(&conn, &self.id) {
            Ok(latest) => latest,
            Err(error) => {
                // Keep the exact attempted operation even when the pre-write
                // read fails. A later confirmation can compare its known
                // baseline; a newly opened writer cannot infer this history.
                if state.pending.is_none()
                    && let Some(baseline) = &state.baseline
                    && revision(baseline) == operation.expected_revision
                {
                    state.pending = Some((
                        PreparedCommit {
                            expected_revision: operation.expected_revision,
                            record: self.clone_record(&operation.record),
                        },
                        self.clone_snapshot(baseline),
                    ));
                }
                return Err(error);
            }
        };
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
        let serialized = serialize_record_columns(&operation.record)?;

        self.hit(CommitStage::Write)?;
        let tx = ImmediateTx::begin(&conn)?;
        upsert_session_and_entry_agent(&conn, &operation.record, &serialized)?;
        self.counters
            .bytes_written
            .fetch_add(serialized.total_bytes as u64, Ordering::Relaxed);
        self.counters.writes.fetch_add(1, Ordering::Relaxed);

        self.hit(CommitStage::Sync)?;
        self.hit(CommitStage::Replace)?;
        tx.commit()?;

        self.hit(CommitStage::Confirm)?;
        let confirmed = read_session_record(&conn, &self.id)?;
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
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        let latest = read_session_record(&conn, &self.id)?;
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

struct RawSessionRow {
    session_id: String,
    revision: i64,
    last_commit_id: String,
    policy_json: String,
    entry_agent_id: String,
    title: Option<String>,
    permission_mode: Option<String>,
    auto_compact: Option<i64>,
    compact_threshold: Option<i64>,
    session_usage_json: Option<String>,
    updated_at_ms: i64,
    agent_id: Option<String>,
    definition_json: Option<String>,
    context_json: Option<String>,
    history_index_json: Option<String>,
    agent_usage_json: Option<String>,
}

impl RawSessionRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            session_id: row.get(0)?,
            revision: row.get(1)?,
            last_commit_id: row.get(2)?,
            policy_json: row.get(3)?,
            entry_agent_id: row.get(4)?,
            title: row.get(5)?,
            permission_mode: row.get(6)?,
            auto_compact: row.get(7)?,
            compact_threshold: row.get(8)?,
            session_usage_json: row.get(9)?,
            updated_at_ms: row.get(10)?,
            agent_id: row.get(11)?,
            definition_json: row.get(12)?,
            context_json: row.get(13)?,
            history_index_json: row.get(14)?,
            agent_usage_json: row.get(15)?,
        })
    }
}

pub(super) struct SerializedRecordColumns {
    pub(super) policy_json: String,
    pub(super) definition_json: String,
    pub(super) context_json: String,
    pub(super) history_index_json: String,
    pub(super) usage_json: Option<String>,
    pub(super) total_bytes: usize,
}

pub(super) fn serialize_record_columns(
    record: &SavedSessionRecord,
) -> Result<SerializedRecordColumns, StoreError> {
    struct BoundedCounter {
        remaining: usize,
        written: usize,
        exceeded: bool,
    }
    impl Write for BoundedCounter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.len() > self.remaining {
                self.exceeded = true;
                return Err(io::Error::other("session record size limit"));
            }
            self.remaining -= buf.len();
            self.written += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn serialize_bounded<T: Serialize>(
        value: &T,
        remaining: &mut usize,
    ) -> Result<String, StoreError> {
        struct BoundedBuf {
            bytes: Vec<u8>,
            remaining: usize,
            exceeded: bool,
        }
        impl Write for BoundedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if buf.len() > self.remaining {
                    self.exceeded = true;
                    return Err(io::Error::other("session record size limit"));
                }
                self.remaining -= buf.len();
                self.bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut buf = BoundedBuf {
            bytes: Vec::new(),
            remaining: *remaining,
            exceeded: false,
        };
        let res = serde_json::to_writer(&mut buf, value);
        if buf.exceeded {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_RECORD_BYTES,
            });
        }
        res?;
        *remaining = buf.remaining;
        String::from_utf8(buf.bytes)
            .map_err(|e| StoreError::InvalidDatabase(format!("non-UTF-8 JSON serialization: {e}")))
    }

    // Check full wire payload footprint first to match MAX_RECORD_BYTES strictly.
    let mut counter = BoundedCounter {
        remaining: MAX_RECORD_BYTES,
        written: 0,
        exceeded: false,
    };
    let wire_res = serde_json::to_writer(&mut counter, record);
    if counter.exceeded {
        return Err(StoreError::RecordTooLarge {
            limit: MAX_RECORD_BYTES,
        });
    }
    wire_res?;

    let mut remaining = MAX_RECORD_BYTES;
    let policy_json = serialize_bounded(
        &StoredSessionPolicy {
            format: record.format.clone(),
        },
        &mut remaining,
    )?;
    let definition_json = serialize_bounded(&record.definition, &mut remaining)?;
    let context_json = serialize_bounded(&record.context, &mut remaining)?;
    let history_index_json = serialize_bounded(&record.history, &mut remaining)?;
    let usage_json = match &record.usage {
        Some(usage) => Some(serialize_bounded(usage, &mut remaining)?),
        None => None,
    };
    let total_bytes = MAX_RECORD_BYTES - remaining;
    Ok(SerializedRecordColumns {
        policy_json,
        definition_json,
        context_json,
        history_index_json,
        usage_json,
        total_bytes,
    })
}

pub(super) fn upsert_session_and_entry_agent(
    conn: &Connection,
    record: &SavedSessionRecord,
    columns: &SerializedRecordColumns,
) -> Result<(), StoreError> {
    let created_at_ms =
        crate::time::created_at_from_id(&record.conversation_id).unwrap_or(record.updated_at_ms);
    let created_at_ms_i64 = i64::try_from(created_at_ms)
        .map_err(|_| StoreError::InvalidDatabase("created_at_ms overflows i64".into()))?;
    let updated_at_ms_i64 = i64::try_from(record.updated_at_ms)
        .map_err(|_| StoreError::InvalidDatabase("updated_at_ms overflows i64".into()))?;
    let revision_i64 = i64::try_from(record.revision)
        .map_err(|_| StoreError::InvalidDatabase("revision overflows i64".into()))?;
    let permission_mode = record.permission_mode.map(|mode| mode.as_str());
    let auto_compact = record.auto_compact.map(i64::from);
    let compact_threshold = record.compact_threshold.map(i64::from);

    conn.execute(
        "INSERT INTO sessions (
            session_id, revision, last_commit_id, policy_json, entry_agent_id,
            title, permission_mode, auto_compact, compact_threshold,
            usage_json, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(session_id) DO UPDATE SET
            revision = excluded.revision,
            last_commit_id = excluded.last_commit_id,
            policy_json = excluded.policy_json,
            entry_agent_id = excluded.entry_agent_id,
            title = excluded.title,
            permission_mode = excluded.permission_mode,
            auto_compact = excluded.auto_compact,
            compact_threshold = excluded.compact_threshold,
            usage_json = excluded.usage_json,
            updated_at_ms = excluded.updated_at_ms",
        params![
            record.conversation_id,
            revision_i64,
            record.last_commit_id,
            columns.policy_json,
            record.conversation_id,
            record.title,
            permission_mode,
            auto_compact,
            compact_threshold,
            columns.usage_json,
            created_at_ms_i64,
            updated_at_ms_i64,
        ],
    )?;

    conn.execute(
        "INSERT INTO agents (
            session_id, agent_id, name, preset_name, interaction,
            definition_json, material_revision, context_json, history_index_json,
            usage_json, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, 'main', NULL, 'reusable', ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(session_id, agent_id) DO UPDATE SET
            definition_json = excluded.definition_json,
            material_revision = excluded.material_revision,
            context_json = excluded.context_json,
            history_index_json = excluded.history_index_json,
            usage_json = excluded.usage_json,
            updated_at_ms = excluded.updated_at_ms",
        params![
            record.conversation_id,
            record.conversation_id,
            columns.definition_json,
            revision_i64,
            columns.context_json,
            columns.history_index_json,
            columns.usage_json,
            created_at_ms_i64,
            updated_at_ms_i64,
        ],
    )?;
    Ok(())
}

pub(super) fn read_session_record(
    conn: &Connection,
    id: &str,
) -> Result<Option<SavedSessionRecord>, StoreError> {
    let raw = conn
        .query_row(
            "SELECT
                s.session_id,
                s.revision,
                s.last_commit_id,
                s.policy_json,
                s.entry_agent_id,
                s.title,
                s.permission_mode,
                s.auto_compact,
                s.compact_threshold,
                s.usage_json,
                s.updated_at_ms,
                a.agent_id,
                a.definition_json,
                a.context_json,
                a.history_index_json,
                a.usage_json
             FROM sessions s
             LEFT JOIN agents a
               ON s.session_id = a.session_id
              AND s.entry_agent_id = a.agent_id
             WHERE s.session_id = ?1",
            params![id],
            RawSessionRow::from_row,
        )
        .optional()?;
    raw.map(|row| decode_session_row(row, id)).transpose()
}

fn decode_session_row(
    row: RawSessionRow,
    expected_id: &str,
) -> Result<SavedSessionRecord, StoreError> {
    let policy: StoredSessionPolicy = serde_json::from_str(&row.policy_json)?;
    if policy.format != RECORD_FORMAT {
        return Err(StoreError::UnsupportedFormat(policy.format));
    }

    let definition_json = row.definition_json.ok_or_else(|| {
        StoreError::InvalidDatabase(format!(
            "missing entry agent row for session {}",
            row.session_id
        ))
    })?;
    let context_json = row.context_json.ok_or_else(|| {
        StoreError::InvalidDatabase(format!(
            "missing context for entry agent of session {}",
            row.session_id
        ))
    })?;
    let history_index_json = row.history_index_json.unwrap_or_else(|| "[]".to_owned());
    let _ = row.agent_id;
    let _ = row.entry_agent_id;

    let usage_json = row.session_usage_json.or(row.agent_usage_json);
    let total_bytes = row.policy_json.len()
        + definition_json.len()
        + context_json.len()
        + history_index_json.len()
        + usage_json.as_deref().map_or(0, str::len);
    if total_bytes > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge {
            limit: MAX_RECORD_BYTES,
        });
    }

    let revision = u64::try_from(row.revision).map_err(|_| StoreError::InvalidRevision)?;
    if revision == 0 {
        return Err(StoreError::InvalidRevision);
    }
    let updated_at_ms = u64::try_from(row.updated_at_ms)
        .map_err(|_| StoreError::InvalidDatabase("negative updated_at_ms".into()))?;

    let permission_mode = row
        .permission_mode
        .as_deref()
        .map(|raw| {
            raw.parse::<PermissionMode>().map_err(|error| {
                StoreError::InvalidDatabase(format!("invalid permission_mode: {error}"))
            })
        })
        .transpose()?;

    let auto_compact = match row.auto_compact {
        None => None,
        Some(0) => Some(false),
        Some(1) => Some(true),
        Some(other) => {
            return Err(StoreError::InvalidDatabase(format!(
                "invalid auto_compact flag: {other}"
            )));
        }
    };

    let compact_threshold = row
        .compact_threshold
        .map(|value| {
            u8::try_from(value).map_err(|_| {
                StoreError::InvalidDatabase(format!("invalid compact_threshold: {value}"))
            })
        })
        .transpose()?;

    let definition: SessionDefinition = serde_json::from_str(&definition_json)?;
    let context: Context = crate::material::decode_stored(context_json.as_bytes())?;
    let history: Vec<HistoryRecordEntry> = serde_json::from_str(&history_index_json)?;
    let usage: Option<WireSessionUsage> = usage_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;

    let record = SavedSessionRecord {
        format: policy.format,
        conversation_id: row.session_id,
        revision,
        last_commit_id: row.last_commit_id,
        definition,
        context,
        title: row.title,
        updated_at_ms,
        permission_mode,
        auto_compact,
        compact_threshold,
        history,
        usage,
    };
    record.validate(expected_id)?;
    Ok(record)
}

fn revision(record: &Option<SavedSessionRecord>) -> Option<u64> {
    record.as_ref().map(|r| r.revision)
}

#[cfg(test)]
fn commit_error(stage: CommitStage, source: io::Error) -> StoreError {
    StoreError::CommitIo {
        stage,
        source: Arc::new(source),
    }
}

fn open_lock(locks_dir: &Path, id: &str) -> Result<File, StoreError> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(locks_dir.join(format!("{id}.lock")))?)
}

fn check_lock_active(locks_dir: &Path, id: &str) -> Result<bool, StoreError> {
    let path = locks_dir.join(format!("{id}.lock"));
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match acquire_lock(&file, id) {
        Ok(()) => Ok(false),
        Err(StoreError::Occupied(_)) => Ok(true),
        Err(error) => Err(error),
    }
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
