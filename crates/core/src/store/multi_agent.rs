//! Multi-agent rows, operation admission, transactional outbox commits,
//! and lock-free WAL snapshot queries on [`SessionStore`] and [`Writer`].

#![allow(dead_code)]

use super::{
    CommitStage, MAX_RECORD_BYTES, SessionStore, StoreError, Writer,
    record::{WireSessionUsage, canonical_uuid},
    schema::{self, ImmediateTx},
};
use crate::definition::SessionDefinition;
use crate::events::{
    AgentInteraction, EventDeliveryState, EventPriority, EventSourceKind, InboxEvent,
    MAX_OPERATION_INPUT_BYTES, NewInboxEvent, OnParentComplete, OperationExecutionMode,
    OperationStatus, Utf8PageSlice, WakePolicy, slice_utf8_page,
};
use causa::kernel::Context;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::sync::Arc;

/// Persisted `agents` table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredAgentRow {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    pub(crate) name: String,
    pub(crate) preset_name: Option<String>,
    pub(crate) interaction: AgentInteraction,
    pub(crate) definition_json: String,
    pub(crate) material_revision: u64,
    pub(crate) context_json: String,
    pub(crate) history_index_json: String,
    pub(crate) usage_json: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
}

/// Pre-insert agent material row created atomically with a new operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewAgentRow {
    pub(crate) agent_id: String,
    pub(crate) name: String,
    pub(crate) preset_name: Option<String>,
    pub(crate) interaction: AgentInteraction,
    pub(crate) definition_json: String,
    pub(crate) context_json: String,
    pub(crate) history_index_json: String,
    pub(crate) usage_json: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
}

/// Persisted `operations` table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredOperationRow {
    pub(crate) operation_id: String,
    pub(crate) session_id: String,
    pub(crate) target_agent_id: String,
    pub(crate) parent_operation_id: Option<String>,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) source_block_id: Option<String>,
    pub(crate) dedup_fingerprint: Option<String>,
    pub(crate) execution_mode: OperationExecutionMode,
    pub(crate) on_parent_complete: OnParentComplete,
    pub(crate) depth: u32,
    pub(crate) status: OperationStatus,
    pub(crate) input_json: String,
    pub(crate) outcome_json: Option<String>,
    pub(crate) result_text: Option<String>,
    pub(crate) result_bytes: Option<usize>,
    pub(crate) created_at_ms: u64,
    pub(crate) finished_at_ms: Option<u64>,
}

/// Request to atomically admit a new operation (and optionally spawn a new agent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmitOperationRequest {
    pub(crate) commit_id: String,
    pub(crate) operation_id: String,
    pub(crate) target_agent_id: String,
    pub(crate) parent_operation_id: Option<String>,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) source_block_id: Option<String>,
    pub(crate) dedup_fingerprint: Option<String>,
    pub(crate) execution_mode: OperationExecutionMode,
    pub(crate) on_parent_complete: OnParentComplete,
    pub(crate) depth: u32,
    pub(crate) input_json: String,
    pub(crate) created_at_ms: u64,
    pub(crate) new_agent: Option<NewAgentRow>,
}

/// Result of [`Writer::admit_operation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmitOperationOutcome {
    /// Operation (and optional new agent) was inserted and session revision bumped.
    Admitted {
        operation: StoredOperationRow,
        session_revision: u64,
    },
    /// Matching `(session_id, parent_operation_id, source_block_id)` with equal fingerprint replayed.
    Replayed(StoredOperationRow),
    /// Matching `(session_id, parent_operation_id, source_block_id)` had a conflicting fingerprint.
    DedupConflict { existing: StoredOperationRow },
}

/// Optimistic-concurrency update to one agent's persisted material columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentMaterialUpdate {
    pub(crate) agent_id: String,
    pub(crate) expected_material_revision: u64,
    pub(crate) context_json: String,
    pub(crate) history_index_json: String,
    pub(crate) usage_json: Option<String>,
}

/// Terminal state and archived result update for an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OperationTerminalUpdate {
    pub(crate) operation_id: String,
    pub(crate) status: OperationStatus,
    pub(crate) outcome_json: Option<String>,
    pub(crate) result_text: Option<String>,
    pub(crate) finished_at_ms: u64,
}

/// Single `BEGIN IMMEDIATE` transactional outbox commit request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxCommitRequest {
    pub(crate) commit_id: String,
    pub(crate) expected_session_revision: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) agent_update: Option<AgentMaterialUpdate>,
    pub(crate) operation_update: Option<OperationTerminalUpdate>,
    pub(crate) new_events: Vec<NewInboxEvent>,
    pub(crate) delivered_event_ids: Vec<i64>,
    pub(crate) dropped_event_ids: Vec<i64>,
}

/// Result of [`Writer::commit_outbox`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxCommitResult {
    pub(crate) session_revision: u64,
    pub(crate) agent_material_revision: Option<u64>,
    pub(crate) operation_updated: bool,
    pub(crate) inserted_event_ids: Vec<i64>,
}

/// Read-only `read_agent` snapshot pairing operation metadata with a UTF-8 output page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadAgentPage {
    pub(crate) operation: StoredOperationRow,
    pub(crate) page: Utf8PageSlice,
}

fn to_i64(value: u64, field: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::InvalidDatabase(format!("{field} overflows i64")))
}

fn usize_to_i64(value: usize, field: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::InvalidDatabase(format!("{field} overflows i64")))
}

fn from_i64_u64(value: i64, field: &str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::InvalidDatabase(format!("negative {field}")))
}

fn from_i64_u32(value: i64, field: &str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::InvalidDatabase(format!("invalid {field}")))
}

fn from_i64_usize(value: i64, field: &str) -> Result<usize, StoreError> {
    usize::try_from(value).map_err(|_| StoreError::InvalidDatabase(format!("negative {field}")))
}

fn validate_agent_materials(
    definition_json: Option<&str>,
    context_json: &str,
    history_index_json: &str,
    usage_json: Option<&str>,
) -> Result<(), StoreError> {
    if let Some(def_json) = definition_json {
        let definition: SessionDefinition = serde_json::from_str(def_json)?;
        definition
            .validate()
            .map_err(|err| StoreError::Definition(Arc::new(err)))?;
    }
    let context: Context = crate::material::decode_stored(context_json.as_bytes())?;
    let history: Vec<crate::compact::HistoryRecordEntry> =
        serde_json::from_str(history_index_json)?;
    crate::compact::history::validate_index(&history)?;
    crate::material::validate_references(&context, Some(&history), None)
        .map_err(|err| StoreError::References(Arc::new(err)))?;
    if let Some(usage_raw) = usage_json {
        let usage: WireSessionUsage = serde_json::from_str(usage_raw)?;
        usage.validate()?;
    }
    Ok(())
}

fn decode_agent_row(row: &Row<'_>) -> Result<StoredAgentRow, StoreError> {
    let interaction_raw: String = row.get(4)?;
    let interaction = interaction_raw
        .parse::<AgentInteraction>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let material_revision = from_i64_u64(row.get(6)?, "material_revision")?;
    let created_at_ms = from_i64_u64(row.get(10)?, "created_at_ms")?;
    let updated_at_ms = from_i64_u64(row.get(11)?, "updated_at_ms")?;

    Ok(StoredAgentRow {
        session_id: row.get(0)?,
        agent_id: row.get(1)?,
        name: row.get(2)?,
        preset_name: row.get(3)?,
        interaction,
        definition_json: row.get(5)?,
        material_revision,
        context_json: row.get(7)?,
        history_index_json: row.get(8)?,
        usage_json: row.get(9)?,
        created_at_ms,
        updated_at_ms,
    })
}

const SELECT_AGENT_COLS: &str = "
    session_id, agent_id, name, preset_name, interaction,
    definition_json, material_revision, context_json, history_index_json,
    usage_json, created_at_ms, updated_at_ms
";

const SELECT_OPERATION_COLS: &str = "
    operation_id, session_id, target_agent_id, parent_operation_id, parent_agent_id,
    source_block_id, dedup_fingerprint, execution_mode, on_parent_complete,
    depth, status, input_json, outcome_json, result_text, result_bytes,
    created_at_ms, finished_at_ms
";

fn decode_operation_row(row: &Row<'_>) -> Result<StoredOperationRow, StoreError> {
    let execution_mode_raw: String = row.get(7)?;
    let execution_mode = execution_mode_raw
        .parse::<OperationExecutionMode>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let on_parent_complete_raw: String = row.get(8)?;
    let on_parent_complete = on_parent_complete_raw
        .parse::<OnParentComplete>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let depth = from_i64_u32(row.get(9)?, "depth")?;
    let status_raw: String = row.get(10)?;
    let status = status_raw
        .parse::<OperationStatus>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let result_bytes = row
        .get::<_, Option<i64>>(14)?
        .map(|val| from_i64_usize(val, "result_bytes"))
        .transpose()?;
    let created_at_ms = from_i64_u64(row.get(15)?, "created_at_ms")?;
    let finished_at_ms = row
        .get::<_, Option<i64>>(16)?
        .map(|val| from_i64_u64(val, "finished_at_ms"))
        .transpose()?;

    Ok(StoredOperationRow {
        operation_id: row.get(0)?,
        session_id: row.get(1)?,
        target_agent_id: row.get(2)?,
        parent_operation_id: row.get(3)?,
        parent_agent_id: row.get(4)?,
        source_block_id: row.get(5)?,
        dedup_fingerprint: row.get(6)?,
        execution_mode,
        on_parent_complete,
        depth,
        status,
        input_json: row.get(11)?,
        outcome_json: row.get(12)?,
        result_text: row.get(13)?,
        result_bytes,
        created_at_ms,
        finished_at_ms,
    })
}

fn decode_inbox_event_row(row: &Row<'_>) -> Result<InboxEvent, StoreError> {
    let priority_raw: String = row.get(3)?;
    let priority = priority_raw
        .parse::<EventPriority>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let source_kind_raw: String = row.get(4)?;
    let source_kind = source_kind_raw
        .parse::<EventSourceKind>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let wake_policy_raw: String = row.get(7)?;
    let wake_policy = wake_policy_raw
        .parse::<WakePolicy>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let state_raw: String = row.get(9)?;
    let state = state_raw
        .parse::<EventDeliveryState>()
        .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
    let created_at_ms = from_i64_u64(row.get(11)?, "created_at_ms")?;
    let delivered_at_ms = row
        .get::<_, Option<i64>>(12)?
        .map(|val| from_i64_u64(val, "delivered_at_ms"))
        .transpose()?;

    Ok(InboxEvent {
        event_id: row.get(0)?,
        session_id: row.get(1)?,
        target_agent_id: row.get(2)?,
        priority,
        source_kind,
        source_id: row.get(5)?,
        coalesce_key: row.get(6)?,
        wake_policy,
        payload_json: row.get(8)?,
        state,
        claimed_op_id: row.get(10)?,
        created_at_ms,
        delivered_at_ms,
    })
}

fn read_agent_with_conn(
    conn: &Connection,
    session_id: &str,
    agent_id: &str,
) -> Result<Option<StoredAgentRow>, StoreError> {
    let sql =
        format!("SELECT {SELECT_AGENT_COLS} FROM agents WHERE session_id = ?1 AND agent_id = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![session_id, agent_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(decode_agent_row(row)?)),
        None => Ok(None),
    }
}

fn read_operation_with_conn(
    conn: &Connection,
    session_id: &str,
    operation_id: &str,
) -> Result<Option<StoredOperationRow>, StoreError> {
    let sql = format!(
        "SELECT {SELECT_OPERATION_COLS} FROM operations WHERE session_id = ?1 AND operation_id = ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![session_id, operation_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(decode_operation_row(row)?)),
        None => Ok(None),
    }
}

fn find_dedup_operation(
    conn: &Connection,
    session_id: &str,
    parent_operation_id: Option<&str>,
    source_block_id: &str,
) -> Result<Option<StoredOperationRow>, StoreError> {
    let sql = format!(
        "SELECT {SELECT_OPERATION_COLS}
         FROM operations
         WHERE session_id = ?1
           AND parent_operation_id IS ?2
           AND source_block_id = ?3
         LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![session_id, parent_operation_id, source_block_id])?;
    match rows.next()? {
        Some(row) => Ok(Some(decode_operation_row(row)?)),
        None => Ok(None),
    }
}

fn read_session_header(conn: &Connection, session_id: &str) -> Result<(u64, String), StoreError> {
    let (rev_i64, last_commit_id): (i64, String) = conn
        .query_row(
            "SELECT revision, last_commit_id FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError::InvalidDatabase(format!("missing session {session_id}")))?;
    Ok((from_i64_u64(rev_i64, "revision")?, last_commit_id))
}

fn verify_admit_matches_db(
    conn: &Connection,
    session_id: &str,
    req: &AdmitOperationRequest,
) -> Result<StoredOperationRow, StoreError> {
    let Some(existing_op) = read_operation_with_conn(conn, session_id, &req.operation_id)? else {
        return Err(StoreError::CommitIdentityConflict);
    };
    let op_matches = existing_op.target_agent_id == req.target_agent_id
        && existing_op.parent_operation_id == req.parent_operation_id
        && existing_op.parent_agent_id == req.parent_agent_id
        && existing_op.source_block_id == req.source_block_id
        && existing_op.dedup_fingerprint == req.dedup_fingerprint
        && existing_op.execution_mode == req.execution_mode
        && existing_op.on_parent_complete == req.on_parent_complete
        && existing_op.depth == req.depth
        && existing_op.input_json == req.input_json
        && existing_op.created_at_ms == req.created_at_ms;
    if !op_matches {
        return Err(StoreError::CommitIdentityConflict);
    }

    if let Some(agent) = &req.new_agent {
        let Some(existing_agent) = read_agent_with_conn(conn, session_id, &agent.agent_id)? else {
            return Err(StoreError::CommitIdentityConflict);
        };
        let agent_matches = existing_agent.name == agent.name
            && existing_agent.preset_name == agent.preset_name
            && existing_agent.interaction == agent.interaction
            && existing_agent.definition_json == agent.definition_json
            && existing_agent.context_json == agent.context_json
            && existing_agent.history_index_json == agent.history_index_json
            && existing_agent.usage_json == agent.usage_json
            && existing_agent.created_at_ms == agent.created_at_ms;
        if !agent_matches {
            return Err(StoreError::CommitIdentityConflict);
        }
    }

    Ok(existing_op)
}

fn verify_outbox_matches_db(
    conn: &Connection,
    session_id: &str,
    session_revision: u64,
    req: &OutboxCommitRequest,
) -> Result<OutboxCommitResult, StoreError> {
    let expected_next_rev = req
        .expected_session_revision
        .checked_add(1)
        .ok_or(StoreError::InvalidRevision)?;
    if session_revision != expected_next_rev {
        return Err(StoreError::CommitIdentityConflict);
    }

    let agent_material_revision = if let Some(upd) = &req.agent_update {
        let Some(agent_row) = read_agent_with_conn(conn, session_id, &upd.agent_id)? else {
            return Err(StoreError::CommitIdentityConflict);
        };
        let expected_mat = upd
            .expected_material_revision
            .checked_add(1)
            .ok_or(StoreError::InvalidRevision)?;
        if agent_row.material_revision != expected_mat
            || agent_row.context_json != upd.context_json
            || agent_row.history_index_json != upd.history_index_json
            || agent_row.usage_json != upd.usage_json
        {
            return Err(StoreError::CommitIdentityConflict);
        }
        Some(expected_mat)
    } else {
        None
    };

    let operation_updated = if let Some(upd) = &req.operation_update {
        let Some(op_row) = read_operation_with_conn(conn, session_id, &upd.operation_id)? else {
            return Err(StoreError::CommitIdentityConflict);
        };
        if op_row.status.is_active() {
            return Err(StoreError::CommitIdentityConflict);
        }
        if op_row.status == upd.status
            && op_row.outcome_json == upd.outcome_json
            && op_row.result_text == upd.result_text
            && op_row.finished_at_ms == Some(upd.finished_at_ms)
        {
            true
        } else if op_row.finished_at_ms.is_some_and(|ts| {
            ts < upd.finished_at_ms || (ts == upd.finished_at_ms && op_row.status != upd.status)
        }) {
            false
        } else {
            return Err(StoreError::CommitIdentityConflict);
        }
    } else {
        false
    };

    for &event_id in &req.delivered_event_ids {
        let row_info: Option<(String, String)> = conn
            .query_row(
                "SELECT target_agent_id, state FROM agent_events WHERE session_id = ?1 AND event_id = ?2",
                params![session_id, event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((target, state_str)) = row_info else {
            return Err(StoreError::CommitIdentityConflict);
        };
        if let Some(upd) = &req.agent_update
            && target != upd.agent_id
        {
            return Err(StoreError::CommitIdentityConflict);
        }
        if state_str != EventDeliveryState::Delivered.as_str() {
            return Err(StoreError::CommitIdentityConflict);
        }
    }

    for &event_id in &req.dropped_event_ids {
        let row_info: Option<(String, String)> = conn
            .query_row(
                "SELECT target_agent_id, state FROM agent_events WHERE session_id = ?1 AND event_id = ?2",
                params![session_id, event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((target, state_str)) = row_info else {
            return Err(StoreError::CommitIdentityConflict);
        };
        if let Some(upd) = &req.agent_update
            && target != upd.agent_id
        {
            return Err(StoreError::CommitIdentityConflict);
        }
        if state_str != EventDeliveryState::Dropped.as_str() {
            return Err(StoreError::CommitIdentityConflict);
        }
    }

    let mut inserted_event_ids = Vec::with_capacity(req.new_events.len());
    if !req.new_events.is_empty() {
        let limit_i64 = usize_to_i64(req.new_events.len(), "new_events.len")?;
        let mut stmt = conn.prepare(
            "SELECT
                event_id, session_id, target_agent_id, priority, source_kind,
                source_id, coalesce_key, wake_policy, payload_json, state,
                claimed_op_id, created_at_ms, delivered_at_ms
             FROM agent_events
             WHERE session_id = ?1
             ORDER BY event_id DESC
             LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![session_id, limit_i64])?;
        let mut fetched = Vec::with_capacity(req.new_events.len());
        while let Some(row) = rows.next()? {
            fetched.push(decode_inbox_event_row(row)?);
        }
        if fetched.len() != req.new_events.len() {
            return Err(StoreError::CommitIdentityConflict);
        }
        fetched.reverse();
        for (stored, expected) in fetched.iter().zip(req.new_events.iter()) {
            if stored.target_agent_id != expected.target_agent_id
                || stored.priority != expected.priority
                || stored.source_kind != expected.source_kind
                || stored.source_id != expected.source_id
                || stored.coalesce_key != expected.coalesce_key
                || stored.wake_policy != expected.wake_policy
                || stored.payload_json != expected.payload_json
                || stored.created_at_ms != expected.created_at_ms
            {
                return Err(StoreError::CommitIdentityConflict);
            }
            inserted_event_ids.push(stored.event_id);
        }
    }

    Ok(OutboxCommitResult {
        session_revision,
        agent_material_revision,
        operation_updated,
        inserted_event_ids,
    })
}

fn validate_event_ownership(
    conn: &Connection,
    session_id: &str,
    event_id: i64,
    expected_agent_id: Option<&str>,
) -> Result<(), StoreError> {
    let target: Option<String> = conn
        .query_row(
            "SELECT target_agent_id FROM agent_events WHERE session_id = ?1 AND event_id = ?2",
            params![session_id, event_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(target_agent_id) = target else {
        return Err(StoreError::InvalidDatabase(format!(
            "event {event_id} not found in session {session_id}"
        )));
    };
    if let Some(expected) = expected_agent_id
        && target_agent_id != expected
    {
        return Err(StoreError::InvalidDatabase(format!(
            "event {event_id} belongs to agent {target_agent_id}, not {expected}"
        )));
    }
    Ok(())
}

impl Writer {
    /// Atomically admit a new operation (and optionally insert a new child agent)
    /// inside a single `BEGIN IMMEDIATE` transaction, checking source-block deduplication first.
    pub(crate) fn admit_operation(
        &self,
        req: &AdmitOperationRequest,
    ) -> Result<AdmitOperationOutcome, StoreError> {
        canonical_uuid(&req.commit_id)?;
        canonical_uuid(&req.operation_id)?;
        canonical_uuid(&req.target_agent_id)?;
        if let Some(parent_op_id) = &req.parent_operation_id {
            canonical_uuid(parent_op_id)?;
        }
        if let Some(parent_agent_id) = &req.parent_agent_id {
            canonical_uuid(parent_agent_id)?;
        }
        if req.input_json.len() > MAX_OPERATION_INPUT_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_OPERATION_INPUT_BYTES,
            });
        }

        let new_agent_bytes = if let Some(agent) = &req.new_agent {
            canonical_uuid(&agent.agent_id)?;
            if agent.agent_id != req.target_agent_id {
                return Err(StoreError::IdentityMismatch {
                    expected: req.target_agent_id.clone(),
                    actual: agent.agent_id.clone(),
                });
            }
            let total = agent.definition_json.len()
                + agent.context_json.len()
                + agent.history_index_json.len()
                + agent.usage_json.as_deref().map_or(0, str::len);
            if total > MAX_RECORD_BYTES {
                return Err(StoreError::RecordTooLarge {
                    limit: MAX_RECORD_BYTES,
                });
            }
            validate_agent_materials(
                Some(&agent.definition_json),
                &agent.context_json,
                &agent.history_index_json,
                agent.usage_json.as_deref(),
            )?;
            total
        } else {
            0
        };

        let created_at_ms_i64 = to_i64(req.created_at_ms, "created_at_ms")?;
        let depth_i64 = i64::from(req.depth);

        let (mut state, conn) = self.lock_state_and_conn()?;
        if state.pending.is_some() || state.pending_outbox.is_some() {
            return Err(StoreError::PendingCommit);
        }

        let (current_rev, current_last_commit_id) = read_session_header(&conn, self.session_id())?;

        if current_last_commit_id == req.commit_id {
            if let Some((pending_req, _)) = &state.pending_admit
                && pending_req.commit_id == req.commit_id
                && pending_req != req
            {
                return Err(StoreError::CommitIdentityConflict);
            }
            let outcome = match &state.pending_admit {
                Some((pending_req, Some(cached)))
                    if pending_req.commit_id == req.commit_id && pending_req == req =>
                {
                    cached.clone()
                }
                _ => {
                    let existing_op = verify_admit_matches_db(&conn, self.session_id(), req)?;
                    AdmitOperationOutcome::Admitted {
                        operation: existing_op,
                        session_revision: current_rev,
                    }
                }
            };
            state.baseline = None;
            self.hit(CommitStage::Confirm)?;
            self.hit(CommitStage::Reply)?;
            state.pending_admit = None;
            return Ok(outcome);
        }

        if let Some((pending_req, _)) = &state.pending_admit {
            if pending_req.commit_id == req.commit_id {
                if pending_req != req {
                    return Err(StoreError::CommitIdentityConflict);
                }
            } else {
                return Err(StoreError::PendingCommit);
            }
        }

        let tx = ImmediateTx::begin(&conn)?;

        if let Some(source_block_id) = &req.source_block_id
            && let Some(existing) = find_dedup_operation(
                &conn,
                self.session_id(),
                req.parent_operation_id.as_deref(),
                source_block_id,
            )?
        {
            tx.rollback()?;
            if existing.dedup_fingerprint == req.dedup_fingerprint {
                return Ok(AdmitOperationOutcome::Replayed(existing));
            }
            return Ok(AdmitOperationOutcome::DedupConflict { existing });
        }

        if let Some(parent_op_id) = &req.parent_operation_id {
            let parent_op_target: Option<String> = conn
                .query_row(
                    "SELECT target_agent_id FROM operations WHERE session_id = ?1 AND operation_id = ?2",
                    params![self.session_id(), parent_op_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(parent_op_target) = parent_op_target else {
                return Err(StoreError::InvalidDatabase(format!(
                    "parent operation {parent_op_id} not found in session {}",
                    self.session_id()
                )));
            };
            if let Some(parent_agent_id) = &req.parent_agent_id
                && parent_op_target != *parent_agent_id
            {
                return Err(StoreError::InvalidDatabase(format!(
                    "parent_agent_id {parent_agent_id} does not match parent operation target {parent_op_target}"
                )));
            }
        }

        if let Some(parent_agent_id) = &req.parent_agent_id {
            let parent_exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM agents WHERE session_id = ?1 AND agent_id = ?2",
                    params![self.session_id(), parent_agent_id],
                    |row| row.get(0),
                )
                .optional()?;
            if parent_exists.is_none() {
                return Err(StoreError::InvalidDatabase(format!(
                    "parent agent {parent_agent_id} not found in session {}",
                    self.session_id()
                )));
            }
        }

        let next_rev = current_rev
            .checked_add(1)
            .ok_or(StoreError::InvalidRevision)?;
        let next_rev_i64 = to_i64(next_rev, "revision")?;

        if state.pending_admit.is_none() {
            state.pending_admit = Some((req.clone(), None));
        }

        self.hit(CommitStage::Serialize)?;
        self.hit(CommitStage::Write)?;

        let write_result: Result<StoredOperationRow, StoreError> = (|| {
            if let Some(agent) = &req.new_agent {
                let agent_created_i64 = to_i64(agent.created_at_ms, "agent.created_at_ms")?;
                let agent_updated_i64 = to_i64(agent.updated_at_ms, "agent.updated_at_ms")?;
                conn.execute(
                    "INSERT INTO agents (
                        session_id, agent_id, name, preset_name, interaction,
                        definition_json, material_revision, context_json, history_index_json,
                        usage_json, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        self.session_id(),
                        agent.agent_id,
                        agent.name,
                        agent.preset_name,
                        agent.interaction.as_str(),
                        agent.definition_json,
                        agent.context_json,
                        agent.history_index_json,
                        agent.usage_json,
                        agent_created_i64,
                        agent_updated_i64,
                    ],
                )?;
            }

            conn.execute(
                "INSERT INTO operations (
                    operation_id, session_id, target_agent_id, parent_operation_id, parent_agent_id,
                    source_block_id, dedup_fingerprint, execution_mode, on_parent_complete,
                    depth, status, input_json, outcome_json, result_text, result_bytes,
                    created_at_ms, finished_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'creating', ?11, NULL, NULL, NULL, ?12, NULL)",
                params![
                    req.operation_id,
                    self.session_id(),
                    req.target_agent_id,
                    req.parent_operation_id,
                    req.parent_agent_id,
                    req.source_block_id,
                    req.dedup_fingerprint,
                    req.execution_mode.as_str(),
                    req.on_parent_complete.as_str(),
                    depth_i64,
                    req.input_json,
                    created_at_ms_i64,
                ],
            )?;

            conn.execute(
                "UPDATE sessions
                 SET revision = ?2,
                     last_commit_id = ?3,
                     updated_at_ms = MAX(updated_at_ms, ?4)
                 WHERE session_id = ?1",
                params![
                    self.session_id(),
                    next_rev_i64,
                    req.commit_id,
                    created_at_ms_i64
                ],
            )?;

            read_operation_with_conn(&conn, self.session_id(), &req.operation_id)?.ok_or_else(
                || StoreError::InvalidDatabase("inserted operation missing before commit".into()),
            )
        })();

        let operation = match write_result {
            Ok(op) => op,
            Err(err) => {
                state.pending_admit = None;
                return Err(err);
            }
        };

        self.hit(CommitStage::Sync)?;
        self.hit(CommitStage::Replace)?;
        tx.commit()?;

        let outcome = AdmitOperationOutcome::Admitted {
            operation,
            session_revision: next_rev,
        };
        state.baseline = None;
        state.pending_admit = Some((req.clone(), Some(outcome.clone())));
        self.record_write_bytes(req.input_json.len() + new_agent_bytes);
        self.hit(CommitStage::Confirm)?;
        self.hit(CommitStage::Reply)?;
        state.pending_admit = None;

        Ok(outcome)
    }

    /// Transition an admitted operation from `creating` to `running`.
    ///
    /// Returns `Ok(true)` if transitioned, or `Ok(false)` if the operation does not
    /// exist in `creating` state (for example, already cancelled before startup).
    pub(crate) fn transition_operation_running(
        &self,
        operation_id: &str,
        updated_at_ms: u64,
    ) -> Result<bool, StoreError> {
        canonical_uuid(operation_id)?;
        let _ = updated_at_ms;
        let (_state, conn) = self.lock_state_and_conn()?;
        let tx = ImmediateTx::begin(&conn)?;
        let changed = conn.execute(
            "UPDATE operations
             SET status = 'running'
             WHERE session_id = ?1 AND operation_id = ?2 AND status = 'creating'",
            params![self.session_id(), operation_id],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Atomically commit agent material updates, terminal operation state/result,
    /// new pending `agent_events`, delivered/dropped event transitions, and
    /// `sessions.revision` increment inside a single `BEGIN IMMEDIATE` transaction.
    pub(crate) fn commit_outbox(
        &self,
        req: &OutboxCommitRequest,
    ) -> Result<OutboxCommitResult, StoreError> {
        canonical_uuid(&req.commit_id)?;
        let updated_at_ms_i64 = to_i64(req.updated_at_ms, "updated_at_ms")?;

        if !req.delivered_event_ids.is_empty() && req.agent_update.is_none() {
            return Err(StoreError::InvalidDatabase(
                "delivered_event_ids requires an agent_update target".into(),
            ));
        }

        let mut total_bytes = 0usize;
        if let Some(agent_upd) = &req.agent_update {
            canonical_uuid(&agent_upd.agent_id)?;
            let agent_bytes = agent_upd.context_json.len()
                + agent_upd.history_index_json.len()
                + agent_upd.usage_json.as_deref().map_or(0, str::len);
            if agent_bytes > MAX_RECORD_BYTES {
                return Err(StoreError::RecordTooLarge {
                    limit: MAX_RECORD_BYTES,
                });
            }
            validate_agent_materials(
                None,
                &agent_upd.context_json,
                &agent_upd.history_index_json,
                agent_upd.usage_json.as_deref(),
            )?;
            total_bytes += agent_bytes;
        }
        if let Some(op_upd) = &req.operation_update {
            canonical_uuid(&op_upd.operation_id)?;
            if !op_upd.status.is_terminal() {
                return Err(StoreError::InvalidDatabase(format!(
                    "operation_update requires a terminal status, found {}",
                    op_upd.status
                )));
            }
            let op_bytes = op_upd.outcome_json.as_deref().map_or(0, str::len)
                + op_upd.result_text.as_deref().map_or(0, str::len);
            if op_bytes > MAX_RECORD_BYTES {
                return Err(StoreError::RecordTooLarge {
                    limit: MAX_RECORD_BYTES,
                });
            }
            total_bytes += op_bytes;
        }
        for event in &req.new_events {
            canonical_uuid(&event.target_agent_id)?;
            if event.payload_json.len() > MAX_RECORD_BYTES {
                return Err(StoreError::RecordTooLarge {
                    limit: MAX_RECORD_BYTES,
                });
            }
            total_bytes += event.payload_json.len();
        }
        if total_bytes > MAX_RECORD_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_RECORD_BYTES,
            });
        }

        let (mut state, conn) = self.lock_state_and_conn()?;
        if state.pending.is_some() || state.pending_admit.is_some() {
            return Err(StoreError::PendingCommit);
        }

        let (current_rev, current_last_commit_id) = read_session_header(&conn, self.session_id())?;

        if current_last_commit_id == req.commit_id {
            if let Some((pending_req, _)) = &state.pending_outbox
                && pending_req.commit_id == req.commit_id
                && pending_req != req
            {
                return Err(StoreError::CommitIdentityConflict);
            }
            let result = match &state.pending_outbox {
                Some((pending_req, Some(cached)))
                    if pending_req.commit_id == req.commit_id && pending_req == req =>
                {
                    cached.clone()
                }
                _ => verify_outbox_matches_db(&conn, self.session_id(), current_rev, req)?,
            };
            state.baseline = None;
            self.hit(CommitStage::Confirm)?;
            self.hit(CommitStage::Reply)?;
            state.pending_outbox = None;
            return Ok(result);
        }

        if let Some((pending_req, _)) = &state.pending_outbox {
            if pending_req.commit_id == req.commit_id {
                if pending_req != req {
                    return Err(StoreError::CommitIdentityConflict);
                }
            } else {
                return Err(StoreError::PendingCommit);
            }
        }

        if current_rev != req.expected_session_revision {
            return Err(StoreError::Conflict {
                expected: Some(req.expected_session_revision),
                actual: Some(current_rev),
            });
        }

        let tx = ImmediateTx::begin(&conn)?;

        if let Some(upd) = &req.agent_update {
            let actual: Option<i64> = conn
                .query_row(
                    "SELECT material_revision FROM agents WHERE session_id = ?1 AND agent_id = ?2",
                    params![self.session_id(), upd.agent_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(actual_rev_i64) = actual else {
                return Err(StoreError::InvalidDatabase(format!(
                    "missing agent {}",
                    upd.agent_id
                )));
            };
            let actual_rev = from_i64_u64(actual_rev_i64, "material_revision")?;
            if actual_rev != upd.expected_material_revision {
                return Err(StoreError::Conflict {
                    expected: Some(upd.expected_material_revision),
                    actual: Some(actual_rev),
                });
            }
        }

        let operation_is_active = if let Some(upd) = &req.operation_update {
            let current_status_raw: String = conn
                .query_row(
                    "SELECT status FROM operations WHERE session_id = ?1 AND operation_id = ?2",
                    params![self.session_id(), upd.operation_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::InvalidDatabase(format!("missing operation {}", upd.operation_id))
                })?;
            let current_status = current_status_raw
                .parse::<OperationStatus>()
                .map_err(|err| StoreError::InvalidDatabase(err.to_string()))?;
            Some(current_status.is_active())
        } else {
            None
        };

        let expected_agent_id = req.agent_update.as_ref().map(|upd| upd.agent_id.as_str());
        for &event_id in &req.delivered_event_ids {
            validate_event_ownership(&conn, self.session_id(), event_id, expected_agent_id)?;
        }
        for &event_id in &req.dropped_event_ids {
            validate_event_ownership(&conn, self.session_id(), event_id, expected_agent_id)?;
        }

        if state.pending_outbox.is_none() {
            state.pending_outbox = Some((req.clone(), None));
        }

        self.hit(CommitStage::Serialize)?;
        self.hit(CommitStage::Write)?;

        let next_session_revision = current_rev
            .checked_add(1)
            .ok_or(StoreError::InvalidRevision)?;
        let next_session_rev_i64 = to_i64(next_session_revision, "revision")?;

        let write_result: Result<OutboxCommitResult, StoreError> = (|| {
            let agent_material_revision = if let Some(upd) = &req.agent_update {
                let expected_mat_i64 = to_i64(upd.expected_material_revision, "material_revision")?;
                let next_mat = upd
                    .expected_material_revision
                    .checked_add(1)
                    .ok_or(StoreError::InvalidRevision)?;
                let next_mat_i64 = to_i64(next_mat, "material_revision")?;
                let changed = conn.execute(
                    "UPDATE agents
                     SET material_revision = ?4,
                         context_json = ?5,
                         history_index_json = ?6,
                         usage_json = ?7,
                         updated_at_ms = MAX(updated_at_ms, ?8)
                     WHERE session_id = ?1
                       AND agent_id = ?2
                       AND material_revision = ?3",
                    params![
                        self.session_id(),
                        upd.agent_id,
                        expected_mat_i64,
                        next_mat_i64,
                        upd.context_json,
                        upd.history_index_json,
                        upd.usage_json,
                        updated_at_ms_i64,
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::Conflict {
                        expected: Some(upd.expected_material_revision),
                        actual: None,
                    });
                }
                Some(next_mat)
            } else {
                None
            };

            let operation_updated = match (&req.operation_update, operation_is_active) {
                (Some(upd), Some(true)) => {
                    let result_bytes_i64 = upd
                        .result_text
                        .as_ref()
                        .map(|text| usize_to_i64(text.len(), "result_bytes"))
                        .transpose()?;
                    let finished_at_i64 = to_i64(upd.finished_at_ms, "finished_at_ms")?;
                    conn.execute(
                        "UPDATE operations
                         SET status = ?3,
                             outcome_json = ?4,
                             result_text = ?5,
                             result_bytes = ?6,
                             finished_at_ms = ?7
                         WHERE session_id = ?1 AND operation_id = ?2",
                        params![
                            self.session_id(),
                            upd.operation_id,
                            upd.status.as_str(),
                            upd.outcome_json,
                            upd.result_text,
                            result_bytes_i64,
                            finished_at_i64,
                        ],
                    )?;
                    true
                }
                _ => false,
            };

            let mut inserted_event_ids = Vec::with_capacity(req.new_events.len());
            for event in &req.new_events {
                let event_created_i64 = to_i64(event.created_at_ms, "event.created_at_ms")?;
                conn.execute(
                    "INSERT INTO agent_events (
                        session_id, target_agent_id, priority, source_kind, source_id,
                        coalesce_key, wake_policy, payload_json, state, claimed_op_id,
                        created_at_ms, delivered_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL, ?9, NULL)",
                    params![
                        self.session_id(),
                        event.target_agent_id,
                        event.priority.as_str(),
                        event.source_kind.as_str(),
                        event.source_id,
                        event.coalesce_key,
                        event.wake_policy.as_str(),
                        event.payload_json,
                        event_created_i64,
                    ],
                )?;
                inserted_event_ids.push(conn.last_insert_rowid());
            }

            for &event_id in &req.delivered_event_ids {
                conn.execute(
                    "UPDATE agent_events
                     SET state = 'delivered',
                         delivered_at_ms = ?3
                     WHERE session_id = ?1
                       AND event_id = ?2
                       AND state IN ('pending', 'claimed')",
                    params![self.session_id(), event_id, updated_at_ms_i64],
                )?;
            }

            for &event_id in &req.dropped_event_ids {
                conn.execute(
                    "UPDATE agent_events
                     SET state = 'dropped'
                     WHERE session_id = ?1
                       AND event_id = ?2
                       AND state IN ('pending', 'claimed')",
                    params![self.session_id(), event_id],
                )?;
            }

            conn.execute(
                "UPDATE sessions
                 SET revision = ?2,
                     last_commit_id = ?3,
                     updated_at_ms = MAX(updated_at_ms, ?4)
                 WHERE session_id = ?1",
                params![
                    self.session_id(),
                    next_session_rev_i64,
                    req.commit_id,
                    updated_at_ms_i64,
                ],
            )?;

            Ok(OutboxCommitResult {
                session_revision: next_session_revision,
                agent_material_revision,
                operation_updated,
                inserted_event_ids,
            })
        })();

        let result = match write_result {
            Ok(res) => res,
            Err(err) => {
                state.pending_outbox = None;
                return Err(err);
            }
        };

        self.hit(CommitStage::Sync)?;
        self.hit(CommitStage::Replace)?;
        tx.commit()?;

        state.baseline = None;
        state.pending_outbox = Some((req.clone(), Some(result.clone())));
        self.record_write_bytes(total_bytes);
        self.hit(CommitStage::Confirm)?;
        self.hit(CommitStage::Reply)?;
        state.pending_outbox = None;

        Ok(result)
    }

    /// Reset any crashed `claimed` events back to `pending` and downgrade `wake_if_idle`
    /// pending events to `deliver_on_next_turn` so reopening a session never auto-wakes.
    pub(crate) fn downgrade_pending_wakeups_on_reopen(&self) -> Result<usize, StoreError> {
        let (_state, conn) = self.lock_state_and_conn()?;
        let tx = ImmediateTx::begin(&conn)?;
        conn.execute(
            "UPDATE agent_events
             SET state = 'pending',
                 claimed_op_id = NULL
             WHERE session_id = ?1 AND state = 'claimed'",
            params![self.session_id()],
        )?;
        let downgraded = conn.execute(
            "UPDATE agent_events
             SET wake_policy = 'deliver_on_next_turn'
             WHERE session_id = ?1
               AND state = 'pending'
               AND wake_policy = 'wake_if_idle'",
            params![self.session_id()],
        )?;
        tx.commit()?;
        Ok(downgraded)
    }

    /// Downgrade pending/claimed events emitted by `(source_kind, source_id)` to `suppress_wake`.
    pub(crate) fn suppress_pending_wakeups_for_source(
        &self,
        source_kind: EventSourceKind,
        source_id: &str,
    ) -> Result<usize, StoreError> {
        let (_state, conn) = self.lock_state_and_conn()?;
        let tx = ImmediateTx::begin(&conn)?;
        let changed = conn.execute(
            "UPDATE agent_events
             SET wake_policy = 'suppress_wake'
             WHERE session_id = ?1
               AND source_kind = ?2
               AND source_id = ?3
               AND state IN ('pending', 'claimed')
               AND wake_policy != 'suppress_wake'",
            params![self.session_id(), source_kind.as_str(), source_id],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// Downgrade pending/claimed events targeting `target_agent_id` to `suppress_wake`.
    pub(crate) fn suppress_pending_wakeups_for_target(
        &self,
        target_agent_id: &str,
    ) -> Result<usize, StoreError> {
        canonical_uuid(target_agent_id)?;
        let (_state, conn) = self.lock_state_and_conn()?;
        let tx = ImmediateTx::begin(&conn)?;
        let changed = conn.execute(
            "UPDATE agent_events
             SET wake_policy = 'suppress_wake'
             WHERE session_id = ?1
               AND target_agent_id = ?2
               AND state IN ('pending', 'claimed')
               AND wake_policy != 'suppress_wake'",
            params![self.session_id(), target_agent_id],
        )?;
        tx.commit()?;
        Ok(changed)
    }
}

impl SessionStore {
    /// List all agent rows belonging to `session_id` via a lock-free WAL snapshot.
    pub(crate) fn list_agents(&self, session_id: &str) -> Result<Vec<StoredAgentRow>, StoreError> {
        canonical_uuid(session_id)?;
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {SELECT_AGENT_COLS}
             FROM agents
             WHERE session_id = ?1
             ORDER BY created_at_ms ASC, agent_id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![session_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(decode_agent_row(row)?);
        }
        Ok(out)
    }

    /// Read one agent row via a lock-free WAL snapshot.
    pub(crate) fn read_agent_row(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> Result<Option<StoredAgentRow>, StoreError> {
        canonical_uuid(session_id)?;
        canonical_uuid(agent_id)?;
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            return Ok(None);
        };
        read_agent_with_conn(&conn, session_id, agent_id)
    }

    /// Read one operation row via a lock-free WAL snapshot.
    pub(crate) fn read_operation(
        &self,
        session_id: &str,
        operation_id: &str,
    ) -> Result<Option<StoredOperationRow>, StoreError> {
        canonical_uuid(session_id)?;
        canonical_uuid(operation_id)?;
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            return Ok(None);
        };
        read_operation_with_conn(&conn, session_id, operation_id)
    }

    /// List all operations belonging to `session_id` via a lock-free WAL snapshot.
    pub(crate) fn list_operations(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredOperationRow>, StoreError> {
        canonical_uuid(session_id)?;
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            return Ok(Vec::new());
        };
        let sql = format!(
            "SELECT {SELECT_OPERATION_COLS}
             FROM operations
             WHERE session_id = ?1
             ORDER BY created_at_ms ASC, operation_id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query(params![session_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(decode_operation_row(row)?);
        }
        Ok(out)
    }

    /// Read one operation's metadata and a UTF-8 safe output slice via a lock-free WAL snapshot.
    pub(crate) fn read_agent_output_page(
        &self,
        session_id: &str,
        operation_id: &str,
        offset_bytes: usize,
        limit_bytes: usize,
    ) -> Result<Option<ReadAgentPage>, StoreError> {
        let Some(operation) = self.read_operation(session_id, operation_id)? else {
            return Ok(None);
        };
        let text = operation.result_text.as_deref().unwrap_or("");
        let page = slice_utf8_page(text, offset_bytes, limit_bytes);
        Ok(Some(ReadAgentPage { operation, page }))
    }

    /// List pending `agent_events` targeting `target_agent_id` via a lock-free WAL snapshot.
    pub(crate) fn list_pending_inbox_events(
        &self,
        session_id: &str,
        target_agent_id: &str,
    ) -> Result<Vec<InboxEvent>, StoreError> {
        canonical_uuid(session_id)?;
        canonical_uuid(target_agent_id)?;
        let Some(conn) = schema::open_read_only(&self.db_path)? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT
                event_id, session_id, target_agent_id, priority, source_kind,
                source_id, coalesce_key, wake_policy, payload_json, state,
                claimed_op_id, created_at_ms, delivered_at_ms
             FROM agent_events
             WHERE session_id = ?1
               AND target_agent_id = ?2
               AND state = 'pending'
             ORDER BY event_id ASC",
        )?;
        let mut rows = stmt.query(params![session_id, target_agent_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(decode_inbox_event_row(row)?);
        }
        Ok(out)
    }
}
