//! SQLite v3 session store, per-session OS advisory locks, and atomic writer commits.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use causa::kernel::{BlockContent, BlockId, ContentPart, Context};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    CommitStage, PreparedCommit, SavedSessionRecord, StoreError,
    record::{
        RECORD_FORMAT, WireSessionUsage, canonical_uuid, derive_fork_title, validate_session_title,
    },
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
    pub(super) db_path: PathBuf,
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

    /// Read all managed `media:sha256:*` blobs referenced by `record.context` and
    /// entry-agent `history_records`, failing closed if any blob is missing.
    pub(crate) fn load_managed_media_blobs(
        &self,
        id: &str,
        record: &SavedSessionRecord,
    ) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        canonical_uuid(id)?;
        let refs = collect_record_managed_media_block_ids(record);
        if refs.is_empty() && record.history.is_empty() {
            return Ok(Vec::new());
        }
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            if let Some((first_ref, first_block_id)) = refs.into_iter().next() {
                return Err(missing_media_error(first_block_id, &first_ref));
            }
            return Ok(Vec::new());
        };
        load_managed_media_blobs_from_conn(&conn, id, id, record)
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

    /// Atomically set or clear the title of an unlocked saved session (`WF01`).
    ///
    /// Fails closed with [`StoreError::Occupied`] when another writer holds the
    /// session's OS advisory lock, or [`StoreError::NotFound`] when the store
    /// database or session row does not exist (never creating `tars.db`).
    pub(crate) fn rename_session(
        &self,
        id: &str,
        title: Option<&str>,
    ) -> Result<SavedSessionRecord, StoreError> {
        canonical_uuid(id)?;
        let validated_title = validate_session_title(title)?;
        if !self.db_path.exists() {
            return Err(StoreError::NotFound(id.to_owned()));
        }
        let lock_file_path = lock_path(&self.locks_dir, id);
        let lock_existed = lock_file_path.exists();
        fs::create_dir_all(&self.locks_dir)?;
        let lock = open_lock(&self.locks_dir, id)?;
        acquire_lock(&lock, id)?;
        let mut lock_guard =
            AdvisoryLockGuard::new(lock, (!lock_existed).then_some(lock_file_path));

        let conn = schema::open_read_write(&self.db_path)?;
        let tx = ImmediateTx::begin(&conn)?;
        let Some((existing, entry_agent_id)) = read_session_record_with_entry_agent(&conn, id)?
        else {
            return Err(StoreError::NotFound(id.to_owned()));
        };
        let new_revision = existing
            .revision
            .checked_add(1)
            .ok_or(StoreError::InvalidRevision)?;
        let revision_i64 = i64::try_from(new_revision)
            .map_err(|_| StoreError::InvalidDatabase("revision overflows i64".into()))?;
        let last_commit_id = uuid::Uuid::now_v7().to_string();
        let now_ms = crate::time::current_time_ms().max(existing.updated_at_ms);
        let updated_at_ms_i64 = i64::try_from(now_ms)
            .map_err(|_| StoreError::InvalidDatabase("updated_at_ms overflows i64".into()))?;

        conn.execute(
            "UPDATE sessions
             SET title = ?1, revision = ?2, last_commit_id = ?3, updated_at_ms = ?4
             WHERE session_id = ?5",
            params![
                validated_title,
                revision_i64,
                last_commit_id,
                updated_at_ms_i64,
                id,
            ],
        )?;
        conn.execute(
            "UPDATE agents
             SET updated_at_ms = ?1
             WHERE session_id = ?2 AND agent_id = ?3",
            params![updated_at_ms_i64, id, entry_agent_id],
        )?;
        tx.commit()?;
        lock_guard.set_cleanup_path(None);

        read_session_record(&conn, id)?.ok_or_else(|| {
            StoreError::InvalidDatabase(format!("renamed session {id} missing after commit"))
        })
    }

    /// Permanently delete an unlocked saved session and its owned rows (`WF02`).
    ///
    /// Unbinds `operations.parent_operation_id` before deleting the `sessions` row
    /// so nested child operations (`depth >= 2`) cascade cleanly without foreign-key
    /// failures. Never touches `workspace_trust`. Best-effort removes `<locks_dir>/<id>.lock`
    /// after releasing the OS lock.
    pub(crate) fn delete_session(&self, id: &str) -> Result<(), StoreError> {
        canonical_uuid(id)?;
        if !self.db_path.exists() {
            return Err(StoreError::NotFound(id.to_owned()));
        }
        let lock_file_path = lock_path(&self.locks_dir, id);
        let lock_existed = lock_file_path.exists();
        fs::create_dir_all(&self.locks_dir)?;
        let lock = open_lock(&self.locks_dir, id)?;
        acquire_lock(&lock, id)?;
        let mut lock_guard =
            AdvisoryLockGuard::new(lock, (!lock_existed).then_some(lock_file_path.clone()));

        let conn = schema::open_read_write(&self.db_path)?;
        let tx = ImmediateTx::begin(&conn)?;
        conn.execute(
            "UPDATE operations SET parent_operation_id = NULL WHERE session_id = ?1",
            params![id],
        )?;
        let affected = conn.execute("DELETE FROM sessions WHERE session_id = ?1", params![id])?;
        if affected == 0 {
            return Err(StoreError::NotFound(id.to_owned()));
        }
        if affected != 1 {
            return Err(StoreError::InvalidDatabase(format!(
                "expected 1 deleted session row for {id}, got {affected}"
            )));
        }
        tx.commit()?;
        drop(conn);
        lock_guard.set_cleanup_path(Some(lock_file_path));
        drop(lock_guard);
        Ok(())
    }

    /// Fork an unlocked saved session into a physically isolated new session (`WF03`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn fork_session(
        &self,
        source_id: &str,
        new_id: &str,
        title: Option<&str>,
    ) -> Result<SavedSessionRecord, StoreError> {
        self.fork_session_with_lock_check(source_id, new_id, title, true)
    }

    /// Fork a saved session into `new_id`, optionally verifying that `source_id`'s OS lock
    /// is currently unheld (`require_source_unlocked = true` for external/unopened callers;
    /// `false` when the calling host holds the lock and already verified idle/saved state).
    pub(crate) fn fork_session_with_lock_check(
        &self,
        source_id: &str,
        new_id: &str,
        title: Option<&str>,
        require_source_unlocked: bool,
    ) -> Result<SavedSessionRecord, StoreError> {
        canonical_uuid(source_id)?;
        canonical_uuid(new_id)?;
        if source_id == new_id {
            return Err(StoreError::InvalidIdentity(
                "source and target session identities must differ".into(),
            ));
        }
        let explicit_title = validate_session_title(title)?;
        if !self.db_path.exists() {
            return Err(StoreError::NotFound(source_id.to_owned()));
        }
        if require_source_unlocked && check_lock_active(&self.locks_dir, source_id)? {
            return Err(StoreError::Occupied(source_id.to_owned()));
        }

        let new_lock_path = lock_path(&self.locks_dir, new_id);
        let new_lock_existed = new_lock_path.exists();
        fs::create_dir_all(&self.locks_dir)?;
        let new_lock = open_lock(&self.locks_dir, new_id)?;
        acquire_lock(&new_lock, new_id)?;
        let mut lock_guard =
            AdvisoryLockGuard::new(new_lock, (!new_lock_existed).then_some(new_lock_path));

        let conn = schema::open_read_write(&self.db_path)?;
        let tx = ImmediateTx::begin(&conn)?;
        let new_exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sessions WHERE session_id = ?1",
                params![new_id],
                |row| row.get(0),
            )
            .optional()?;
        if new_exists.is_some() {
            return Err(StoreError::AlreadyExists(new_id.to_owned()));
        }

        let Some((existing, source_entry_agent_id)) =
            read_session_record_with_entry_agent(&conn, source_id)?
        else {
            return Err(StoreError::NotFound(source_id.to_owned()));
        };

        let forked_title = explicit_title.or_else(|| derive_fork_title(existing.title.as_deref()));
        let permission_mode = match existing.permission_mode {
            Some(PermissionMode::FullAccess)
                if existing.definition.initial_permission_mode() != PermissionMode::FullAccess =>
            {
                Some(PermissionMode::Ask)
            }
            other => other,
        };
        let now_ms = crate::time::current_time_ms();
        let last_commit_id = uuid::Uuid::now_v7().to_string();
        let new_record = SavedSessionRecord {
            format: RECORD_FORMAT.into(),
            conversation_id: new_id.to_owned(),
            revision: 1,
            last_commit_id,
            definition: existing.definition,
            context: existing.context,
            title: forked_title,
            updated_at_ms: now_ms,
            permission_mode,
            auto_compact: existing.auto_compact,
            compact_threshold: existing.compact_threshold,
            history: existing.history,
            usage: None,
        };
        new_record.validate(new_id)?;
        let serialized = serialize_record_columns(&new_record)?;
        upsert_session_and_entry_agent(&conn, &new_record, &serialized)?;

        {
            let mut stmt = conn.prepare(
                "SELECT record_id, seq, kind, summary_json, document_json
                 FROM history_records
                 WHERE session_id = ?1 AND agent_id = ?2
                 ORDER BY seq ASC",
            )?;
            let mut rows = stmt.query(params![source_id, &source_entry_agent_id])?;
            let mut insert_stmt = conn.prepare(
                "INSERT INTO history_records (
                    session_id, agent_id, record_id, seq, kind, summary_json, document_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            while let Some(row) = rows.next()? {
                let record_id: String = row.get(0)?;
                let seq: i64 = row.get(1)?;
                let kind: String = row.get(2)?;
                let summary_json: String = row.get(3)?;
                let document_json: String = row.get(4)?;
                if document_json.len() > crate::compact::history::MAX_HISTORY_BYTES {
                    return Err(StoreError::HistoryTooLarge {
                        limit: crate::compact::history::MAX_HISTORY_BYTES,
                    });
                }
                let mut doc: crate::compact::HistoryRecord = serde_json::from_str(&document_json)?;
                doc.validate(source_id, &record_id)?;
                doc.conversation_id = new_id.to_owned();
                let new_document_json = serde_json::to_string(&doc)?;
                if new_document_json.len() > crate::compact::history::MAX_HISTORY_BYTES {
                    return Err(StoreError::HistoryTooLarge {
                        limit: crate::compact::history::MAX_HISTORY_BYTES,
                    });
                }
                insert_stmt.execute(params![
                    new_id,
                    new_id,
                    record_id,
                    seq,
                    kind,
                    summary_json,
                    new_document_json,
                ])?;
            }
        }

        conn.execute(
            "INSERT INTO artifacts (session_id, agent_id, artifact_id, content)
             SELECT ?1, ?1, artifact_id, content
             FROM artifacts
             WHERE session_id = ?2 AND agent_id = ?3",
            params![new_id, source_id, &source_entry_agent_id],
        )?;

        tx.commit()?;
        lock_guard.set_cleanup_path(None);

        read_session_record(&conn, new_id)?.ok_or_else(|| {
            StoreError::InvalidDatabase(format!("forked session {new_id} missing after commit"))
        })
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

/// Which kind of uncommitted writer operation is currently retained on `Writer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingWriterKind {
    /// No pending write is retained on the writer.
    None,
    /// A root snapshot/session commit is pending confirmation/retry.
    RootCommit,
    /// An operation admission is pending confirmation/retry.
    Admit,
    /// A multi-agent outbox commit is pending confirmation/retry.
    Outbox,
}

// The outer Option distinguishes an unread baseline from confirmed absence.
#[derive(Default)]
pub(super) struct WriterState {
    pub(super) baseline: Option<Option<SavedSessionRecord>>,
    pub(super) pending: Option<(PreparedCommit, Option<SavedSessionRecord>)>,
    pub(super) pending_admit: Option<(
        super::AdmitOperationRequest,
        Option<super::AdmitOperationOutcome>,
    )>,
    pub(super) pending_outbox: Option<(
        super::OutboxCommitRequest,
        Option<super::OutboxCommitResult>,
    )>,
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

    pub(super) fn session_id(&self) -> &str {
        &self.id
    }

    pub(super) fn lock_state_and_conn(
        &self,
    ) -> Result<
        (
            std::sync::MutexGuard<'_, WriterState>,
            std::sync::MutexGuard<'_, Connection>,
        ),
        StoreError,
    > {
        let state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        Ok((state, conn))
    }

    pub(super) fn record_write_bytes(&self, bytes: usize) {
        self.counters
            .bytes_written
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
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

    /// Read all managed `media:sha256:*` blobs referenced by `record.context` and
    /// entry-agent `history_records`, failing closed if any blob is missing.
    pub(crate) fn load_managed_media_blobs(
        &self,
        record: &SavedSessionRecord,
    ) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        load_managed_media_blobs_from_conn(&conn, &self.id, &self.id, record)
    }

    #[allow(dead_code)]
    pub(crate) fn commit(
        &self,
        operation: &PreparedCommit,
    ) -> Result<SavedSessionRecord, StoreError> {
        self.commit_with_consumed_events(operation, &self.id, &[], &[])
    }

    /// Atomically commit `operation` and mark `delivered_ids` / `dropped_ids` consumed
    /// inside the exact same `BEGIN IMMEDIATE` transaction.
    pub(crate) fn commit_with_consumed_events(
        &self,
        operation: &PreparedCommit,
        target_agent_id: &str,
        delivered_ids: &[i64],
        dropped_ids: &[i64],
    ) -> Result<SavedSessionRecord, StoreError> {
        self.commit_with_consumed_events_and_media(
            operation,
            target_agent_id,
            delivered_ids,
            dropped_ids,
            &[],
        )
    }

    /// Atomically commit `operation`, mark `delivered_ids` / `dropped_ids` consumed,
    /// persist `media_blobs` into `artifacts`, and verify all managed media references
    /// in `operation.record.context` exist inside the exact same `BEGIN IMMEDIATE` transaction.
    pub(crate) fn commit_with_consumed_events_and_media(
        &self,
        operation: &PreparedCommit,
        target_agent_id: &str,
        delivered_ids: &[i64],
        dropped_ids: &[i64],
        media_blobs: &[(String, Arc<[u8]>)],
    ) -> Result<SavedSessionRecord, StoreError> {
        operation.validate(&self.id)?;
        canonical_uuid(target_agent_id)?;
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let conn = self.conn.lock().map_err(|_| StoreError::Poisoned)?;
        if state.pending_admit.is_some() || state.pending_outbox.is_some() {
            return Err(StoreError::PendingCommit);
        }
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
        let updated_at_ms_i64 = i64::try_from(operation.record.updated_at_ms)
            .map_err(|_| StoreError::InvalidDatabase("updated_at_ms overflows i64".into()))?;

        self.hit(CommitStage::Write)?;
        let tx = ImmediateTx::begin(&conn)?;
        for &event_id in delivered_ids {
            super::multi_agent::validate_event_ownership(
                &conn,
                &self.id,
                event_id,
                Some(target_agent_id),
            )?;
        }
        for &event_id in dropped_ids {
            super::multi_agent::validate_event_ownership(
                &conn,
                &self.id,
                event_id,
                Some(target_agent_id),
            )?;
        }
        upsert_session_and_entry_agent(&conn, &operation.record, &serialized)?;
        persist_and_verify_media_in_tx(&conn, &self.id, &operation.record.context, media_blobs)?;
        super::multi_agent::mark_events_consumed_in_tx(
            &conn,
            &self.id,
            delivered_ids,
            dropped_ids,
            updated_at_ms_i64,
        )?;
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
        if state.pending_admit.is_some() || state.pending_outbox.is_some() {
            return Err(StoreError::PendingCommit);
        }
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

    pub(crate) fn pending_kind(&self) -> PendingWriterKind {
        let Ok(state) = self.state.lock() else {
            return PendingWriterKind::None;
        };
        if state.pending.is_some() {
            PendingWriterKind::RootCommit
        } else if state.pending_admit.is_some() {
            PendingWriterKind::Admit
        } else if state.pending_outbox.is_some() {
            PendingWriterKind::Outbox
        } else {
            PendingWriterKind::None
        }
    }

    pub(super) fn hit(&self, stage: CommitStage) -> Result<(), StoreError> {
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
    read_session_record_with_entry_agent(conn, id).map(|opt| opt.map(|(record, _)| record))
}

fn read_session_record_with_entry_agent(
    conn: &Connection,
    id: &str,
) -> Result<Option<(SavedSessionRecord, String)>, StoreError> {
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
    raw.map(|row| {
        let entry_agent_id = row.entry_agent_id.clone();
        decode_session_row(row, id).map(|record| (record, entry_agent_id))
    })
    .transpose()
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

fn collect_blocks_managed_media_block_ids(
    blocks: &[causa::kernel::ContextBlock],
    into: &mut BTreeMap<String, BlockId>,
) {
    for block in blocks {
        match block.content() {
            BlockContent::Message { parts, .. } => {
                for part in parts {
                    if let ContentPart::Media(media) = part
                        && crate::media::is_managed_media_ref(&media.reference)
                    {
                        into.entry(media.reference.clone()).or_insert(block.id());
                    }
                }
            }
            BlockContent::ToolResult(result) => {
                for media in &result.media {
                    if crate::media::is_managed_media_ref(&media.reference) {
                        into.entry(media.reference.clone()).or_insert(block.id());
                    }
                }
            }
            BlockContent::ToolCall(_) | BlockContent::ToolChange(_) => {}
        }
    }
}

fn collect_record_managed_media_block_ids(
    record: &SavedSessionRecord,
) -> BTreeMap<String, BlockId> {
    let mut map = BTreeMap::new();
    collect_blocks_managed_media_block_ids(record.context.blocks(), &mut map);
    map
}

fn missing_media_error(block_id: BlockId, reference: &str) -> StoreError {
    StoreError::References(Arc::new(
        crate::material::MaterialError::ReferenceUnavailable {
            block_id,
            source: io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing managed media blob {reference}"),
            ),
        },
    ))
}

fn persist_and_verify_media_in_tx(
    conn: &Connection,
    session_id: &str,
    context: &Context,
    media_blobs: &[(String, Arc<[u8]>)],
) -> Result<(), StoreError> {
    for (reference, bytes) in media_blobs {
        if crate::media::parse_managed_media_sha256(reference).is_none() {
            return Err(StoreError::InvalidDatabase(format!(
                "invalid managed media reference: {reference}"
            )));
        }
        conn.execute(
            "INSERT OR IGNORE INTO artifacts (session_id, agent_id, artifact_id, content)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, session_id, reference, bytes.as_ref()],
        )?;
    }
    let mut required = BTreeMap::new();
    collect_blocks_managed_media_block_ids(context.blocks(), &mut required);
    for (reference, block_id) in required {
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM artifacts
                 WHERE session_id = ?1 AND agent_id = ?2 AND artifact_id = ?3",
                params![session_id, session_id, reference],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(missing_media_error(block_id, &reference));
        }
    }
    Ok(())
}

fn load_managed_media_blobs_from_conn(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
    record: &SavedSessionRecord,
) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
    let mut refs = BTreeMap::new();
    collect_blocks_managed_media_block_ids(record.context.blocks(), &mut refs);
    for entry in &record.history {
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT length(CAST(document_json AS BLOB)), document_json
                 FROM history_records
                 WHERE session_id = ?1 AND agent_id = ?2 AND record_id = ?3",
                params![session_id, agent_id, &entry.record_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((len_i64, document_json)) = row {
            let len = u64::try_from(len_i64).map_err(|_| {
                StoreError::InvalidDatabase("negative history record length".into())
            })?;
            if len > crate::compact::history::MAX_HISTORY_BYTES as u64 {
                return Err(StoreError::HistoryTooLarge {
                    limit: crate::compact::history::MAX_HISTORY_BYTES,
                });
            }
            let doc: crate::compact::HistoryRecord = serde_json::from_str(&document_json)?;
            doc.validate(session_id, &entry.record_id)?;
            for replacement in &doc.replacements {
                collect_blocks_managed_media_block_ids(&replacement.originals, &mut refs);
            }
        }
    }
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let mut blobs = Vec::with_capacity(refs.len());
    for (reference, block_id) in refs {
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT CAST(content AS BLOB) FROM artifacts
                 WHERE session_id = ?1 AND agent_id = ?2 AND artifact_id = ?3",
                params![session_id, agent_id, &reference],
                |row| row.get(0),
            )
            .optional()?;
        let Some(bytes) = bytes else {
            return Err(missing_media_error(block_id, &reference));
        };
        blobs.push((reference, bytes));
    }
    Ok(blobs)
}

#[cfg(test)]
fn commit_error(stage: CommitStage, source: io::Error) -> StoreError {
    StoreError::CommitIo {
        stage,
        source: Arc::new(source),
    }
}

fn lock_path(locks_dir: &Path, id: &str) -> PathBuf {
    locks_dir.join(format!("{id}.lock"))
}

struct AdvisoryLockGuard {
    file: Option<File>,
    cleanup_path: Option<PathBuf>,
}

impl AdvisoryLockGuard {
    fn new(file: File, cleanup_path: Option<PathBuf>) -> Self {
        Self {
            file: Some(file),
            cleanup_path,
        }
    }

    fn set_cleanup_path(&mut self, path: Option<PathBuf>) {
        self.cleanup_path = path;
    }
}

impl Drop for AdvisoryLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        if let Some(path) = self.cleanup_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn open_lock(locks_dir: &Path, id: &str) -> Result<File, StoreError> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(locks_dir, id))?)
}

fn check_lock_active(locks_dir: &Path, id: &str) -> Result<bool, StoreError> {
    let path = lock_path(locks_dir, id);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    match acquire_lock(&file, id) {
        Ok(()) => {
            let _ = file.unlock();
            Ok(false)
        }
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
