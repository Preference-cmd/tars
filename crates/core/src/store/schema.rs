//! SQLite v3 schema definition, PRAGMA setup, and connection helpers.

use super::StoreError;
use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::path::Path;

/// Supported `PRAGMA user_version` of `<store_root>/tars.db`.
pub(crate) const SCHEMA_VERSION: u32 = 2;

const TRUSTED_WORKSPACES_DDL: &str = "
CREATE TABLE IF NOT EXISTS trusted_workspaces (
    workspace_hash TEXT PRIMARY KEY,
    workspace_root TEXT NOT NULL,
    trusted_at_ms  INTEGER NOT NULL
);";

const SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    session_id        TEXT PRIMARY KEY,
    revision          INTEGER NOT NULL CHECK (revision >= 0),
    last_commit_id    TEXT NOT NULL,
    policy_json       TEXT NOT NULL,
    entry_agent_id    TEXT NOT NULL,
    title             TEXT,
    permission_mode   TEXT,
    auto_compact      INTEGER,
    compact_threshold INTEGER,
    usage_json        TEXT,
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS agents (
    session_id         TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
    agent_id           TEXT NOT NULL,
    name               TEXT NOT NULL,
    preset_name        TEXT,
    interaction        TEXT NOT NULL CHECK (interaction IN ('single_work', 'reusable')),
    definition_json    TEXT NOT NULL,
    material_revision  INTEGER NOT NULL CHECK (material_revision >= 0),
    context_json       TEXT NOT NULL,
    history_index_json TEXT NOT NULL DEFAULT '[]',
    usage_json         TEXT,
    created_at_ms      INTEGER NOT NULL,
    updated_at_ms      INTEGER NOT NULL,
    PRIMARY KEY (session_id, agent_id)
);

CREATE TABLE IF NOT EXISTS operations (
    operation_id        TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL,
    target_agent_id     TEXT NOT NULL,
    parent_operation_id TEXT REFERENCES operations(operation_id),
    parent_agent_id     TEXT,
    source_block_id     TEXT,
    dedup_fingerprint   TEXT,
    execution_mode      TEXT NOT NULL CHECK (execution_mode IN ('sync', 'background')),
    on_parent_complete  TEXT NOT NULL CHECK (on_parent_complete IN ('continue', 'cancel')),
    depth               INTEGER NOT NULL CHECK (depth >= 0),
    status              TEXT NOT NULL CHECK (status IN (
                            'creating', 'running', 'completed', 'cancelled',
                            'failed', 'deadline', 'faulted', 'interrupted'
                        )),
    input_json          TEXT NOT NULL,
    outcome_json        TEXT,
    result_text         TEXT,
    result_bytes        INTEGER,
    created_at_ms       INTEGER NOT NULL,
    finished_at_ms      INTEGER,
    FOREIGN KEY (session_id, target_agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_operations_source_dedup
    ON operations(session_id, parent_operation_id, source_block_id)
    WHERE source_block_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_operations_session_target
    ON operations(session_id, target_agent_id, created_at_ms);

CREATE INDEX IF NOT EXISTS idx_operations_parent
    ON operations(parent_operation_id)
    WHERE parent_operation_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS agent_events (
    event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    target_agent_id TEXT NOT NULL,
    priority        TEXT NOT NULL CHECK (priority IN ('urgent', 'normal', 'low')),
    source_kind     TEXT NOT NULL CHECK (source_kind IN (
                        'agent_operation', 'background_command', 'timer', 'user_steer'
                    )),
    source_id       TEXT NOT NULL,
    coalesce_key    TEXT,
    wake_policy     TEXT NOT NULL CHECK (wake_policy IN (
                        'wake_if_idle', 'deliver_on_next_turn', 'suppress_wake'
                    )),
    payload_json    TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'delivered', 'dropped')),
    claimed_op_id   TEXT,
    created_at_ms   INTEGER NOT NULL,
    delivered_at_ms INTEGER,
    FOREIGN KEY (session_id, target_agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_agent_events_inbox
    ON agent_events(session_id, target_agent_id, state, event_id);

CREATE TABLE IF NOT EXISTS history_records (
    session_id    TEXT NOT NULL,
    agent_id      TEXT NOT NULL,
    record_id     TEXT NOT NULL,
    seq           INTEGER NOT NULL,
    kind          TEXT NOT NULL,
    summary_json  TEXT NOT NULL,
    document_json TEXT NOT NULL,
    PRIMARY KEY (session_id, agent_id, record_id),
    FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS artifacts (
    session_id  TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    content     BLOB NOT NULL,
    PRIMARY KEY (session_id, agent_id, artifact_id),
    FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS workspace_trust (
    workspace_hash TEXT NOT NULL,
    kind           TEXT NOT NULL CHECK (kind IN ('permissions_toml', 'workspace_agents')),
    workspace_root TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    content        TEXT NOT NULL,
    trusted_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (workspace_hash, kind)
);

CREATE TABLE IF NOT EXISTS trusted_workspaces (
    workspace_hash TEXT PRIMARY KEY,
    workspace_root TEXT NOT NULL,
    trusted_at_ms  INTEGER NOT NULL
);
"#;

/// Rolls back an active `BEGIN IMMEDIATE` transaction if dropped before [`Self::commit`].
pub(crate) struct ImmediateTx<'a> {
    conn: &'a Connection,
    active: bool,
}

impl<'a> ImmediateTx<'a> {
    /// Start an immediate SQLite write transaction.
    pub(crate) fn begin(conn: &'a Connection) -> Result<Self, StoreError> {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        Ok(Self { conn, active: true })
    }

    /// Commit the transaction and disarm rollback on drop.
    pub(crate) fn commit(mut self) -> Result<(), StoreError> {
        self.conn.execute_batch("COMMIT;")?;
        self.active = false;
        Ok(())
    }

    /// Explicitly roll back the transaction and disarm drop rollback.
    pub(crate) fn rollback(mut self) -> Result<(), StoreError> {
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("ROLLBACK;")?;
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for ImmediateTx<'_> {
    fn drop(&mut self) {
        if self.active && !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK;");
        }
    }
}

/// Apply standard connection PRAGMAs for a read-write SQLite connection.
fn apply_rw_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

/// Apply standard connection PRAGMAs for a read-only SQLite connection.
fn apply_ro_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA query_only = ON;",
    )?;
    Ok(())
}

/// Read `PRAGMA user_version` from an open connection.
pub(crate) fn read_user_version(conn: &Connection) -> Result<u32, StoreError> {
    let version: u32 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    Ok(version)
}

/// Open `<store_root>/tars.db` for read-write access, initializing or migrating schema to v2.
pub(crate) fn open_read_write(db_path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    apply_rw_pragmas(&conn)?;
    let version = read_user_version(&conn)?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == 0 {
        let tx = ImmediateTx::begin(&conn)?;
        let current = read_user_version(&conn)?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        if current == 0 {
            let existing_tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if existing_tables > 0 {
                return Err(StoreError::InvalidDatabase(
                    "database has user_version 0 with pre-existing tables".into(),
                ));
            }
            conn.execute_batch(SCHEMA_DDL)?;
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            tx.commit()?;
            return Ok(conn);
        }
        tx.rollback()?;
    }
    let version = read_user_version(&conn)?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == 1 {
        let tx = ImmediateTx::begin(&conn)?;
        let current = read_user_version(&conn)?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        if current == 1 {
            conn.execute_batch(TRUSTED_WORKSPACES_DDL)?;
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            tx.commit()?;
        } else {
            tx.rollback()?;
        }
    }
    Ok(conn)
}

/// Open `<store_root>/tars.db` read-only if it already exists, never creating it.
pub(crate) fn open_read_only(db_path: &Path) -> Result<Option<Connection>, StoreError> {
    match fs::metadata(db_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    apply_ro_pragmas(&conn)?;
    let version = read_user_version(&conn)?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == 0 {
        return Err(StoreError::InvalidDatabase(
            "SQLite store schema is uninitialized (user_version = 0)".into(),
        ));
    }
    Ok(Some(conn))
}
